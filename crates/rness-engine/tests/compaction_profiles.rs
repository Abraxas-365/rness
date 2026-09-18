//! Summary profile routing is shared by automatic, region, and legacy compaction.
use async_trait::async_trait;
use rness_engine::{
    config::ModelRegistry,
    service::SessionService,
    session::branch::SessionStore,
    tools::ToolRegistry,
    turn::{
        compaction::Policy,
        provider::{Provider, StepOutcome, StepRequest},
        TurnConfig,
    },
};
use rness_kernel::EventBus;
use rness_protocol::events::*;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

struct Capture {
    name: String,
    seen: Arc<Mutex<Vec<(String, CallConfig)>>>,
    limit: bool,
}
#[async_trait]
impl Provider for Capture {
    fn model(&self) -> &str {
        &self.name
    }
    fn supports_max_output_tokens(&self) -> bool {
        self.limit
    }
    async fn step(&self, request: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        self.seen
            .lock()
            .unwrap()
            .push((self.name.clone(), request.context.config.clone()));
        if request.system == "Summarize" {
            assert!(request.tools.is_empty());
        }
        StepOutcome::Committed(AssistantMessage {
            model: self.name.clone(),
            content: vec![ContentPart::Text {
                text: "brief".into(),
            }],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            estimated_input: 0,
            chunks: vec![],
        })
    }
}
fn policy(profile: Option<&str>) -> Policy {
    serde_json::from_value(serde_json::json!({"summary_profile":profile,"system_prompt":"Summarize","prompt":"Preserve context",
        "threshold_tokens":5000,"retain_tokens":1,"summary_tokens":2048,"max_overflow_retries":1,"max_compactions":1,
        "prune_threshold":8192,"prune_head":4096,"prune_tail":1024})).unwrap()
}
fn current() -> CallConfig {
    CallConfig {
        selection: Some(ModelSelection {
            route: "main-route".into(),
            model: "main".into(),
        }),
        reasoning: Some(Reasoning::Effort {
            effort: "high".into(),
        }),
        temperature: Some(0.7),
        max_output_tokens: Some(3000),
        ..Default::default()
    }
}
#[tokio::test]
async fn summary_profiles_isolate_options_on_every_compaction_path() {
    for mode in ["automatic", "region", "legacy"] {
        for variant in ["fixed", "relative", "defaults", "inherit", "no-limit"] {
            let dir = tempfile::tempdir().unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut registry = ModelRegistry::default();
            let options = serde_json::json!({"reasoning":{"kind":"effort","effort":"low"},"temperature":0.2,"max_output_tokens":4096});
            let profile = match variant {
                "relative" => {
                    serde_json::json!({"by_provider":{"main-route":{"model":"summary","options":options}}})
                }
                "defaults" => serde_json::json!({"provider":"summary-route","model":"summary"}),
                _ => {
                    serde_json::json!({"provider":"summary-route","model":"summary","options":options})
                }
            };
            registry
                .declare_profile("compact".into(), serde_json::from_value(profile).unwrap())
                .unwrap();
            let policy = policy((variant != "inherit").then_some("compact"));
            let main = Arc::new(Capture {
                name: "main".into(),
                seen: seen.clone(),
                limit: true,
            });
            let capture = seen.clone();
            let limit = variant != "no-limit";
            let svc = SessionService::new(
                SessionStore::new(dir.path()),
                main,
                Arc::new(ToolRegistry::default()),
                TurnConfig {
                    compaction: [("default".into(), policy.clone())].into(),
                    ..Default::default()
                },
                Arc::new(EventBus::default()),
            )
            .with_agents(Default::default(), registry)
            .with_provider_resolver(
                current(),
                Arc::new(move |s| {
                    Ok(Arc::new(Capture {
                        name: s.model.clone(),
                        seen: capture.clone(),
                        limit,
                    }))
                }),
            );
            let id = svc.create(None).unwrap();
            let mut log = svc.store().open(&id).unwrap();
            log.append(&SessionEvent::UserMessage(UserMessage {
                intent: UserIntent::Followup,
                content: vec![ContentPart::Text {
                    text: "source context ".repeat(2000),
                }],
                source: None,
            }))
            .unwrap();
            log.append(&SessionEvent::TurnEnded {
                turn: 1,
                outcome: TurnOutcome::Completed,
            })
            .unwrap();
            drop(log);
            match mode {
                "automatic" => {
                    svc.send(
                        &id,
                        UserIntent::Followup,
                        vec![ContentPart::Text {
                            text: "continue".into(),
                        }],
                    )
                    .unwrap();
                    svc.join(&id).await;
                }
                "region" => {
                    let ctx = svc.replay(&id).unwrap().context;
                    assert!(svc
                        .compact_region(&id, 0, ctx.turns.len(), ctx.sources, policy)
                        .await
                        .unwrap());
                }
                _ => {
                    svc.compact(&id, 0).await.unwrap();
                }
            }
            let calls = seen.lock().unwrap();
            let (model, summary) = &calls[0];
            assert_eq!(
                model,
                if variant == "inherit" {
                    "main"
                } else {
                    "summary"
                },
                "{mode}/{variant}"
            );
            assert_eq!(summary.max_output_tokens, limit.then_some(2048));
            if variant == "inherit" {
                assert_eq!(summary.reasoning, current().reasoning);
                assert_eq!(summary.temperature, Some(0.7));
            } else {
                assert_eq!(summary.profile.as_deref(), Some("compact"));
                assert_eq!(
                    summary.selection.as_ref().unwrap().route,
                    if variant == "relative" {
                        "main-route"
                    } else {
                        "summary-route"
                    }
                );
                assert_eq!(
                    summary.temperature,
                    if variant == "defaults" {
                        None
                    } else {
                        Some(0.2)
                    }
                );
                assert_eq!(
                    summary.reasoning,
                    if variant == "defaults" {
                        None
                    } else {
                        Some(Reasoning::Effort {
                            effort: "low".into(),
                        })
                    }
                );
            }
            if mode == "automatic" {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[1], ("main".into(), current()));
            }
            assert_eq!(svc.config(&id).unwrap(), current());
            assert!(svc
                .store()
                .history(&id)
                .unwrap()
                .iter()
                .any(|e| matches!(&e.event,SessionEvent::Compaction(c) if c.model==*model)));
        }
    }
}

#[tokio::test]
async fn invalid_summary_profiles_fail_before_requests_or_history_changes() {
    for bad in [
        "unknown",
        "missing-variant",
        "unavailable-route",
        "budget",
        "capability",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ModelRegistry::default();
        if bad != "unknown" {
            let profile = match bad {
                "missing-variant" => {
                    serde_json::json!({"by_provider":{"other":{"model":"summary"}}})
                }
                "budget" => {
                    serde_json::json!({"provider":"summary-route","model":"summary","options":{"reasoning":{"kind":"budget_tokens","tokens":4096},"max_output_tokens":8192}})
                }
                _ => serde_json::json!({"provider":"summary-route","model":"summary"}),
            };
            registry
                .declare_profile("compact".into(), serde_json::from_value(profile).unwrap())
                .unwrap();
        }
        if bad == "capability" {
            registry
                .declare_model(rness_engine::config::ModelDeclaration {
                    provider: "summary-route".into(),
                    model: "summary".into(),
                    capabilities: rness_engine::config::ModelCapabilities {
                        max_output_tokens: Some(1000),
                        ..Default::default()
                    },
                })
                .unwrap();
        }
        let main = Arc::new(Capture {
            name: "main".into(),
            seen: seen.clone(),
            limit: true,
        });
        let capture = seen.clone();
        let policy = policy(Some("compact"));
        let svc = SessionService::new(
            SessionStore::new(dir.path()),
            main,
            Arc::new(ToolRegistry::default()),
            TurnConfig {
                compaction: [("default".into(), policy.clone())].into(),
                ..Default::default()
            },
            Arc::new(EventBus::default()),
        )
        .with_agents(Default::default(), registry)
        .with_provider_resolver(
            current(),
            Arc::new(move |s| {
                if bad == "unavailable-route" && s.route == "summary-route" {
                    return Err("unavailable summary route".into());
                }
                Ok(Arc::new(Capture {
                    name: s.model.clone(),
                    seen: capture.clone(),
                    limit: true,
                }))
            }),
        );
        let id = svc.create(None).unwrap();
        let history = svc.store().history(&id).unwrap();
        let error = svc
            .send(
                &id,
                UserIntent::Followup,
                vec![ContentPart::Text {
                    text: "hello".into(),
                }],
            )
            .unwrap_err();
        let expected = match bad {
            "unknown" => "unknown profile",
            "missing-variant" => "no variant",
            "unavailable-route" => "unavailable summary route",
            "budget" => "less than summary_tokens",
            _ => "exceeds declared limit",
        };
        assert!(error.to_string().contains(expected), "{bad}: {error}");
        let context = svc.replay(&id).unwrap().context;
        let error = svc
            .compact_region(&id, 0, context.turns.len(), context.sources, policy)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{bad}: {error}");
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(svc.store().history(&id).unwrap(), history);
    }
}

#[test]
fn removed_selection_is_rejected_in_all_policy_deserializations() {
    let mut value = serde_json::to_value(policy(None)).unwrap();
    assert!(value.get("summary_selection").is_none());
    for old in [
        serde_json::json!({"route":"a","model":"b"}),
        serde_json::Value::Null,
    ] {
        value["summary_selection"] = old;
        let error = serde_json::from_value::<Policy>(value.clone())
            .unwrap_err()
            .to_string();
        assert!(error.contains("summary_selection was removed"), "{error}");
    }
}

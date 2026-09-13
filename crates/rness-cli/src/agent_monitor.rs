//! Host-side read bridge for the read-only agent drawer. Rendering never reads
//! logs or waits for Lua; child cards use the same renderers, with isolated caches.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rness_engine::presentation::ToolCards;
use rness_engine::service::SessionService;
use rness_engine::subagent::SubagentRuntime;
use rness_lua::plugin_host::LuaHost;
use rness_protocol::api::History;
use rness_protocol::branch::DelegationMode;
use rness_protocol::events::{ContentPart, Envelope, SessionEvent, TurnOutcome};
use rness_tui::modules::agents::{AgentInfo, AgentMonitorState};
use serde_json::{Value, json};

struct ChildRead {
    info: AgentInfo,
    after: Option<String>,
    turn_started: Option<u64>,
    calls: HashMap<String, (String, Value)>,
}

/// Hide inherited fork context, but retain the child's own configuration and
/// assignment. A missing boundary fails closed rather than exposing parent text.
fn own_history(mut events: Vec<Envelope>, fork_at: Option<&str>) -> Vec<Envelope> {
    let events = if let Some(at) = fork_at {
        let start = events
            .iter()
            .position(|event| event.id == at)
            .map(|i| i + 1);
        start
            .map(|start| events.split_off(start))
            .unwrap_or_default()
    } else {
        events
    };
    visible_events(events)
}

fn visible_events(events: Vec<Envelope>) -> Vec<Envelope> {
    events
        .into_iter()
        .filter(|event| {
            !matches!(&event.event,
        SessionEvent::UserMessage(message) if matches!(message.source,
            Some(rness_protocol::events::MessageSource::Instructions { .. })))
        })
        .collect()
}

fn project(child: &mut ChildRead, events: &[Envelope], running: bool) {
    for event in events {
        match &event.event {
            SessionEvent::RequestConfig(config) => {
                if let Some(agent) = &config.agent {
                    child.info.name = agent.name.clone();
                }
            }
            SessionEvent::UserMessage(message)
                if child.info.task.is_empty() && message.source.is_none() =>
            {
                child.info.task = message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            SessionEvent::TurnStarted { .. } => {
                child.info.status = "running".into();
                child.info.elapsed_ms = 0;
                child.turn_started = event
                    .id
                    .parse::<ulid::Ulid>()
                    .ok()
                    .map(|id| id.timestamp_ms());
            }
            SessionEvent::TurnEnded { outcome, .. } => {
                child.info.status = match outcome {
                    TurnOutcome::Completed if child.info.mode == "continuable" => "idle",
                    TurnOutcome::Completed => "finished",
                    TurnOutcome::Cancelled => "interrupted",
                    _ => "error",
                }
                .into();
                child.info.elapsed_ms = child
                    .turn_started
                    .zip(event.id.parse::<ulid::Ulid>().ok())
                    .map(|(start, end)| end.timestamp_ms().saturating_sub(start))
                    .unwrap_or(0);
                child.turn_started = None;
            }
            SessionEvent::AssistantMessage(message) => {
                for part in &message.content {
                    if let ContentPart::ToolUse { call, name, args } = part {
                        child
                            .calls
                            .insert(call.clone(), (name.clone(), args.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(last) = events.last() {
        child.after = Some(last.id.clone());
    }
    if running {
        child.info.status = "running".into();
    } else if child.info.status == "running" {
        // A historical open turn is not evidence of an active process.
        child.info.status = "interrupted".into();
        child.turn_started = None;
    }
    if let Some(started) = child.turn_started {
        child.info.elapsed_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_sub(started as u128) as u64;
    }
}

async fn render_results(
    lua: &impl ToolCards,
    state: &AgentMonitorState,
    child: &ChildRead,
    events: &[Envelope],
) {
    let cache = state.cards(&child.info.session);
    let generation = cache.generation();
    for event in events {
        if let SessionEvent::ToolResult(result) = &event.event {
            let args = child
                .calls
                .get(&result.call)
                .map(|(_, args)| args.clone())
                .unwrap_or(Value::Null);
            if let Some(lines) = lua.tool_card_result(args, result).await {
                cache.insert_if_current(generation, result.call.clone(), lines);
            } else {
                cache.remove_if_current(generation, &result.call);
            }
        }
    }
}

/// Runs only while the drawer is open. Per-child cursors avoid repeatedly
/// reconstructing histories; discovery is throttled independently of live refresh.
pub fn spawn(
    sessions: Arc<SessionService>,
    subagents: Arc<SubagentRuntime>,
    lua: LuaHost,
    state: AgentMonitorState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut root = String::new();
        let mut readers: HashMap<String, ChildRead> = HashMap::new();
        let mut order = Vec::new();
        let mut discovered = Instant::now() - Duration::from_secs(2);
        let mut shown: Option<String> = None;
        let mut shown_generation = 0;
        let mut live_published = HashMap::<String, Value>::new();
        let mut recovery = crate::activity_recovery::ActivityRecovery::default();
        loop {
            tick.tick().await;
            let Some((requested_root, selected)) = state.requested() else {
                shown = None;
                continue;
            };
            if root != requested_root {
                root = requested_root;
                readers.clear();
                order.clear();
                shown = None;
                discovered = Instant::now() - Duration::from_secs(2);
            }
            if discovered.elapsed() >= Duration::from_secs(1) {
                match subagents.list_agents(&root) {
                    Ok(agents) => {
                        order = agents.iter().map(|agent| agent.session.clone()).collect();
                        for agent in agents {
                            if readers.contains_key(&agent.session) {
                                continue;
                            }
                            let Ok(Some(link)) = sessions.store().delegation(&agent.session) else {
                                continue;
                            };
                            let info = AgentInfo {
                                alias: agent.alias.unwrap_or_else(|| agent.session.clone()),
                                session: agent.session.clone(),
                                parent: Some(agent.parent),
                                depth: agent.depth as usize,
                                name: "subagent".into(),
                                task: String::new(),
                                mode: if link.mode == DelegationMode::Continuable {
                                    "continuable"
                                } else {
                                    "one-shot"
                                }
                                .into(),
                                status: "unknown".into(),
                                elapsed_ms: 0,
                                call: link.call,
                            };
                            readers.insert(
                                agent.session,
                                ChildRead {
                                    info,
                                    after: None,
                                    turn_started: None,
                                    calls: HashMap::new(),
                                },
                            );
                        }
                        discovered = Instant::now();
                    }
                    Err(error) => {
                        tracing::warn!(%error, "agent discovery failed");
                        continue;
                    }
                }
            }
            // Only descendant IDs from the authoritative list may be hydrated.
            let selected = selected.filter(|id| order.contains(id));
            let pass_generation = selected
                .as_ref()
                .map(|id| state.cards(id).generation())
                .unwrap_or(0);
            let changed =
                selected != shown || (selected.is_some() && pass_generation != shown_generation);
            if changed {
                live_published.clear();
            }
            for id in &order {
                let Some(child) = readers.get_mut(id) else {
                    continue;
                };
                let full = child.after.is_none() || (changed && selected.as_ref() == Some(id));
                let events = if full {
                    let Ok(events) = sessions.store().history(id) else {
                        continue;
                    };
                    let fork_at = sessions
                        .store()
                        .parent(id)
                        .ok()
                        .flatten()
                        .map(|parent| parent.at);
                    own_history(events, fork_at.as_deref())
                } else {
                    match sessions
                        .store()
                        .history_after(id, child.after.as_deref().unwrap_or_default())
                    {
                        Ok(Some(events)) => events,
                        _ => {
                            child.after = None;
                            continue;
                        }
                    }
                };
                project(
                    child,
                    &events,
                    sessions.phase(id) == rness_engine::inbox::Phase::Running,
                );
                if selected.as_ref() == Some(id) {
                    if full {
                        state.publish_history(
                            id,
                            History {
                                session: id.clone(),
                                envelopes: events.clone(),
                            },
                        );
                    } else if !events.is_empty() {
                        state.append_history(id, visible_events(events.clone()));
                    }
                    render_results(&lua, &state, child, &events).await;
                }
            }
            // Existing bounded activity supplies live tool streams and accurate
            // duration. Never overwrite a committed result with a live card.
            if let Some(id) = &selected {
                if let Some(child) = readers.get_mut(id) {
                    let parent = child.info.parent.as_deref().unwrap_or(&root);
                    recovery.recover(&subagents.activity, &sessions, parent);
                    for (_, args, activity) in subagents.activity.snapshots(&sessions, parent) {
                        if activity["session"].as_str() != Some(id) {
                            continue;
                        }
                        child.info.elapsed_ms = activity["elapsed_ms"]
                            .as_u64()
                            .unwrap_or(child.info.elapsed_ms);
                        if child.info.mode == "one-shot" {
                            child.info.mode = if args["run_in_background"] == true {
                                "background"
                            } else {
                                "foreground"
                            }
                            .into();
                        }
                        let cache = state.cards(id);
                        let generation = cache.generation();
                        let mut active = HashSet::new();
                        for tool in activity["tools"].as_array().into_iter().flatten() {
                            if tool["status"] != "running" || child.info.status != "running" {
                                continue;
                            }
                            let Some(call) = tool["call"].as_str() else {
                                continue;
                            };
                            active.insert(call.to_owned());
                            if live_published.get(call) == Some(tool) {
                                continue;
                            }
                            let name = tool["name"].as_str().unwrap_or("tool");
                            let output = tool["stream"].as_str().unwrap_or_default();
                            if let Some(lines) = lua
                                .tool_card_presented(
                                    name,
                                    tool["args"].clone(),
                                    output,
                                    false,
                                    Some(json!({"kind":"tool_live","status":"running"})),
                                )
                                .await
                            {
                                cache.insert_if_current(generation, call.to_owned(), lines);
                                live_published.insert(call.to_owned(), tool.clone());
                            }
                        }
                        live_published
                            .retain(|call, _| call.starts_with("agent:") || active.contains(call));
                    }
                }
                // Delegations made by the inspected child use the same compact
                // lifecycle renderer as delegations in the principal view.
                recovery.recover(&subagents.activity, &sessions, id);
                let cache = state.cards(id);
                let generation = cache.generation();
                for (call, args, mut activity) in subagents.activity.snapshots(&sessions, id) {
                    let ms = activity["elapsed_ms"].as_u64().unwrap_or(0);
                    activity["elapsed_ms"] = json!(ms / 1000 * 1000);
                    let key = format!("agent:{call}");
                    if live_published.get(&key) == Some(&activity) {
                        continue;
                    }
                    if let Some(lines) = lua
                        .tool_card_presented(
                            "subagent",
                            args,
                            "",
                            activity["status"] == "error",
                            Some(activity.clone()),
                        )
                        .await
                    {
                        cache.insert_if_current(generation, call, lines);
                        live_published.insert(key, activity);
                    }
                }
            }
            state.publish_agents(
                &root,
                order
                    .iter()
                    .filter_map(|id| readers.get(id).map(|child| child.info.clone()))
                    .collect(),
            );
            shown = selected;
            // Record the generation we began hydrating, not a generation
            // invalidated while awaiting Lua. A changed epoch retries next tick.
            shown_generation = pass_generation;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(mode: &str) -> ChildRead {
        ChildRead {
            info: AgentInfo {
                alias: "a1".into(),
                session: "child".into(),
                parent: Some("root".into()),
                depth: 1,
                name: "reviewer".into(),
                task: String::new(),
                mode: mode.into(),
                status: "unknown".into(),
                elapsed_ms: 0,
                call: None,
            },
            after: None,
            turn_started: None,
            calls: HashMap::new(),
        }
    }

    fn event(ms: u64, event: SessionEvent) -> Envelope {
        Envelope {
            id: ulid::Ulid::from_parts(ms, 0).to_string(),
            at: String::new(),
            event,
        }
    }

    #[test]
    fn lifecycle_distinguishes_idle_finished_and_interrupted_and_resets_duration() {
        let mut child = child("continuable");
        project(
            &mut child,
            &[
                event(1000, SessionEvent::TurnStarted { turn: 1 }),
                event(
                    3500,
                    SessionEvent::TurnEnded {
                        turn: 1,
                        outcome: TurnOutcome::Completed,
                    },
                ),
            ],
            false,
        );
        assert_eq!(child.info.status, "idle");
        assert_eq!(child.info.elapsed_ms, 2500);
        project(
            &mut child,
            &[
                event(10000, SessionEvent::TurnStarted { turn: 2 }),
                event(
                    10020,
                    SessionEvent::TurnEnded {
                        turn: 2,
                        outcome: TurnOutcome::Completed,
                    },
                ),
            ],
            false,
        );
        assert_eq!(child.info.elapsed_ms, 20);
        child.info.mode = "background".into();
        project(
            &mut child,
            &[event(
                10030,
                SessionEvent::TurnEnded {
                    turn: 2,
                    outcome: TurnOutcome::Completed,
                },
            )],
            false,
        );
        assert_eq!(child.info.status, "finished");
        project(
            &mut child,
            &[event(11000, SessionEvent::TurnStarted { turn: 3 })],
            false,
        );
        assert_eq!(child.info.status, "interrupted");
    }

    struct Declines;
    #[async_trait::async_trait]
    impl ToolCards for Declines {
        async fn tool_card(
            &self,
            _: &str,
            _: Value,
            _: &str,
            _: bool,
        ) -> Option<Vec<rness_kernel::presentation::StyledLine>> {
            None
        }
    }

    fn result() -> Envelope {
        event(
            1000,
            SessionEvent::ToolResult(
                serde_json::from_value(json!({
                    "call":"call", "name":"Bash", "output":"complete", "duration_ms":10
                }))
                .unwrap(),
            ),
        )
    }

    #[tokio::test]
    async fn declined_completed_renderer_removes_stale_live_card() {
        let state = AgentMonitorState::default();
        let cache = state.cards("child");
        cache.insert("call".into(), vec![]);
        let revision = cache.revision();
        render_results(&Declines, &state, &child("foreground"), &[result()]).await;
        assert!(!cache.contains(&"call".into()));
        assert!(cache.revision() > revision);
    }

    struct InvalidateOnce {
        cache: rness_tui::modules::tool_cards::CardCache,
        first: std::sync::atomic::AtomicBool,
    }
    #[async_trait::async_trait]
    impl ToolCards for InvalidateOnce {
        async fn tool_card(
            &self,
            _: &str,
            _: Value,
            _: &str,
            _: bool,
        ) -> Option<Vec<rness_kernel::presentation::StyledLine>> {
            if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.cache.invalidate();
            }
            Some(vec![])
        }
    }

    #[tokio::test]
    async fn invalidation_during_render_requires_generation_retry() {
        let state = AgentMonitorState::default();
        let cache = state.cards("child");
        let renderer = InvalidateOnce {
            cache: cache.clone(),
            first: true.into(),
        };
        let pass_generation = cache.generation();
        render_results(&renderer, &state, &child("foreground"), &[result()]).await;
        assert!(!cache.contains(&"call".into()));
        assert_ne!(cache.generation(), pass_generation);
        render_results(&renderer, &state, &child("foreground"), &[result()]).await;
        assert!(cache.contains(&"call".into()));
    }

    #[test]
    fn fork_context_is_excluded_and_missing_boundary_fails_closed() {
        let event = |id: &str| Envelope {
            id: id.into(),
            at: String::new(),
            event: SessionEvent::TurnStarted { turn: 1 },
        };
        let events = vec![event("inherited"), event("boundary"), event("child")];
        assert_eq!(
            own_history(events.clone(), Some("boundary"))
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["child"]
        );
        assert!(own_history(events.clone(), Some("missing")).is_empty());
        assert_eq!(own_history(events, None).len(), 3);
    }
}

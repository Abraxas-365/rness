//! User steering is descendant-scoped and never impersonates the parent agent.
use async_trait::async_trait;
use rness_engine::{
    service::SessionService,
    session::branch::SessionStore,
    subagent::{SubagentError, SubagentRuntime},
    tools::ToolRegistry,
    turn::{
        provider::{Provider, StepOutcome, StepRequest},
        TurnConfig,
    },
};
use rness_kernel::EventBus;
use rness_protocol::{
    branch::{Delegation, DelegationMode},
    events::*,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct Done;
#[async_trait]
impl Provider for Done {
    fn model(&self) -> &str {
        "test"
    }
    async fn step(&self, _: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        StepOutcome::Committed(AssistantMessage {
            model: "test".into(),
            content: vec![ContentPart::Text {
                text: "done".into(),
            }],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            estimated_input: 0,
            chunks: vec![],
        })
    }
}
fn messages(sessions: &SessionService, id: &SessionId) -> Vec<UserMessage> {
    sessions
        .store()
        .history(id)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::UserMessage(m) => Some(m),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn user_steering_records_origin_notifies_caller_and_preserves_agent_authority() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(Done),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let main = sessions.create(None).unwrap();
    let other = sessions.create(None).unwrap();
    let child = sessions
        .create_delegated(
            None,
            Delegation {
                parent: main.clone(),
                depth: 1,
                mode: DelegationMode::OneShot,
                call: None,
            },
        )
        .unwrap();
    let grandchild = sessions
        .create_delegated(
            None,
            Delegation {
                parent: child.clone(),
                depth: 2,
                mode: DelegationMode::Continuable,
                call: None,
            },
        )
        .unwrap();
    // Ancestry remains valid even if the configured creation limit is reduced.
    let runtime = SubagentRuntime::new(sessions.clone(), 0);
    assert!(matches!(
        runtime.steer_user(&main, &child, "no".into()),
        Err(SubagentError::NotAuthorized(_))
    ));
    assert!(messages(&sessions, &child).is_empty());
    for (caller, target) in [(&main, &other), (&grandchild, &main), (&child, &child)] {
        assert!(matches!(
            runtime.steer_user(caller, target, "no".into()),
            Err(SubagentError::NotAuthorized(_))
        ));
    }
    assert!(runtime
        .steer_user(&main, &grandchild, " \n ".into())
        .is_err());
    assert!(runtime
        .send_message(&main, &grandchild, "no".into())
        .is_err());
    assert!(messages(&sessions, &grandchild).is_empty());
    runtime
        .steer_user(&main, &grandchild, "Do this instead".into())
        .unwrap();
    sessions.join(&grandchild).await;
    let user = messages(&sessions, &grandchild);
    assert_eq!(user.len(), 1);
    // The existing inbox normalizes idle steering to a followup activation.
    assert_eq!(user[0].intent, UserIntent::Followup);
    assert_eq!(user[0].source, None); // Existing plain-user provenance.
    assert_eq!(
        user[0].content,
        vec![ContentPart::Text {
            text: format!("User steering from conversation {main}:\nDo this instead"),
        }]
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if messages(&sessions, &main).iter().any(|m| {
                m.content
                    == vec![ContentPart::Text {
                        text: format!(
                        "User intervened in subagent {grandchild} with steering:\nDo this instead"
                    ),
                    }]
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    sessions.join(&main).await;
    assert!(sessions
        .store()
        .history(&main)
        .unwrap()
        .iter()
        .all(|e| !matches!(e.event, SessionEvent::TurnStarted { .. })));
    assert!(messages(&sessions, &main)
        .iter()
        .all(|m| m.intent == UserIntent::Inject));
    // Reopening the store retains the distinct text, not a transient UI label.
    assert!(SessionStore::new(dir.path())
        .history(&grandchild)
        .unwrap()
        .iter()
        .any(|e| matches!(&e.event, SessionEvent::UserMessage(m) if m == &user[0])));
    assert!(messages(&sessions, &other).is_empty());
}

#[test]
fn user_alias_order_appends_new_nested_children_without_changing_model_tree_order() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(Done),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let main = sessions.create(None).unwrap();
    let child = |parent: &SessionId, depth| {
        // ULID timestamps establish creation order independently of random bits.
        std::thread::sleep(std::time::Duration::from_millis(2));
        sessions
            .create_delegated(
                None,
                Delegation {
                    parent: parent.clone(),
                    depth,
                    mode: DelegationMode::Continuable,
                    call: None,
                },
            )
            .unwrap()
    };
    let first = child(&main, 1);
    let second = child(&main, 1);
    let second_nested = child(&second, 2);
    let runtime = SubagentRuntime::new(sessions.clone(), 3);
    let before: Vec<_> = runtime
        .list_agents(&main)
        .unwrap()
        .into_iter()
        .map(|c| c.session)
        .collect();
    assert_eq!(
        before,
        vec![first.clone(), second.clone(), second_nested.clone()]
    );
    // Adding a descendant below the earlier sibling must not renumber a3.
    let first_nested = child(&first, 2);
    let after: Vec<_> = runtime
        .list_agents(&main)
        .unwrap()
        .into_iter()
        .map(|c| c.session)
        .collect();
    assert_eq!(&after[..before.len()], before.as_slice());
    assert_eq!(after.last(), Some(&first_nested));
    let model_tree: Vec<_> = runtime
        .list_children(&main, true)
        .unwrap()
        .into_iter()
        .map(|c| c.session)
        .collect();
    assert_eq!(model_tree, vec![first, second, first_nested, second_nested]);
}

#[test]
fn user_aliases_keep_tombstones_and_do_not_retarget_after_older_import() {
    use rness_engine::session::log::SessionLog;
    let dir = tempfile::tempdir().unwrap();
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(Done),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let main = sessions.create(None).unwrap();
    let make = |id: &str| {
        SessionLog::create(
            dir.path(),
            &id.into(),
            None,
            None,
            Some(Delegation {
                parent: main.clone(),
                depth: 1,
                mode: DelegationMode::OneShot,
                call: None,
            }),
        )
        .unwrap();
    };
    let first = "00000000000000000000000002";
    let older = "00000000000000000000000001";
    make(first);
    let runtime = SubagentRuntime::new(sessions, 3);
    assert_eq!(
        runtime.list_agents(&main).unwrap()[0].alias.as_deref(),
        Some("a1")
    );
    make(older);
    let list = runtime.list_agents(&main).unwrap();
    assert_eq!(list[0].session, first);
    assert_eq!(list[1].session, older);
    assert_eq!(list[1].alias.as_deref(), Some("a2"));
    // Move only the disposable test fixture out of the store to simulate deletion.
    let removed = tempfile::tempdir().unwrap();
    std::fs::rename(dir.path().join(first), removed.path().join(first)).unwrap();
    let list = runtime.list_agents(&main).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].alias.as_deref(), Some("a2"));
}

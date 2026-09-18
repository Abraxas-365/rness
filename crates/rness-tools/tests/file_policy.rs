use rness_engine::tools::{ToolCall, ToolRegistry};
use rness_protocol::sandbox::SandboxMode;
use rness_tools::{register_all, Workspace};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn file_mutations_obey_session_policy() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let registry = ToolRegistry::default();
    register_all(&registry, Workspace::new(&root_path));
    for (mode, path, denied) in [
        (SandboxMode::ReadOnly, root_path.join("denied"), true),
        (
            SandboxMode::WorkspaceWrite,
            outside.path().join("denied"),
            true,
        ),
        (
            SandboxMode::WorkspaceWrite,
            root_path.join("nested/allowed"),
            false,
        ),
    ] {
        let bound = registry.for_workspace_with_policy(&"s".into(), &root_path, mode);
        let result = bound
            .dispatch(
                &"s".into(),
                &[ToolCall {
                    call: "w".into(),
                    name: "Write".into(),
                    args: json!({"path":path,"content":"hello"}),
                }],
                4,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(result[0].is_error, denied, "{}", result[0].output);
        assert_eq!(path.exists(), !denied);
    }
    let bound = registry.for_workspace_with_policy(&"s".into(), &root_path, SandboxMode::ReadOnly);
    let results = bound
        .dispatch(
            &"s".into(),
            &[
                ToolCall {
                    call: "r".into(),
                    name: "Read".into(),
                    args: json!({"path":"nested/allowed"}),
                },
                ToolCall {
                    call: "e".into(),
                    name: "Edit".into(),
                    args: json!({"path":"nested/allowed","old_string":"hello","new_string":"bad"}),
                },
            ],
            4,
            &CancellationToken::new(),
        )
        .await;
    assert!(!results[0].is_error);
    assert!(results[1].is_error);
    assert_eq!(
        std::fs::read_to_string(root_path.join("nested/allowed")).unwrap(),
        "hello"
    );
}

#[cfg(unix)]
#[test]
fn workspace_policy_rejects_symlink_and_hardlink_escapes() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret");
    std::fs::write(&secret, "secret").unwrap();
    let policy = rness_tools::sandbox::Policy::new(SandboxMode::WorkspaceWrite, root.path());
    std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
    assert!(policy.check_write(&root.path().join("link/new")).is_err());
    std::fs::hard_link(&secret, root.path().join("hard")).unwrap();
    assert!(policy.check_write(&root.path().join("hard")).is_err());
    std::os::unix::fs::symlink(outside.path().join("absent"), root.path().join("dangling"))
        .unwrap();
    assert!(policy.check_write(&root.path().join("dangling")).is_err());
    assert!(policy.check_write(&root.path().join("../escape")).is_err());
}

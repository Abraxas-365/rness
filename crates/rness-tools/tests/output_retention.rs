use rness_engine::tools::Tool;
use rness_tools::{
    bash::BashTool,
    jobs::{JobOutputTool, JobRegistry, JobStatus},
    Workspace,
};
use serde_json::json;

#[tokio::test]
async fn full_background_output_is_paged_and_survives_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = JobRegistry::new();
    jobs.enable_persistence(dir.path()).unwrap();
    let (id, writer) = jobs.start_owned("bash", "large".into(), Some(&"owner".into()));
    writer.append(&vec![b'a'; 200_000]);
    writer.append(b"tail");
    writer.settle(JobStatus::Exited(Some(0)));
    let tool = JobOutputTool::new(jobs.clone());
    assert!(tool
        .execute_in(&"other".into(), json!({"job_id":id,"offset":0}))
        .await
        .is_err());
    let text = tool
        .execute_in(&"owner".into(), json!({"job_id":id}))
        .await
        .unwrap();
    assert!(text.contains("Full output:"));
    assert!(text.contains("tail"));
    assert!(jobs.inspect("owner", &id).unwrap().output.len() <= 8192);
    assert_eq!(jobs.inspect("owner", &id).unwrap().output_bytes, 200_004);
    let page = tool
        .execute_in(&"owner".into(), json!({"job_id":id,"offset":0}))
        .await
        .unwrap();
    assert!(page.starts_with(&"a".repeat(65536)));
    assert!(page.contains("next offset: 65536"));
    drop(tool);
    drop(writer);
    drop(jobs);
    let jobs = JobRegistry::new();
    jobs.enable_persistence(dir.path()).unwrap();
    let page = JobOutputTool::new(jobs)
        .execute_in(&"owner".into(), json!({"job_id":id,"offset":200000}))
        .await
        .unwrap();
    assert!(page.starts_with("tail"));
}

#[tokio::test]
async fn foreground_spool_failure_stops_silent_command_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = JobRegistry::new();
    jobs.enable_persistence(dir.path()).unwrap();
    let bash = BashTool::new(Workspace::new(dir.path()), jobs.clone());
    let task = tokio::spawn(async move {
        bash.execute(json!({"command":"sleep 1; printf output; sleep 30", "description":"Exercise output failure cancellation", "timeout_ms":60000})).await
    });
    let output = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            for directory in std::fs::read_dir(dir.path()).unwrap().flatten() {
                if let Ok(entries) = std::fs::read_dir(directory.path()) {
                    for entry in entries.flatten() {
                        if entry.path().extension().is_some_and(|ext| ext == "output") {
                            return entry.path();
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    std::fs::remove_file(&output).unwrap();
    std::fs::create_dir(&output).unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error.contains("capture") || error.contains("persistence"),
        "{error}"
    );
}

#[tokio::test]
async fn foreground_truncation_retains_retrievable_full_output() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = JobRegistry::new();
    let bash = BashTool::new(Workspace::new(dir.path()), jobs.clone());
    let result = bash.execute_presented(&"owner".into(), &"call".into(), json!({"command":"head -c 200000 /dev/zero | tr '\\000' x", "description":"Generate large test output"}), &tokio_util::sync::CancellationToken::new()).await.unwrap();
    let metadata = result.3.unwrap();
    assert_eq!(metadata["stdout_bytes"], 200000);
    assert_eq!(metadata["truncated"], true);
    let id = metadata["output_artifact"].as_str().unwrap();
    let page = JobOutputTool::new(jobs)
        .execute_in(&"owner".into(), json!({"job_id":id,"offset":0}))
        .await
        .unwrap();
    assert!(page.starts_with(&"x".repeat(65536)));
}

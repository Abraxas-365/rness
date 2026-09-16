use std::{io::Cursor, sync::{Arc, OnceLock}};
use rness_engine::{images::{ImagePolicy, ImageStore}, tools::{Tool, ToolRegistry}};
use rness_tools::read_image::ReadImage;
use rness_protocol::events::ToolResultContentPart;
use serde_json::json;

fn png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::new_rgba8(32, 16).write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
    bytes
}
fn tool(root: &std::path::Path, policy: ImagePolicy) -> (ReadImage, Arc<ImageStore>) {
    let store = Arc::new(ImageStore::new(root.join("images"), policy).unwrap());
    let images = Arc::new(OnceLock::new());
    assert!(images.set(store.clone()).is_ok());
    (ReadImage { images, capability: Arc::new(|_| Ok(())), processing: Arc::new(tokio::sync::Semaphore::new(2)), workspace: Some(root.into()) }, store)
}

#[tokio::test]
async fn image_reader_is_opt_in_and_returns_durable_normalized_content() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::default();
    rness_tools::register_all(&registry, rness_tools::Workspace::new(dir.path()));
    assert!(registry.get("read_image").is_none());
    std::fs::write(dir.path().join("extensionless"), png()).unwrap();
    let (reader, store) = tool(dir.path(), ImagePolicy { max_dimension: 8, ..Default::default() });
    let (parts, _, _) = reader.execute_rich(&"s".into(), "c", json!({"file_path":"extensionless"}), &Default::default()).await.unwrap();
    let ToolResultContentPart::Image { attachment } = &parts[1] else { panic!("expected image"); };
    assert_eq!((attachment.width, attachment.height), (8, 4));
    assert!(store.admitted_for_session("s", &attachment.id));
    assert!(!store.admitted_for_session("other", &attachment.id));
    let registry = Arc::new(ToolRegistry::default());
    registry.images.set(store.clone()).ok().unwrap();
    registry.register(Arc::new(reader));
    let (result, _) = rness_engine::tools::exposure::program(registry, "s".into(),
        rness_engine::tools::ToolCall {call:"ptc".into(), name:"run_code".into(), args:json!({"code":"return tools.call('read_image', {file_path='extensionless'})"})},
        Default::default(), None).await;
    assert!(!result.is_error, "{}", result.output);
    assert!(result.content.iter().any(|part| matches!(part, ToolResultContentPart::Image { .. })));
    let reopened = ImageStore::new(dir.path().join("images"), Default::default()).unwrap();
    assert!(!reopened.read(attachment).unwrap().is_empty());
}

#[tokio::test]
async fn image_reader_refuses_invalid_oversized_and_unavailable_inputs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("image"), png()).unwrap();
    std::fs::write(dir.path().join("bad"), b"not an image").unwrap();
    let (mut reader, _) = tool(dir.path(), Default::default());
    for path in ["bad", "missing", ".", ""] {
        assert!(reader.execute_rich(&"s".into(), "c", json!({"file_path":path}), &Default::default()).await.is_err());
    }
    let cancel = tokio_util::sync::CancellationToken::new(); cancel.cancel();
    assert!(reader.execute_rich(&"s".into(), "c", json!({"file_path":"image"}), &cancel).await.is_err());
    reader.capability = Arc::new(|_| Err("text-only model".into()));
    assert_eq!(reader.execute_rich(&"s".into(), "c", json!({"file_path":"missing"}), &Default::default()).await.unwrap_err(), "text-only model");
    for policy in [ImagePolicy {max_input_bytes: 1, ..Default::default()}, ImagePolicy {max_input_dimension: 10, ..Default::default()}, ImagePolicy {max_bytes: 1, ..Default::default()}] {
        let (reader, _) = tool(dir.path(), policy);
        assert!(reader.execute_rich(&"s".into(), "c", json!({"file_path":"image"}), &Default::default()).await.is_err());
    }
}

#[tokio::test]
async fn workspace_binding_uses_the_callers_directory() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("image"), png()).unwrap();
    let (reader, _) = tool(dir.path(), Default::default());
    let bound = reader.for_workspace(&"s".into(), other.path()).unwrap();
    assert!(bound.execute_rich(&"s".into(), "c", json!({"file_path":"image"}), &Default::default()).await.is_ok());
}

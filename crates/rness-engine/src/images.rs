//! Durable image bytes and deterministic, policy-specific request variants.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::{GenericImageView, ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use rness_protocol::events::ImageRef;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnimationPolicy {
    #[default]
    FirstFrame,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImagePolicy {
    pub animation: AnimationPolicy,
    pub normalize_srgb: bool,
    pub deepseek_files: bool,
    pub anthropic_files: bool,
    pub max_request_images: usize,
    pub max_request_bytes: usize,
    pub max_input_bytes: usize,
    pub max_input_pixels: u64,
    pub max_input_dimension: u32,
    pub max_pixels: u64,
    pub max_dimension: u32,
    pub max_bytes: usize,
    pub lossless: bool,
    pub quality: u8,
}

impl Default for ImagePolicy {
    fn default() -> Self {
        Self { animation: AnimationPolicy::FirstFrame, normalize_srgb: true, deepseek_files: false, anthropic_files: false, max_request_images: 20, max_request_bytes: 20 * 1024 * 1024, max_input_bytes: 20 * 1024 * 1024, max_input_pixels: 40_000_000,
            max_input_dimension: 8192,
            max_pixels: 4_000_000, max_dimension: 4096, max_bytes: 5 * 1024 * 1024,
            lossless: true, quality: 85 }
    }
}

impl ImagePolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_request_images == 0 || self.max_request_bytes == 0 || self.max_input_bytes == 0 || self.max_input_pixels == 0 || self.max_pixels == 0
            || self.max_input_dimension == 0
            || self.max_dimension == 0 || self.max_bytes == 0 || !(1..=100).contains(&self.quality)
        { return Err("image limits must be positive; quality must be 1..100".into()); }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let image = image::DynamicImage::new_rgba8(32, 16);
        let mut data = Vec::new();
        image.write_to(&mut Cursor::new(&mut data), ImageFormat::Png).unwrap();
        data
    }

    #[test]
    fn tool_image_normalization_is_not_repeated_by_provider_projection() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path().into(), ImagePolicy { lossless: false, max_dimension: 8, ..Default::default() }).unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        store.set_processor(Some(("once".into(), std::sync::Arc::new(move |_, bytes| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(bytes.to_vec())
        })))).unwrap();
        let (reference, original) = store.admit_tool_image("s", &png()).unwrap();
        assert_eq!(original, (32, 16));
        assert_eq!((reference.width, reference.height), (8, 4));
        let (_, projected) = store.request_image(&reference, store.policy()).unwrap();
        assert_eq!(projected, store.read(&reference).unwrap());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn ownership_survives_reopen_and_cleanup_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let scope = digest(b"account");
        let store = ImageStore::new(dir.path().into(), Default::default()).unwrap();
        for i in 0..20 {
            let id = format!("file_{i:02}");
            store.record_owned_upload(&scope, &id, u64::MAX - i).unwrap();
            let file = std::fs::File::options().write(true).open(store.root.join("owned-uploads").join(&scope).join(id)).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(i + 1))).unwrap();
        }
        store.record_owned_upload(&scope, "active", 1000).unwrap();
        assert!(store.record_owned_upload(&scope, "../escape", 0).is_err());
        assert!(store.forget_owned_upload("../escape", "file").is_err());
        drop(store);
        let store = ImageStore::new(dir.path().into(), Default::default()).unwrap();
        let candidates = store.oldest_owned_uploads(&scope, &["active".into()]).unwrap();
        assert_eq!(candidates.len(), 16);
        assert_eq!(candidates[0], "file_00");
        assert!(!candidates.iter().any(|id| id == "active"));
        store.forget_owned_upload(&scope, "file_00").unwrap();
        store.forget_owned_upload(&scope, "file_00").unwrap();
        assert_eq!(store.oldest_owned_uploads(&scope, &["active".into()]).unwrap()[0], "file_01");
    }

    #[test]
    fn upload_cache_survives_reopen_and_checks_expiration() {
        let dir = tempfile::tempdir().unwrap();
        let key = digest(b"endpoint credential image");
        let store = ImageStore::new(dir.path().into(), Default::default()).unwrap();
        store.cache_upload(&key, "file_test", 500).unwrap();
        drop(store);
        let store = ImageStore::new(dir.path().into(), Default::default()).unwrap();
        assert_eq!(store.cached_upload(&key, 100).unwrap(), Some(("file_test".into(),500)));
        assert_eq!(store.cached_upload(&key, 440).unwrap(), None);
        assert!(store.cached_upload("../escape", 0).is_err());
    }

    #[test]
    fn custom_processor_is_cached_versioned_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path().into(), Default::default()).unwrap();
        let reference = store.admit(&png(), "image/png").unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        store.set_processor(Some(("v1".into(), std::sync::Arc::new(move |_, bytes| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(bytes.to_vec())
        })))).unwrap();
        for _ in 0..2 { store.request_image(&reference, store.policy()).unwrap(); }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        store.set_processor(Some(("v2".into(), std::sync::Arc::new(|_, _| Ok(b"invalid".to_vec()))))).unwrap();
        assert!(store.request_image(&reference, store.policy()).is_err());
        assert_eq!(store.read(&reference).unwrap(), png());
    }

    #[test]
    fn lossy_alpha_uses_webp_and_opaque_rgba_uses_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path().into(), Default::default()).unwrap();
        let policy = ImagePolicy { lossless:false, ..Default::default() };
        for (alpha, mime) in [(127,"image/webp"), (255,"image/jpeg")] {
            let mut data = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4,4,image::Rgba([100,50,30,alpha]))).write_to(&mut data, ImageFormat::Png).unwrap();
            let reference = store.admit(data.get_ref(), "image/png").unwrap();
            let (actual, encoded) = store.request_image(&reference, &policy).unwrap();
            assert_eq!(actual, mime);
            assert_eq!(image::load_from_memory(&encoded).unwrap().to_rgba8().get_pixel(0,0)[3], alpha);
        }
    }

    #[test]
    fn icc_conversion_preserves_alpha_and_removes_profile() {
        use image::{ImageEncoder, ImageDecoder};
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path().into(), ImagePolicy::default()).unwrap();
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder.set_icc_profile(moxcms::ColorProfile::new_display_p3().encode().unwrap()).unwrap();
        encoder.write_image(&[200,100,50,127], 1, 1, image::ExtendedColorType::Rgba8).unwrap();
        let reference = store.admit(&bytes, "image/png").unwrap();
        let (_, converted) = store.request_image(&reference, &ImagePolicy::default()).unwrap();
        let pixel = image::load_from_memory(&converted).unwrap().to_rgba8().get_pixel(0,0).0;
        assert_eq!(pixel[3], 127);
        assert_ne!(&pixel[..3], &[200,100,50]);
        assert!(image::codecs::png::PngDecoder::new(Cursor::new(&converted)).unwrap().icc_profile().unwrap().is_none());
        assert_eq!(store.read(&reference).unwrap(), bytes);
    }

    #[test]
    fn animated_sources_are_preserved_and_request_policy_is_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path().into(), ImagePolicy::default()).unwrap();
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            for color in [[255,0,0,255], [0,0,255,255]] {
                encoder.encode_frame(image::Frame::new(image::RgbaImage::from_pixel(2, 2, image::Rgba(color)))).unwrap();
            }
        }
        let reference = store.admit(&bytes, "image/gif").unwrap();
        let (_, first) = store.request_image(&reference, &ImagePolicy::default()).unwrap();
        assert_eq!(image::load_from_memory(&first).unwrap().to_rgba8().get_pixel(0,0).0, [255,0,0,255]);
        assert!(store.request_image(&reference, &ImagePolicy { animation:AnimationPolicy::Reject, ..Default::default() }).unwrap_err().contains("animated"));
        assert_eq!(store.read(&reference).unwrap(), bytes);
    }

    #[test]
    fn runtime_policy_is_shared_validated_and_resettable() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path().into(), ImagePolicy::default()).unwrap();
        let clone = store.clone();
        let policy = ImagePolicy { max_request_images: 2, quality: 60, ..Default::default() };
        store.set_request_policy(Some(policy)).unwrap();
        assert_eq!(clone.effective_request_policy(store.policy()).max_request_images, 2);
        assert!(store.set_request_policy(Some(ImagePolicy { max_request_images:0, ..Default::default() })).is_err());
        assert!(store.set_request_policy(Some(ImagePolicy { max_input_bytes:1, ..Default::default() })).is_err());
        assert_eq!(clone.effective_request_policy(store.policy()).quality, 60);
        store.set_request_policy(None).unwrap();
        assert_eq!(clone.effective_request_policy(store.policy()).quality, 85);
    }

    #[test]
    fn request_budgets_remove_oldest_occurrences_without_mutating_history() {
        use crate::session::projection::{ModelContext, ModelTurn};
        use rness_protocol::events::{ContentPart, ToolResult, ToolResultContentPart};
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path().into(), ImagePolicy::default()).unwrap();
        let attachment = store.admit(&png(), "image/png").unwrap();
        let context = ModelContext { turns: vec![
            ModelTurn::User { content: vec![ContentPart::Image { attachment: attachment.clone() }] },
            ModelTurn::ToolResults { results: vec![ToolResult {
                call:"c1".into(), name:"camera".into(), output:"caption".into(),
                content:vec![ToolResultContentPart::Text { text:"caption".into() }, ToolResultContentPart::Image { attachment:attachment.clone() }],
                is_error:false, duration_ms:0, tasks:None, plan_review:None, presentation:None,
            }] },
        ], ..Default::default() };
        let policy = ImagePolicy { max_request_images:1, ..Default::default() };
        let projected = store.project_request(&context, &policy).unwrap();
        assert!(matches!(&projected.turns[0], ModelTurn::User { content } if matches!(&content[0], ContentPart::Text { text } if text.contains("omitted"))));
        assert!(matches!(&context.turns[0], ModelTurn::User { content } if matches!(&content[0], ContentPart::Image { .. })));
        assert_eq!(store.read(&attachment).unwrap(), png());
        assert_eq!(projected, store.project_request(&context, &policy).unwrap());
        let projected = store.project_request(&context, &ImagePolicy { max_request_bytes:1, ..Default::default() }).unwrap();
        let ModelTurn::ToolResults { results } = &projected.turns[1] else { panic!("missing results") };
        assert!(results[0].output.contains("caption"));
        assert!(results[0].output.contains("omitted"));
        assert!(!results[0].content.iter().any(|p| matches!(p, ToolResultContentPart::Image { .. })));
    }

    #[test]
    fn durable_source_and_cached_policy_variants() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path().into(), ImagePolicy::default()).unwrap();
        let source = png();
        let reference = store.admit(&source, "image/png").unwrap();
        assert_eq!(store.read(&reference).unwrap(), source);
        let policy = ImagePolicy { max_pixels: 128, max_dimension: 16, ..Default::default() };
        let first = store.request_image(&reference, &policy).unwrap();
        assert_eq!(first, store.request_image(&reference, &policy).unwrap());
        let image = image::load_from_memory(&first.1).unwrap();
        assert_eq!(image.dimensions(), (16, 8));
        assert!(image.color().has_alpha());
        assert_eq!(store.read(&reference).unwrap(), source);
        let reopened = ImageStore::new(dir.path().into(), ImagePolicy::default()).unwrap();
        assert_eq!(reopened.read(&reference).unwrap(), source);
    }

    #[test]
    fn rejects_spoofed_mime_limits_corruption_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path().into(), ImagePolicy::default()).unwrap();
        let source = png();
        assert!(store.admit(&source, "image/jpeg").is_err());
        assert!(store.admit(b"not an image", "image/png").is_err());
        let mut reference = store.admit(&source, "image/png").unwrap();
        reference.id = "../../secret".into();
        assert!(store.read(&reference).is_err());
        let reference = store.admit(&source, "image/png").unwrap();
        std::fs::write(store.path(&reference.id).unwrap(), b"corrupt").unwrap();
        assert!(store.read(&reference).is_err());
        let bounded = ImageStore::new(dir.path().into(), ImagePolicy { max_input_pixels: 10, ..Default::default() }).unwrap();
        assert!(bounded.admit(&source, "image/png").is_err());
        let reference = store.admit(&source, "image/png").unwrap();
        assert!(store.request_image(&reference, &ImagePolicy { max_bytes: 1, ..Default::default() }).is_err());
    }
}

pub type ImageProcessor = dyn Fn(&ImageRef, &[u8]) -> Result<Vec<u8>, String> + Send + Sync;

#[derive(Clone)]
pub struct ImageStore {
    processor: std::sync::Arc<std::sync::RwLock<Option<(String, std::sync::Arc<ImageProcessor>)>>>,
    root: PathBuf,
    request_policy: std::sync::Arc<std::sync::RwLock<Option<ImagePolicy>>>,
    policy: ImagePolicy,
}

fn digest(data: &[u8]) -> String { format!("{:x}", Sha256::digest(data)) }

fn media_type(format: ImageFormat) -> Result<&'static str, String> {
    match format {
        ImageFormat::Png => Ok("image/png"), ImageFormat::Jpeg => Ok("image/jpeg"),
        ImageFormat::WebP => Ok("image/webp"), ImageFormat::Gif => Ok("image/gif"),
        _ => Err("only PNG, JPEG, WebP and GIF images are supported".into()),
    }
}

impl ImageStore {
    pub fn set_processor(&self, processor: Option<(String, std::sync::Arc<ImageProcessor>)>) -> Result<(), String> {
        if processor.as_ref().is_some_and(|(version, _)| version.is_empty()) { return Err("image processor version must not be empty".into()); }
        *self.processor.write().expect("image processor lock") = processor;
        Ok(())
    }

    pub fn policy(&self) -> &ImagePolicy { &self.policy }

    pub fn effective_request_policy(&self, fallback: &ImagePolicy) -> ImagePolicy {
        self.request_policy.read().expect("image policy lock").clone().unwrap_or_else(|| fallback.clone())
    }

    pub fn set_request_policy(&self, policy: Option<ImagePolicy>) -> Result<(), String> {
        if let Some(policy) = &policy {
            policy.validate()?;
            if policy.max_input_bytes != self.policy.max_input_bytes || policy.max_input_pixels != self.policy.max_input_pixels
                || policy.max_input_dimension != self.policy.max_input_dimension {
                return Err("image admission limits are startup-only".into());
            }
        }
        *self.request_policy.write().expect("image policy lock") = policy;
        Ok(())
    }

    pub fn new(root: PathBuf, policy: ImagePolicy) -> Result<Self, String> {
        policy.validate()?;
        Ok(Self { root, policy, request_policy: Default::default(), processor: Default::default() })
    }

    fn decode(&self, data: &[u8]) -> Result<(image::DynamicImage, ImageFormat), String> {
        if data.is_empty() || data.len() > self.policy.max_input_bytes {
            return Err("image exceeds input byte limit or is empty".into());
        }
        let format = image::guess_format(data).map_err(|e| e.to_string())?;
        media_type(format)?;
        let (width, height) = ImageReader::with_format(Cursor::new(data), format)
            .into_dimensions().map_err(|e| e.to_string())?;
        if width.max(height) > self.policy.max_input_dimension {
            return Err("image exceeds input dimension limit".into());
        }
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > self.policy.max_input_pixels {
            return Err("image exceeds input pixel limit".into());
        }
        let mut reader = ImageReader::with_format(Cursor::new(data), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(width);
        limits.max_image_height = Some(height);
        limits.max_alloc = Some(self.policy.max_input_pixels.saturating_mul(16));
        reader.limits(limits);
        let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
        let orientation = image::ImageDecoder::orientation(&mut decoder).map_err(|e| e.to_string())?;
        let mut image = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
        image.apply_orientation(orientation);
        Ok((image, format))
    }

    pub fn cached_upload(&self, key: &str, now: u64) -> Result<Option<(String, u64)>, String> {
        self.path(key)?;
        let path = self.root.join("uploads").join(key);
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let (id, expires): (String, u64) = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if expires <= now.saturating_add(60) { return Ok(None); }
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') { return Err("invalid cached upload ID".into()); }
        Ok(Some((id, expires)))
    }

    pub fn cache_upload(&self, key: &str, id: &str, expires: u64) -> Result<(), String> {
        self.path(key)?;
        self.write(&self.root.join("uploads").join(key), &serde_json::to_vec(&(id, expires)).map_err(|e| e.to_string())?)
    }

    pub fn record_owned_upload(&self, scope: &str, id: &str, expires: u64) -> Result<(), String> {
        self.path(scope)?;
        if id.is_empty() || id.len() > 256 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err("invalid owned upload ID".into());
        }
        self.write(&self.root.join("owned-uploads").join(scope).join(id),
            &serde_json::to_vec(&expires).map_err(|e| e.to_string())?)
    }

    // Ownership records are written when uploads succeed, not when reused.
    // Their modification time orders uploads independently of expiration.
    pub fn oldest_owned_uploads(&self, scope: &str, protected: &[String]) -> Result<Vec<String>, String> {
        use std::io::Read;
        self.path(scope)?;
        let entries = match std::fs::read_dir(self.root.join("owned-uploads").join(scope)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.to_string()),
        };
        let mut candidates = Vec::new();
        for entry in entries.take(4096) {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_file() { continue; }
            let id = entry.file_name().to_string_lossy().into_owned();
            if id.is_empty() || id.len() > 256 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') { continue; }
            let mut bytes = Vec::new();
            std::fs::File::open(entry.path()).map_err(|e| e.to_string())?.take(64).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            if !protected.contains(&id) && serde_json::from_slice::<u64>(&bytes).is_ok() {
                let created = entry.metadata().map_err(|e| e.to_string())?.modified().map_err(|e| e.to_string())?;
                candidates.push((created, id));
            }
        }
        candidates.sort();
        Ok(candidates.into_iter().take(16).map(|(_, id)| id).collect())
    }

    pub fn forget_owned_upload(&self, scope: &str, id: &str) -> Result<(), String> {
        self.path(scope)?;
        if id.is_empty() || id.len() > 256 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') { return Err("invalid owned upload ID".into()); }
        match std::fs::remove_file(self.root.join("owned-uploads").join(scope).join(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn path(&self, id: &str) -> Result<PathBuf, String> {
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err("invalid image id".into());
        }
        Ok(self.root.join("objects").join(&id[..2]).join(id))
    }

    fn write(&self, path: &Path, data: &[u8]) -> Result<(), String> {
        use std::io::Write;
        let parent = path.parent().ok_or("invalid image path")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        file.write_all(data).map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(path).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Validate and normalize before granting a tool-result attachment. Unlike
    /// uploads, this path stores the normalized result, preventing oversized or
    /// unsupported animation results from poisoning the next provider request.
    pub fn admit_tool_image(&self, session: &str, data: &[u8]) -> Result<(ImageRef, (u32, u32)), String> {
        let format = image::guess_format(data).map_err(|e| e.to_string())?;
        let source = self.admit(data, media_type(format)?)?;
        let dimensions = (source.width, source.height);
        let policy = self.effective_request_policy(self.policy());
        let (mime, bytes) = self.request_image(&source, &policy)?;
        let attachment = self.admit_for_session(session, &bytes, &mime)?;
        // Seed the identical-policy variant for the normalized attachment so
        // providers do not invoke the processor or lossy encoder a second time.
        let processor = self.processor.read().expect("image processor lock").clone();
        let key = digest(&serde_json::to_vec(&("image-v3", &attachment, &policy, processor.as_ref().map(|(version, _)| version))).map_err(|e| e.to_string())?);
        self.write(&self.root.join("variants").join(key), &bytes)?;
        Ok((attachment, dimensions))
    }

    /// Preserve admitted source bytes; processing variants never overwrite the source.
    pub fn admit(&self, data: &[u8], declared_type: &str) -> Result<ImageRef, String> {
        let (image, format) = self.decode(data)?;
        let actual = media_type(format)?;
        if declared_type != actual { return Err("image MIME type does not match its bytes".into()); }
        let id = digest(data);
        self.write(&self.path(&id)?, data)?;
        Ok(ImageRef { id, media_type: actual.into(), bytes: data.len() as u64,
            width: image.width(), height: image.height() })
    }

    pub fn admit_for_session(&self, session: &str, data: &[u8], media_type: &str) -> Result<ImageRef, String> {
        let reference = self.admit(data, media_type)?;
        let grant = self.root.join("admissions").join(digest(session.as_bytes())).join(&reference.id);
        self.write(&grant, b"")?;
        Ok(reference)
    }

    pub fn admitted_for_session(&self, session: &str, id: &str) -> bool {
        self.path(id).is_ok() && self.root.join("admissions").join(digest(session.as_bytes())).join(id).is_file()
    }

    pub fn read(&self, reference: &ImageRef) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let file = std::fs::File::open(self.path(&reference.id)?).map_err(|e| e.to_string())?;
        let mut data = Vec::new();
        file.take(self.policy.max_input_bytes as u64 + 1).read_to_end(&mut data).map_err(|e| e.to_string())?;
        if data.len() > self.policy.max_input_bytes || digest(&data) != reference.id || data.len() as u64 != reference.bytes {
            return Err("stored image is corrupt or exceeds the input limit".into());
        }
        let (image, format) = self.decode(&data)?;
        if image.dimensions() != (reference.width, reference.height) || media_type(format)? != reference.media_type {
            return Err("image reference metadata mismatch".into());
        }
        Ok(data)
    }

    /// Request-only projection; originals and durable history remain unchanged.
    /// Budgets count occurrences and base64 bytes, including nested tool images.
    pub fn project_request(&self, context: &crate::session::projection::ModelContext, policy: &ImagePolicy) -> Result<crate::session::projection::ModelContext, String> {
        use crate::session::projection::ModelTurn;
        use rness_protocol::events::{ContentPart, ToolResultContentPart};
        policy.validate()?;
        let mut lengths = Vec::new();
        let mut variants = std::collections::HashMap::new();
        let mut record = |attachment: &ImageRef| -> Result<(), String> {
            let key = serde_json::to_string(attachment).map_err(|e| e.to_string())?;
            let bytes = if let Some(bytes) = variants.get(&key) { *bytes } else {
                let (_, data) = self.request_image(attachment, policy)?;
                let bytes = data.len().div_ceil(3).saturating_mul(4);
                variants.insert(key, bytes);
                bytes
            };
            lengths.push(bytes);
            Ok(())
        };
        for turn in &context.turns {
            match turn {
                ModelTurn::User { content } | ModelTurn::Assistant { content } => for part in content {
                    if let ContentPart::Image { attachment } = part { record(attachment)?; }
                },
                ModelTurn::ToolResults { results } => for result in results { for part in &result.content {
                    if let ToolResultContentPart::Image { attachment } = part { record(attachment)?; }
                } },
            }
        }
        let mut total = lengths.iter().fold(0usize, |total, bytes| total.saturating_add(*bytes));
        let mut remove = 0;
        while lengths.len() - remove > policy.max_request_images || total > policy.max_request_bytes {
            total = total.saturating_sub(lengths[remove]);
            remove += 1;
        }
        let mut projected = context.clone();
        let placeholder = |attachment: &ImageRef| format!("[Image {} omitted from this request by image budget; {} × {}. Original attachment remains in session history. Ask the user to reattach it if needed.]", attachment.id, attachment.width, attachment.height);
        for turn in &mut projected.turns {
            match turn {
                ModelTurn::User { content } | ModelTurn::Assistant { content } => for part in content {
                    if remove > 0 { if let ContentPart::Image { attachment } = part {
                        *part = ContentPart::Text { text: placeholder(attachment) }; remove -= 1;
                    } }
                },
                ModelTurn::ToolResults { results } => for result in results {
                    for part in &mut result.content {
                        if remove > 0 { if let ToolResultContentPart::Image { attachment } = part {
                            *part = ToolResultContentPart::Text { text: placeholder(attachment) }; remove -= 1;
                        } }
                    }
                    if !result.content.is_empty() { result.output = rness_protocol::events::ToolResult::text_output(&result.content); }
                },
            }
        }
        Ok(projected)
    }

    pub fn request_image(&self, reference: &ImageRef, policy: &ImagePolicy) -> Result<(String, Vec<u8>), String> {
        policy.validate()?;
        let source = self.read(reference)?;
        let processor = self.processor.read().expect("image processor lock").clone();
        let key = digest(&serde_json::to_vec(&("image-v3", reference, policy, processor.as_ref().map(|(version, _)| version))).map_err(|e| e.to_string())?);
        let path = self.root.join("variants").join(key);
        if let Ok(file) = std::fs::File::open(&path) {
            use std::io::Read;
            let mut data = Vec::new();
            file.take(policy.max_bytes as u64 + 1).read_to_end(&mut data).map_err(|e| e.to_string())?;
            if data.len() <= policy.max_bytes {
                if let Ok((cached, format)) = self.decode(&data) {
                    if u64::from(cached.width()) * u64::from(cached.height()) <= policy.max_pixels
                        && cached.width().max(cached.height()) <= policy.max_dimension {
                        return Ok((media_type(format)?.into(), data));
                    }
                }
            }
        }
        let source = if let Some((_, process)) = processor { process(reference, &source)? } else { source };
        let (mut image, format) = self.decode(&source)?;
        if policy.animation == AnimationPolicy::Reject {
            use image::AnimationDecoder;
            let animated = match format {
                ImageFormat::Gif => {
                    let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(&source)).map_err(|e| e.to_string())?;
                    let mut limits = image::Limits::default();
                    limits.max_alloc = Some(self.policy.max_input_pixels.saturating_mul(16));
                    image::ImageDecoder::set_limits(&mut decoder, limits).map_err(|e| e.to_string())?;
                    decoder.into_frames().take(2).collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?.len() > 1
                },
                ImageFormat::Png => image::codecs::png::PngDecoder::new(Cursor::new(&source)).map_err(|e| e.to_string())?.is_apng().map_err(|e| e.to_string())?,
                ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(&source)).map_err(|e| e.to_string())?.has_animation(),
                _ => false,
            };
            if animated { return Err("animated images are disabled by image policy".into()); }
        }
        if policy.normalize_srgb {
            use image::ImageDecoder;
            let mut decoder = ImageReader::with_format(Cursor::new(&source), format).into_decoder().map_err(|e| e.to_string())?;
            if let Some(icc) = decoder.icc_profile().map_err(|e| e.to_string())? {
                let profile = moxcms::ColorProfile::new_from_slice(&icc).map_err(|e| format!("invalid image ICC profile: {e}"))?;
                let transform = profile.create_transform_8bit(moxcms::Layout::Rgba, &moxcms::ColorProfile::new_srgb(), moxcms::Layout::Rgba, moxcms::TransformOptions::default()).map_err(|e| format!("unsupported image ICC transform: {e}"))?;
                let pixels = image.to_rgba8();
                let mut converted = vec![0; pixels.len()];
                transform.transform(pixels.as_raw(), &mut converted).map_err(|e| format!("image ICC transform failed: {e}"))?;
                image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_raw(pixels.width(), pixels.height(), converted).ok_or("invalid converted image")?);
            } else {
                image.apply_color_space(image::metadata::Cicp::SRGB, Default::default()).map_err(|e| format!("image color conversion failed: {e}"))?;
            }
        }
        let scale = ((policy.max_pixels as f64 / (u64::from(image.width()) * u64::from(image.height())) as f64).sqrt())
            .min(policy.max_dimension as f64 / image.width().max(image.height()) as f64).min(1.0);
        if scale < 1.0 {
            image = image.resize((image.width() as f64 * scale).max(1.0) as u32,
                (image.height() as f64 * scale).max(1.0) as u32, image::imageops::FilterType::Lanczos3);
        }
        let mut data = Vec::new();
        let mime = if policy.lossless {
            image.write_to(&mut Cursor::new(&mut data), ImageFormat::Png).map_err(|e| e.to_string())?;
            "image/png"
        } else if image.to_rgba8().pixels().any(|pixel| pixel[3] != 255) {
            let rgba = image.to_rgba8();
            let encoder = webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height());
            for quality in [policy.quality, policy.quality.min(75), policy.quality.min(60)] {
                let mut config = webp::WebPConfig::new().map_err(|e| format!("WebP configuration: {e:?}"))?;
                config.quality = f32::from(quality);
                config.method = 0;
                data = encoder.encode_advanced(&config).map_err(|e| format!("WebP encoding: {e:?}"))?.to_vec();
                if data.len() <= policy.max_bytes { break; }
            }
            "image/webp"
        } else {
            for quality in [policy.quality, policy.quality.min(75), policy.quality.min(60)] {
                data.clear();
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, quality)
                    .encode_image(&image.to_rgb8()).map_err(|e| e.to_string())?;
                if data.len() <= policy.max_bytes { break; }
            }
            "image/jpeg"
        };
        if data.len() > policy.max_bytes { return Err("processed image exceeds byte limit; adjust image policy".into()); }
        self.write(&path, &data)?;
        Ok((mime.into(), data))
    }
}

//! The app shell: model, actions, and the frame loop.
//!
//! Elm-ish: crossterm events + protocol frames mutate a [`Model`];
//! components render immediate-mode from it. A slow component can't
//! block input — rendering is budgeted per tick and frames coalesce.

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event as TermEvent, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use rness_protocol::api::{ClientRequest, History};
use rness_protocol::events::{
    ChunkDelta, ContentPart, SessionEvent, SessionId, ToolCallId, UserIntent,
};
use rness_protocol::frames::Frame;
use tokio::sync::mpsc;

use crate::component::Ctx;
use crate::slots::Slots;
use crate::theme::Theme;

/// The engine seam. In-process today (CLI wraps SessionService), remote
/// later — the TUI only ever sees protocol types.
pub trait Backend: Send + Sync {
    fn admit_image(&self, _session: &SessionId, _data: &[u8], _media_type: &str) -> Result<rness_protocol::events::ImageRef, String> {
        Err("image uploads are not supported by this backend".into())
    }
    fn read_image(&self, _session: &SessionId, _id: &str) -> Result<Vec<u8>, String> {
        Err("image retrieval is not supported by this backend".into())
    }
    fn request(&self, request: ClientRequest);
    fn submit(&self, request: ClientRequest) -> Result<Option<String>, String> {
        self.request(request);
        Ok(None)
    }
    fn complete(&self, _session: &SessionId, _text: String) {}
    fn command_running(&self, _session: &SessionId) -> bool { false }
    fn history(&self, session: &SessionId) -> History;
    /// None requests a full reload (unsupported cursor or backend).
    fn history_after(&self, _session: &SessionId, _after: &str) -> Option<Vec<rness_protocol::events::Envelope>> { None }
    fn prepare_input(&self, _session: &SessionId, text: &str) -> Result<Option<Vec<ContentPart>>, String> {
        Ok(Some(vec![ContentPart::Text { text: text.into() }]))
    }
}

/// One rendered conversation entry, derived from durable events.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User { content: Vec<ContentPart> },
    Assistant { model: String, content: Vec<ContentPart> },
    ToolResult { call: ToolCallId, name: String, output: String, is_error: bool },
    Notice(String),
}

/// The in-flight step being streamed (ephemeral; replaced by durable
/// entries on StepCommitted).
#[derive(Debug, Clone, Default)]
pub struct LiveStep {
    pub text: String,
    pub thinking: String,
    /// Tool calls streaming in: (call id, accumulated args).
    pub tool_args: Vec<(ToolCallId, String)>,
    /// Tools currently executing: (call id, name).
    pub running_tools: Vec<(ToolCallId, String)>,
}

/// Everything components render from.
pub struct Model {
    pub session: SessionId,
    pub entries: Vec<Entry>,
    pub entry_ids: Vec<String>,
    pub history_revision: u64,
    pub history_epoch: u64,
    projected_session: Option<SessionId>,
    projected_events: usize,
    projected_tail: Option<String>,
    projected_entries: usize,
    projected_names: std::collections::HashMap<ToolCallId, String>,
    projected_failed_attempts: usize,
    #[cfg(test)]
    projection_visits: usize,
    pub assistant_ids: std::collections::HashMap<usize, usize>,
    pub tool_durations: std::collections::HashMap<ToolCallId, u64>,
    pub error_entries: std::collections::HashSet<usize>,
    pub cancelled_tools: std::collections::HashSet<ToolCallId>,
    durable_assistant_ids: std::collections::HashMap<String, usize>,
    pub next_assistant_id: usize,
    pub live_assistant_id: Option<usize>,

    pub live: Option<LiveStep>,
    pub busy: bool,
    pub model_name: String,
    /// Scrollback offset from the bottom (0 = pinned to latest).
    pub scroll_from_bottom: u16,
    /// A sensitive tool call paused for a decision. The approval overlay
    /// selects itself into `overlay` while this is Some (chain pattern —
    /// no shell-driven mount/unmount).
    pub pending_approval: Option<crate::modules::approval::PendingApproval>,
    pub should_quit: bool,
}

impl Model {
    pub fn new(session: SessionId, model_name: String) -> Self {
        Self {
            session,
            entries: Vec::new(),
            entry_ids: Vec::new(),
            history_revision: 0,
            history_epoch: 0,
            projected_session: None,
            projected_events: 0,
            projected_tail: None,
            projected_entries: 0,
            projected_names: Default::default(),
            projected_failed_attempts: 0,
            #[cfg(test)]
            projection_visits: 0,
            assistant_ids: Default::default(),
            tool_durations: Default::default(),
            error_entries: Default::default(),
            cancelled_tools: Default::default(),
            durable_assistant_ids: Default::default(),
            next_assistant_id: 0,
            live_assistant_id: None,

            live: None,
            busy: false,
            model_name,
            scroll_from_bottom: 0,
            pending_approval: None,
            should_quit: false,
        }
    }

    /// Project an immutable, append-only session log. Session changes, shortened
    /// histories and compaction checkpoints require a full projection.
    pub fn load_history(&mut self, history: &History) {
        let append = self.projected_session.as_ref() == Some(&history.session)
            && self.projected_events > 0
            && history.envelopes.len() >= self.projected_events
            && history.envelopes[self.projected_events - 1].id.as_str()
                == self.projected_tail.as_deref().unwrap_or_default()
            && self.entries.len() >= self.projected_entries
            && !history.envelopes[self.projected_events..].iter()
                .any(|env| matches!(env.event, SessionEvent::Compaction(_)));
        if append {
            self.append_history(&history.envelopes[self.projected_events..]);
            return;
        }
        self.history_epoch = self.history_epoch.wrapping_add(1);
        self.cancelled_tools.clear();
        let old_entries = std::mem::take(&mut self.entries);
        let old_ids = std::mem::take(&mut self.entry_ids);
        let old_durations = std::mem::take(&mut self.tool_durations);
        let old_errors = std::mem::take(&mut self.error_entries);
        let old_assistant_ids = std::mem::take(&mut self.assistant_ids);
        self.assistant_ids.clear();
        self.tool_durations.clear();
        self.error_entries.clear();
        // Compaction shadowing: events named by a live checkpoint's
        // `replaces` are hidden; a notice renders where the folded span
        // began (anchor = first shadowed id).
        let mut shadowed: std::collections::HashSet<&str> = Default::default();
        let mut anchors: std::collections::HashMap<&str, usize> = Default::default();
        for env in history.envelopes.iter().rev() {
            if let SessionEvent::Compaction(c) = &env.event {
                if shadowed.contains(env.id.as_str()) {
                    continue;
                }
                shadowed.extend(c.replaces.iter().map(|s| s.as_str()));
                if let Some(first) = c.replaces.first() {
                    anchors.insert(first.as_str(), c.replaces.len());
                }
            }
        }
        // Tool names live on the assistant ToolUse parts; map call → name.
        let mut names: std::collections::HashMap<ToolCallId, String> = Default::default();
        let mut failed_attempts = 0;
        self.project_events(&history.envelopes, &shadowed, &anchors, &mut names, &mut failed_attempts);
        self.projected_names = names;
        self.projected_failed_attempts = failed_attempts;
        if self.history_revision == 0 || self.entries != old_entries || self.entry_ids != old_ids
            || self.tool_durations != old_durations || self.error_entries != old_errors || self.assistant_ids != old_assistant_ids {
            self.history_revision = self.history_revision.wrapping_add(1);
        }
        self.remember_projection(history);
    }

    fn append_history(&mut self, events: &[rness_protocol::events::Envelope]) {
        let removed_local = self.entries.len() != self.projected_entries;
        self.entries.truncate(self.projected_entries);
        let mut names = std::mem::take(&mut self.projected_names);
        let mut failed_attempts = self.projected_failed_attempts;
        self.project_events(events, &Default::default(), &Default::default(), &mut names, &mut failed_attempts);
        self.projected_names = names;
        self.projected_failed_attempts = failed_attempts;
        if removed_local || self.entries.len() != self.projected_entries {
            self.history_revision = self.history_revision.wrapping_add(1);
        }
        self.projected_events += events.len();
        self.projected_entries = self.entries.len();
        if let Some(last) = events.last() { self.projected_tail = Some(last.id.clone()); }
    }

    fn remember_projection(&mut self, history: &History) {
        self.projected_session = Some(history.session.clone());
        self.projected_events = history.envelopes.len();
        self.projected_tail = history.envelopes.last().map(|env| env.id.clone());
        self.projected_entries = self.entries.len();
    }

    fn project_events(
        &mut self,
        events: &[rness_protocol::events::Envelope],
        shadowed: &std::collections::HashSet<&str>,
        anchors: &std::collections::HashMap<&str, usize>,
        names: &mut std::collections::HashMap<ToolCallId, String>,
        failed_attempts: &mut usize,
    ) {
        for env in events {
            #[cfg(test)]
            { self.projection_visits += 1; }
            if shadowed.contains(env.id.as_str()) {
                if let Some(span) = anchors.get(env.id.as_str()) {
                    self.entry_ids.push(format!("compaction:{}", env.id));
                    self.entries.push(Entry::Notice(format!(
                        "── {span} earlier events compacted into a summary ──"
                    )));
                }
                continue;
            }
            let previous_len = self.entries.len();
            match &env.event {
                SessionEvent::UserMessage(m) => {
                    self.entries.push(Entry::User { content: m.content.clone() })
                }
                SessionEvent::TurnStarted { .. } => *failed_attempts = 0,
                SessionEvent::TurnEnded { outcome: rness_protocol::events::TurnOutcome::Failed, .. } => {
                    let detail = if *failed_attempts > 0 { format!(" after {failed_attempts} failed provider attempt(s); no automatic retries remain") } else { String::new() };
                    self.error_entries.insert(self.entries.len());
                    self.entries.push(Entry::Notice(format!("Turn failed{detail}. Use /retry to continue from saved context.")));
                }
                SessionEvent::AssistantMessage(m) => {
                    *failed_attempts = 0;
                    for part in &m.content {
                        if let ContentPart::ToolUse { call, name, .. } = part {
                            names.insert(call.clone(), name.clone());
                        }
                    }
                    let identity = *self.durable_assistant_ids.entry(env.id.clone()).or_insert_with(|| {
                        let id = self.next_assistant_id;
                        self.next_assistant_id += 1;
                        id
                    });
                    self.assistant_ids.insert(self.entries.len(), identity);
                    self.entries.push(Entry::Assistant {
                        model: m.model.clone(),
                        content: m.content.clone(),
                    });
                }
                SessionEvent::AssistantAttempt(attempt) => {
                    if let rness_protocol::events::AttemptOutcome::Error { message, code, retry_in_ms, .. } = &attempt.outcome {
                        *failed_attempts += 1;
                        let recovery = retry_in_ms.map(|ms| format!("; retry {} scheduled after {ms} ms", *failed_attempts + 1)).unwrap_or_default();
                        self.error_entries.insert(self.entries.len());
                        self.entries.push(Entry::Notice(format!("Provider error ({}), attempt {} [{}]: {}{}", attempt.model, failed_attempts, code.as_deref().unwrap_or("PROVIDER"), message, recovery)));
                    }
                }
                SessionEvent::ToolResult(r) => {
                    if r.presentation.as_ref().and_then(|p| p.get("outcome")).and_then(|v| v.as_str()) == Some("approval_cancelled") {
                        self.cancelled_tools.insert(r.call.clone());
                    }
                    self.tool_durations.insert(r.call.clone(), r.duration_ms);
                    self.entries.push(Entry::ToolResult {
                    call: r.call.clone(),
                    name: names.get(&r.call).cloned().unwrap_or_default(),
                    output: if r.content.is_empty() { r.output.clone() } else {
                        r.content.iter().map(|part| match part {
                            rness_protocol::events::ToolResultContentPart::Text { text } => text.clone(),
                            rness_protocol::events::ToolResultContentPart::Image { attachment } => format!("[Image · {} × {} · {} bytes]", attachment.width, attachment.height, attachment.bytes),
                        }).collect::<Vec<_>>().join("\n")
                    },
                    is_error: r.is_error,
                });
                },
                _ => {}
            }
            if self.entries.len() > previous_len { self.entry_ids.push(env.id.clone()); }
        }
    }

    /// Apply one live frame.
    pub fn apply_frame(&mut self, frame: &Frame) -> FrameEffect {
        match frame {
            Frame::StepStarted { .. } => {
                self.live_assistant_id = Some(self.next_assistant_id);
                self.next_assistant_id += 1;
                self.busy = true;
                self.live = Some(LiveStep::default());
                FrameEffect::None
            }
            Frame::Delta { chunk, .. } => {
                let live = self.live.get_or_insert_with(LiveStep::default);
                match chunk {
                    ChunkDelta::Text { t } => live.text.push_str(t),
                    ChunkDelta::Thinking { t } => live.thinking.push_str(t),
                    ChunkDelta::ToolArgs { call, t } => {
                        match live.tool_args.iter_mut().find(|(c, _)| c == call) {
                            Some((_, args)) => args.push_str(t),
                            None => live.tool_args.push((call.clone(), t.clone())),
                        }
                    }
                }
                FrameEffect::None
            }
            Frame::ToolStarted { call, name, .. } => {
                let live = self.live.get_or_insert_with(LiveStep::default);
                live.running_tools.push((call.clone(), name.clone()));
                FrameEffect::None
            }
            Frame::ToolOutput { .. } => FrameEffect::None,
            Frame::StepCommitted { event, .. } => {
                // Durable now — reload from the log and drop the stream state.
                if let Some(id) = self.live_assistant_id.take() {
                    self.durable_assistant_ids.insert(event.clone(), id);
                }
                self.live = None;
                FrameEffect::Reconcile
            }
            Frame::TurnIdle { .. } => {
                self.live_assistant_id = None;
                self.busy = false;
                self.live = None;
                FrameEffect::Reconcile
            }
            // Compaction (or any out-of-turn durable change): reload.
            Frame::HistoryChanged { .. } => FrameEffect::Reconcile,
            // Remote-approval announcements: the in-process TUI gets its
            // questions over the answerer channel, not from frames.
            Frame::ApprovalRequested { .. } | Frame::ApprovalResolved { .. } => FrameEffect::None,
        }
    }
}

/// What applying a frame asks of the shell.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameEffect {
    None,
    /// Re-fetch durable history (a commit happened).
    Reconcile,
}

/// Component-emitted commands, applied by the shell after input handling.
#[derive(Debug)]
pub enum Action {
    Submit(String),
    SubmitImages(String, Vec<rness_protocol::events::ImageRef>),
    PreviewHistoryImage,
    PasteClipboard,
    Cancel,
    Quit,
    ScrollUp(u16),
    ScrollDown(u16),
    /// The pending approval was answered with this decision.
    ResolveApproval(rness_protocol::api::ApprovalDecision),
    /// Open extension point: named action + payload, broadcast to every
    /// mounted component. How Lua/plugin components talk to each other
    /// without extending this enum.
    Custom(String, serde_json::Value),
    Complete(String),
    CompletionResult(SessionId, String, Vec<String>),
    CommandResult(SessionId, String),
    Notice(String),
    /// Point the TUI at another session: model resets and rehydrates
    /// from that session's durable history.
    SwitchSession(SessionId),
}

pub struct App {
    pub model: Model,
    pub slots: Slots,
    pub theme: Theme,
    pub colorschemes: std::collections::BTreeMap<String, Theme>,
    /// External app toggles (checked before the focused component so
    /// ctrl+e etc. work while typing). None = no app host connected.
    pub apps: Option<crate::modules::ext_apps::AppsState>,
    /// Rebindable host bindings (chord → named action). Lua rewrites
    /// this via rness.keymaps; the shell only reads.
    pub keymap: crate::keymaps::KeymapState,
    edit_prompt: Option<serde_json::Value>,
    command_results: std::collections::HashMap<SessionId, Vec<String>>,
    background_models: std::collections::HashMap<SessionId, Model>,
    backend: Arc<dyn Backend>,
}

impl App {
    pub fn new(model: Model, slots: Slots, backend: Arc<dyn Backend>) -> Self {
        Self {
            model,
            slots,
            theme: Theme::default(),
            colorschemes: [("default".into(), Theme::default())].into(),
            apps: None,
            keymap: crate::keymaps::KeymapState::stock(),
            edit_prompt: None,
            command_results: Default::default(),
            background_models: Default::default(),
            backend,
        }
    }

    /// Load durable history into the model.
    pub fn reconcile(&mut self) {
        let delta = self.model.projected_tail.as_deref()
            .filter(|_| self.model.projected_session.as_ref() == Some(&self.model.session)
                && self.model.entries.len() >= self.model.projected_entries)
            .and_then(|after| self.backend.history_after(&self.model.session, after))
            .filter(|events| !events.iter().any(|env| matches!(env.event, SessionEvent::Compaction(_))));
        if let Some(events) = delta {
            self.model.append_history(&events);
        } else {
            let history = self.backend.history(&self.model.session);
            self.model.load_history(&history);
        }
        if let Some(results) = self.command_results.get(&self.model.session) {
            self.model.entries.extend(results.iter().cloned().map(Entry::Notice));
        }
    }

    pub fn apply_frame(&mut self, frame: &Frame) {
        let session = match frame {
            Frame::StepStarted { session, .. } | Frame::Delta { session, .. }
            | Frame::ToolStarted { session, .. } | Frame::ToolOutput { session, .. }
            | Frame::StepCommitted { session, .. } | Frame::TurnIdle { session }
            | Frame::HistoryChanged { session } | Frame::ApprovalRequested { session, .. }
            | Frame::ApprovalResolved { session, .. } => session,
        };
        if session != &self.model.session {
            if !self.background_models.contains_key(session) && !matches!(frame,
                Frame::StepStarted { .. } | Frame::Delta { .. } | Frame::ToolStarted { .. }) {
                return;
            }
            let model = self.background_models.entry(session.clone())
                .or_insert_with(|| Model::new(session.clone(), self.model.model_name.clone()));
            model.apply_frame(frame);
            if matches!(frame, Frame::TurnIdle { .. }) {
                self.background_models.remove(session);
            }
            return;
        }
        if self.model.apply_frame(frame) == FrameEffect::Reconcile {
            self.reconcile();
        }
    }

    /// Route a terminal event; apply resulting actions.
    pub fn on_term_event(&mut self, event: TermEvent) {
        match event {
            TermEvent::Paste(text) => {
                let ctx = Ctx { model: &self.model, theme: &self.theme };
                let actions = self.slots.focused_mut(&ctx).map(|c| c.on_paste(&ctx, &text).actions).unwrap_or_default();
                for action in actions { self.apply(action); }
            }
            TermEvent::Key(key) => {
                let actions = self.route_key(key);
                for action in actions {
                    self.apply(action);
                }
            }
            // Wheel scroll goes to the transcript regardless of focus —
            // the editor never owns vertical scrolling.
            TermEvent::Mouse(m) => match m.kind {
                crossterm::event::MouseEventKind::ScrollUp => self.apply(Action::ScrollUp(3)),
                crossterm::event::MouseEventKind::ScrollDown => self.apply(Action::ScrollDown(3)),
                _ => {}
            },
            _ => {}
        }
    }

    fn route_key(&mut self, key: KeyEvent) -> Vec<Action> {
        // App toggles fire globally — even while the editor is focused.
        // (Except when a modal overlay like approval holds focus.)
        if let Some(apps) = &self.apps {
            if self.model.pending_approval.is_none()
                && crate::modules::ext_apps::handle_global_key(apps, &key)
            {
                return vec![];
            }
        }
        // Focused component (active overlay > editor) sees the key first.
        let ctx = Ctx { model: &self.model, theme: &self.theme };
        if let Some(focused) = self.slots.focused_mut(&ctx) {
            let outcome = focused.on_key(&ctx, key);
            if outcome.handled {
                return outcome.actions;
            }
        }
        let message_outcome = self.slots.message_key(&ctx, key);
        if message_outcome.handled { return message_outcome.actions; }
        // Host keymap: the rebindable chord→action table (stock seeded
        // in keymaps.rs, rewritten live by rness.keymaps).
        use crate::keymaps::HostAction;
        match self.keymap.lookup(&key) {
            Some(HostAction::CancelOrQuit) => {
                if self.model.busy || self.backend.command_running(&self.model.session) {
                    vec![Action::Cancel]
                } else {
                    vec![Action::Quit]
                }
            }
            Some(HostAction::Quit) => vec![Action::Quit],
            Some(HostAction::ScrollUpPage) => vec![Action::ScrollUp(10)],
            Some(HostAction::ScrollDownPage) => vec![Action::ScrollDown(10)],
            Some(HostAction::ScrollUpLine) => vec![Action::ScrollUp(1)],
            Some(HostAction::ScrollDownLine) => vec![Action::ScrollDown(1)],
            None => vec![],
        }
    }

    pub fn apply(&mut self, action: Action) {
        match action {
            Action::PreviewHistoryImage => {
                let history = self.backend.history(&self.model.session);
                let mut references = Vec::new();
                for envelope in &history.envelopes {
                    match &envelope.event {
                        SessionEvent::UserMessage(message) => for part in &message.content { if let ContentPart::Image { attachment } = part { references.push(attachment.clone()); } },
                        SessionEvent::AssistantMessage(message) => for part in &message.content { if let ContentPart::Image { attachment } = part { references.push(attachment.clone()); } },
                        SessionEvent::ToolResult(result) => for part in &result.content { if let rness_protocol::events::ToolResultContentPart::Image { attachment } = part { references.push(attachment.clone()); } },
                        _ => {},
                    }
                }
                if references.is_empty() { self.apply(Action::Notice("No images in session history".into())); return; }
                self.apply(Action::Custom("input:history-images".into(), serde_json::to_value(&references).unwrap()));
                for reference in references {
                    let thumbnail = self.backend.read_image(&self.model.session, &reference.id).and_then(|bytes| image::load_from_memory(&bytes).map_err(|e| e.to_string())).map(|image| image.thumbnail(160.min(image.width()), 80.min(image.height())).to_rgba8());
                    match thumbnail {
                        Ok(image) => self.apply(Action::Custom("input:image-thumbnail".into(), serde_json::json!({"id":reference.id,"width":image.width(),"height":image.height(),"pixels":image.into_raw()}))),
                        Err(error) => self.apply(Action::Notice(format!("Cannot preview image: {error}"))),
                    }
                }
            }
            Action::PasteClipboard => {
                let result: Result<_, String> = (|| {
                    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
                    let pixels = match clipboard.get_image() {
                        Ok(pixels) => pixels,
                        Err(arboard::Error::ContentNotAvailable) => {
                            let text = clipboard.get_text().map_err(|e| e.to_string())?;
                            self.apply(Action::Custom("input:clipboard-text".into(), serde_json::json!(text)));
                            return Ok(None);
                        }
                        Err(error) => return Err(error.to_string()),
                    };
                    let width = u32::try_from(pixels.width).map_err(|e| e.to_string())?;
                    let height = u32::try_from(pixels.height).map_err(|e| e.to_string())?;
                    if u64::from(width) * u64::from(height) > 40_000_000 { return Err("clipboard image exceeds pixel limit".into()); }
                    let image = image::RgbaImage::from_raw(width, height, pixels.bytes.into_owned()).ok_or("invalid clipboard pixels")?;
                    let thumbnail = image::DynamicImage::ImageRgba8(image.clone()).thumbnail(160.min(width), 80.min(height)).to_rgba8();
                    let mut bytes = std::io::Cursor::new(Vec::new());
                    image::DynamicImage::ImageRgba8(image).write_to(&mut bytes, image::ImageFormat::Png).map_err(|e| e.to_string())?;
                    let reference = self.backend.admit_image(&self.model.session, bytes.get_ref(), "image/png")?;
                    Ok(Some((reference, thumbnail)))
                })();
                match result {
                    Ok(None) => {},
                    Ok(Some((image, thumbnail))) => {
                        let id = image.id.clone();
                        self.apply(Action::Custom("input:image-added".into(), serde_json::to_value(image).unwrap()));
                        self.apply(Action::Custom("input:image-thumbnail".into(), serde_json::json!({"id":id, "width":thumbnail.width(), "height":thumbnail.height(), "pixels":thumbnail.into_raw()})));
                    },
                    Err(error) => self.apply(Action::Notice(format!("Cannot paste from clipboard: {error}"))),
                }
            }
            Action::SubmitImages(text, images) => {
                if text.trim_start().starts_with('/') {
                    self.apply(Action::Notice("Send images with a prompt, not a slash command".into()));
                    return;
                }
                let mut content = vec![ContentPart::Text { text }];
                content.extend(images.into_iter().map(|attachment| ContentPart::Image { attachment }));
                match self.backend.submit(ClientRequest::Send { session: self.model.session.clone(), intent: UserIntent::Followup, content: content.clone() }) {
                    Ok(None) => {
                        self.model.entries.push(Entry::User { content });
                        self.apply(Action::Custom("input:images-submitted".into(), serde_json::Value::Null));
                    }
                    Ok(Some(message)) => self.apply(Action::Notice(message)),
                    Err(error) => self.apply(Action::Notice(error)),
                }
            }
            Action::Submit(text) => {
                if text.trim().is_empty() {
                    return;
                }
                if text.trim() == "/help retry" || text.trim() == "/retry --help" {
                    self.model.entries.push(Entry::Notice("/retry — Continue the last failed turn from saved context without another user message. Requires an idle session; configuration changes are allowed.".into()));
                    return;
                }
                if text.split_whitespace().next() == Some("/retry") && text.trim() != "/retry" {
                    self.model.entries.push(Entry::Notice("Usage: /retry".into()));
                    return;
                }
                if text.trim() == "/retry" {
                    match self.backend.submit(ClientRequest::Retry { session: self.model.session.clone() }) {
                        Ok(_) => { self.model.busy = true; self.model.scroll_from_bottom = 0; }
                        Err(error) => self.model.entries.push(Entry::Notice(error)),
                    }
                    return;
                }
                if text.split_whitespace().next() == Some("/colorscheme") {
                    let words = text.split_whitespace().collect::<Vec<_>>();
                    if words.len() != 2 {
                        self.model.entries.push(Entry::Notice("Usage: /colorscheme <name>".into()));
                    } else if let Some(theme) = self.colorschemes.get(words[1]) {
                        self.theme = theme.clone();
                        self.model.entries.push(Entry::Notice(format!("Colorscheme: {}", words[1])));
                    } else {
                        self.model.entries.push(Entry::Notice(format!("Unknown colorscheme: {}", words[1])));
                    }
                    return;
                }
                let mut words = text.split_whitespace();
                if words.next() == Some("/unload") {
                    if let (Some(name), None, Some(apps)) = (words.next(), words.next(), &self.apps) {
                        apps.request_unload(name.to_owned());
                    } else {
                        self.model.entries.push(Entry::Notice("Usage: /unload <plugin> (requires the local plugin host)".into()));
                    }
                    return;
                }
                let content = match self.backend.prepare_input(&self.model.session, &text) {
                    Ok(Some(content)) => content,
                    Ok(None) => { self.model.entries.push(Entry::Notice("Command completed".into())); return; }
                    Err(error) => { self.model.entries.push(Entry::Notice(error)); return; }
                };
                match self.backend.submit(ClientRequest::Send {
                    session: self.model.session.clone(),
                    intent: UserIntent::Followup,
                    content: content.clone(),
                }) {
                    Ok(None) => self.model.entries.push(Entry::User { content }),
                    Ok(Some(message)) => self.model.entries.push(Entry::Notice(message)),
                    Err(error) => self.model.entries.push(Entry::Notice(error)),
                }
                self.model.scroll_from_bottom = 0;
            }
            Action::Cancel => {
                self.backend.request(ClientRequest::Cancel { session: self.model.session.clone() });
            }
            Action::Quit => self.model.should_quit = true,
            Action::ScrollUp(n) => {
                self.model.scroll_from_bottom = self.model.scroll_from_bottom.saturating_add(n)
            }
            Action::ScrollDown(n) => {
                self.model.scroll_from_bottom = self.model.scroll_from_bottom.saturating_sub(n)
            }
            Action::ResolveApproval(decision) => {
                if let Some(pending) = self.model.pending_approval.take() {
                    let _ = pending.respond.send(decision);
                }
            }
            Action::Complete(text) => self.backend.complete(&self.model.session, text),
            Action::CompletionResult(session, text, values) => {
                if session == self.model.session {
                    let ctx = Ctx { model: &self.model, theme: &self.theme };
                    self.slots.broadcast(&ctx, "input:completion", &serde_json::json!({"text":text,"values":values}));
                }
            }
            Action::CommandResult(session, message) => {
                self.command_results.entry(session.clone()).or_default().push(message.clone());
                if session == self.model.session { self.model.entries.push(Entry::Notice(message)); }
            }
            Action::Notice(text) => self.model.entries.push(Entry::Notice(text)),
            Action::Custom(name, payload) => {
                if name == "terminal:edit-prompt" { self.edit_prompt = Some(payload); return; }
                let ctx = Ctx { model: &self.model, theme: &self.theme };
                self.slots.broadcast(&ctx, &name, &payload);
            }
            Action::SwitchSession(session) => {
                if session == self.model.session { return; }
                let next = self.background_models.remove(&session)
                    .unwrap_or_else(|| Model::new(session, self.model.model_name.clone()));
                let previous = std::mem::replace(&mut self.model, next);
                if previous.busy || previous.live.is_some() {
                    self.background_models.insert(previous.session.clone(), previous);
                }
                self.reconcile();
            }
        }
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let ctx = Ctx { model: &self.model, theme: &self.theme };
        self.slots.render(&ctx, area, buf);
    }
}

fn edit_prompt(payload: &serde_json::Value) -> Result<String, String> {
    use std::io::Write;
    let argv = serde_json::from_value::<Vec<String>>(payload["editor"].clone()).ok()
        .or_else(|| std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).ok().map(|s| vec![s]));
    let argv = argv.ok_or("Configure an editor argv or set VISUAL/EDITOR to an executable")?;
    let program = argv.first().filter(|s| !s.is_empty()).ok_or("Editor command is empty")?;
    let mut file = tempfile::Builder::new().prefix("rness-prompt-").suffix(".txt").tempfile().map_err(|e| e.to_string())?;
    file.write_all(payload["text"].as_str().ok_or("Missing prompt text")?.as_bytes()).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    let status = std::process::Command::new(program).args(&argv[1..]).arg(file.path()).status().map_err(|e| e.to_string())?;
    if !status.success() { return Err(format!("editor exited with {status}")); }
    std::fs::read_to_string(file.path()).map_err(|e| e.to_string())
}

/// Run the full-screen TUI until quit. Owns the terminal.
/// `host_actions`: actions injected by the composition root (e.g. a Lua
/// app driver applying "session:switch") — same apply path as key-borne
/// actions.
pub async fn run(
    mut app: App,
    mut frames: mpsc::UnboundedReceiver<Frame>,
    mut approvals: mpsc::UnboundedReceiver<crate::modules::approval::PendingApproval>,
    mut host_actions: mpsc::UnboundedReceiver<Action>,
) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    // Wheel scroll for the transcript. Best-effort: a terminal without
    // mouse support just keeps keyboard scrolling.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture, crossterm::event::EnableBracketedPaste);
    app.reconcile();

    // Crossterm events on a blocking thread → channel.
    let (tx, mut term_events) = mpsc::unbounded_channel();
    let terminal_input = Arc::new(std::sync::Mutex::new(()));
    let reader_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_stop = reader_done.clone();
    let reader_lock = terminal_input.clone();
    std::thread::spawn(move || loop {
        if reader_stop.load(std::sync::atomic::Ordering::Relaxed) { return; }
        let _guard = reader_lock.lock().unwrap();
        match crossterm::event::poll(Duration::from_millis(20)) {
            Ok(false) => continue,
            Err(_) => return,
            Ok(true) => {}
        }
        match crossterm::event::read() {
            Ok(ev) => {
                if tx.send(ev).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    });

    let tick = Duration::from_millis(33); // ~30fps redraw budget
    let result = loop {
        // Coalesce: drain everything pending, then draw once.
        tokio::select! {
            ev = term_events.recv() => {
                let Some(ev) = ev else { break Ok(()) };
                app.on_term_event(ev);
                while let Ok(ev) = term_events.try_recv() {
                    app.on_term_event(ev);
                }
            }
            frame = frames.recv() => {
                if let Some(frame) = frame {
                    app.apply_frame(&frame);
                    while let Ok(frame) = frames.try_recv() {
                        app.apply_frame(&frame);
                    }
                }
            }
            pending = approvals.recv() => {
                if let Some(pending) = pending {
                    // Publish the question; the approval overlay selects
                    // itself into the overlay slot while this is Some.
                    app.model.pending_approval = Some(pending);
                }
            }
            action = host_actions.recv() => {
                if let Some(action) = action {
                    app.apply(action);
                    while let Ok(action) = host_actions.try_recv() {
                        app.apply(action);
                    }
                }
            }
            _ = tokio::time::sleep(tick) => {}
        }
        if let Some(payload) = app.edit_prompt.take() {
            // The reader must relinquish stdin before handing it to an editor.
            let _guard = terminal_input.lock().unwrap();
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture, crossterm::event::DisableBracketedPaste);
            ratatui::restore();
            let edited = edit_prompt(&payload);
            terminal = ratatui::init();
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture, crossterm::event::EnableBracketedPaste);
            match edited {
                Ok(text) => app.apply(Action::Custom("input:prompt-edited".into(), serde_json::json!({"text":text}))),
                Err(error) => app.apply(Action::Notice(format!("Editor failed; prompt unchanged: {error}"))),
            }
        }
        if app.model.should_quit {
            break Ok(());
        }
        let draw = terminal.draw(|f| {
            let area = f.area();
            app.render(area, f.buffer_mut());
        });
        if let Err(e) = draw {
            break Err(e);
        }
    };

    reader_done.store(true, std::sync::atomic::Ordering::Relaxed);
    let _guard = terminal_input.lock().unwrap();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste, crossterm::event::DisableMouseCapture);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rness_protocol::events::{
        AssistantMessage, Envelope, Header, StopReason, ToolResult, Usage, UserMessage,
        FORMAT_VERSION,
    };

    /// Backend that serves a canned history — the `-s` reopen case.
    struct FakeBackend {
        history: History,
    }

    impl Backend for FakeBackend {
        fn request(&self, _request: ClientRequest) {}
        fn history(&self, _session: &SessionId) -> History {
            self.history.clone()
        }
    }

    #[test]
    fn ctrl_c_cancels_admitted_command_before_any_turn_frame() {
        struct Pending;
        impl Backend for Pending {
            fn request(&self, _: ClientRequest) {}
            fn command_running(&self, _: &SessionId) -> bool { true }
            fn history(&self, _: &SessionId) -> History { prior_history("s") }
        }
        let mut app = App::new(Model::new("s".into(), "m".into()), Slots::default(), Arc::new(Pending));
        assert!(!app.model.busy);
        let actions = app.route_key(KeyEvent::new(crossterm::event::KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL));
        assert!(matches!(actions.as_slice(), [Action::Cancel]));
    }

    #[test]
    fn hidden_command_results_survive_switch_and_reconcile_without_duplicates() {
        let backend = Arc::new(FakeBackend { history: prior_history("s") });
        let mut app = App::new(Model::new("s".into(), "m".into()), Slots::default(), backend);
        app.apply(Action::CommandResult("other".into(), "saved result".into()));
        assert!(app.model.entries.is_empty());
        app.apply(Action::SwitchSession("other".into()));
        app.reconcile();
        assert_eq!(app.model.entries.iter().filter(|entry| **entry == Entry::Notice("saved result".into())).count(), 1);
        app.apply(Action::SwitchSession("s".into()));
        assert!(!app.model.entries.contains(&Entry::Notice("saved result".into())));
    }

    #[test]
    fn command_result_is_notice_without_user_echo() {
        struct CommandBackend;
        impl Backend for CommandBackend {
            fn request(&self, _: ClientRequest) { panic!("use submit"); }
            fn submit(&self, _: ClientRequest) -> Result<Option<String>, String> { Ok(Some("hello world".into())) }
            fn history(&self, _: &SessionId) -> History { prior_history("s") }
        }
        let mut app = App::new(Model::new("s".into(), "m".into()), Slots::default(), Arc::new(CommandBackend));
        app.apply(Action::Submit("/hello world".into()));
        assert_eq!(app.model.entries, vec![Entry::Notice("hello world".into())]);
    }

    #[test]
    fn rejected_submission_is_notice_not_optimistic_message() {
        struct Reject;
        impl Backend for Reject {
            fn request(&self, _: ClientRequest) { panic!("must use acknowledged submit"); }
            fn submit(&self, _: ClientRequest) -> Result<Option<String>, String> { Err("engine busy".into()) }
            fn history(&self, _: &SessionId) -> History { prior_history("s") }
        }
        let mut app = App::new(Model::new("s".into(), "m".into()), Slots::default(), Arc::new(Reject));
        app.apply(Action::Submit("hello".into()));
        assert_eq!(app.model.entries, vec![Entry::Notice("engine busy".into())]);
    }

    #[test]
    fn unload_command_routes_locally_without_a_user_message() {
        let backend = Arc::new(FakeBackend { history: prior_history("s") });
        let mut app = App::new(Model::new("s".into(), "m".into()), Slots::default(), backend);
        let apps = crate::modules::ext_apps::AppsState::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        apps.connect(tx);
        app.apps = Some(apps);
        app.apply(Action::Submit("/unload spinner".into()));
        assert_eq!(rx.try_recv().unwrap(), crate::modules::ext_apps::AppEvent::Unload("spinner".into()));
        assert!(app.model.entries.is_empty());
        app.apply(Action::Submit("/unload".into()));
        assert!(matches!(app.model.entries.last(), Some(Entry::Notice(_))));
        assert!(rx.try_recv().is_err());
    }

    fn env(event: SessionEvent) -> Envelope {
        Envelope { id: "01TEST".into(), at: "2026-01-01T00:00:00.000Z".into(), event }
    }

    #[test]
    fn editor_failure_preserves_original_and_success_reads_saved_file() {
        let payload = serde_json::json!({"text":"original\n", "editor":["/bin/sh", "-c", "printf 'edited\\n' > \"$1\"", "rness-editor"]});
        assert_eq!(edit_prompt(&payload).unwrap(), "edited\n");
        let failure = serde_json::json!({"text":"original", "editor":["/bin/sh", "-c", "exit 7"]});
        assert!(edit_prompt(&failure).unwrap_err().contains("7"));
        assert_eq!(failure["text"], "original");
    }

    #[test]
    fn provider_errors_are_visible_after_history_reload() {
        use rness_protocol::events::{AssistantAttempt, AttemptOutcome};
        let mut model = Model::new("s".into(), "fake".into());
        let history = History { session: "s".into(), envelopes: vec![env(SessionEvent::AssistantAttempt(AssistantAttempt {
            model: "fake".into(), chunks: vec![], outcome: AttemptOutcome::Error { code: None, retry_in_ms: None, message: "connection closed before message_stop".into(), retryable: true },
        }))] };
        model.load_history(&history);
        assert!(matches!(&model.entries[0], Entry::Notice(text) if text.contains("Provider error (fake)") && text.contains("connection closed")));
        model.load_history(&history);
        assert_eq!(model.entries.len(), 1);
    }

    fn prior_history(session: &str) -> History {
        History {
            session: session.into(),
            envelopes: vec![
                env(SessionEvent::Header(Header {
                    version: FORMAT_VERSION,
                    session: session.into(),
                    parent: None,
                    delegation: None,
                    workspace: None,
                })),
                env(SessionEvent::UserMessage(UserMessage {
                    intent: UserIntent::Followup,
                    content: vec![ContentPart::Text { text: "crea foo.txt".into() }],
                    source: None,
                })),
                env(SessionEvent::AssistantMessage(AssistantMessage {
                    model: "claude-sonnet-4-5".into(),
                    content: vec![ContentPart::ToolUse {
                        call: "c1".into(),
                        name: "Write".into(),
                        args: serde_json::json!({"path": "foo.txt"}),
                    }],
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                    chunks: vec![],
                })),
                env(SessionEvent::ToolResult(ToolResult {
                    content: vec![], tasks: None, plan_review: None, presentation: None,
                    call: "c1".into(),
                    name: "Write".into(),
                    output: "wrote foo.txt".into(),
                    is_error: false,
                    duration_ms: 3,
                })),
                env(SessionEvent::AssistantMessage(AssistantMessage {
                    model: "claude-sonnet-4-5".into(),
                    content: vec![ContentPart::Text { text: "listo".into() }],
                    stop: StopReason::EndTurn,
                    usage: Usage::default(),
                    chunks: vec![],
                })),
            ],
        }
    }

    #[test]
    fn retries_interruptions_and_delayed_reconcile_keep_distinct_identities() {
        let mut model = Model::new("s".into(), "fake".into());
        let start = Frame::StepStarted {session:"s".into(),turn:1};
        model.apply_frame(&start);
        let failed = model.live_assistant_id.unwrap();
        model.apply_frame(&start);
        let retried = model.live_assistant_id.unwrap();
        assert_ne!(failed, retried);
        model.apply_frame(&Frame::TurnIdle {session:"s".into()});
        assert_eq!(model.live_assistant_id, None);
        model.apply_frame(&start);
        let committed = model.live_assistant_id.unwrap();
        assert_ne!(retried, committed);
        model.apply_frame(&Frame::StepCommitted {session:"s".into(),event:"target".into()});
        model.apply_frame(&Frame::TurnIdle {session:"s".into()});
        let mut history = prior_history("s");
        for (index, envelope) in history.envelopes.iter_mut().enumerate() { envelope.id = format!("older-{index}"); }
        history.envelopes[4].id = "target".into();
        model.load_history(&history);
        assert_eq!(model.assistant_ids[&3], committed);
        assert_ne!(model.assistant_ids[&1], committed);
        model.load_history(&history);
        assert_eq!(model.assistant_ids[&3], committed);
    }

    #[test]
    fn assistant_identity_survives_removed_history_and_new_commits() {
        let mut model = Model::new("s".into(), "fake".into());
        let mut history = prior_history("s");
        for (index, envelope) in history.envelopes.iter_mut().enumerate() { envelope.id = format!("event-{index}"); }
        model.load_history(&history);
        let retained = model.assistant_ids[&3];
        history.envelopes.remove(2);
        model.load_history(&history);
        assert_eq!(model.assistant_ids[&2], retained);
        let pending = model.next_assistant_id;
        history.envelopes.push(env(SessionEvent::AssistantMessage(AssistantMessage {
            model:"fake".into(), content:vec![ContentPart::Thinking {text:"new".into(),signature:None}],
            stop:StopReason::EndTurn,usage:Usage::default(),chunks:vec![],
        })));
        model.load_history(&history);
        assert_eq!(model.assistant_ids[&3], pending);
        assert_ne!(pending, retained);
        model.load_history(&history);
        assert_eq!(model.assistant_ids[&2], retained);
        assert_eq!(model.assistant_ids[&3], pending);
    }

    #[test]
    fn append_projection_visits_only_new_events_and_rebuilds_compaction() {
        let mut model = Model::new("s1".into(), "fake".into());
        let mut history = prior_history("s1");
        for (i, event) in history.envelopes.iter_mut().enumerate() { event.id = format!("event-{i}"); }
        model.load_history(&history);
        let visits = model.projection_visits;
        model.load_history(&history);
        assert_eq!(model.projection_visits, visits);
        let mut added = history.envelopes[1].clone();
        added.id = "appended".into();
        history.envelopes.push(added);
        model.load_history(&history);
        assert_eq!(model.projection_visits, visits + 1);
        let mut fresh = Model::new("s1".into(), "fake".into());
        fresh.load_history(&history);
        assert_eq!(model.entries, fresh.entries);
        assert_eq!(model.entry_ids, fresh.entry_ids);
        assert_eq!(model.tool_durations, fresh.tool_durations);
        let mut checkpoint = env(SessionEvent::Compaction(rness_protocol::events::Compaction {
            replaces: vec!["event-1".into()], summary: "summary".into(), model: "fake".into(),
        }));
        checkpoint.id = "checkpoint".into();
        history.envelopes.push(checkpoint);
        model.load_history(&history);
        assert_eq!(model.projection_visits, visits + 1 + history.envelopes.len());
        fresh = Model::new("s1".into(), "fake".into());
        fresh.load_history(&history);
        assert_eq!(model.entries, fresh.entries);
        assert_eq!(model.entry_ids, fresh.entry_ids);
        history.session = "s2".into();
        let visits = model.projection_visits;
        model.load_history(&history);
        assert_eq!(model.projection_visits, visits + history.envelopes.len());
    }

    #[test]
    fn switching_back_restores_stream_including_background_deltas() {
        let backend = Arc::new(FakeBackend { history: prior_history("s1") });
        let mut app = App::new(Model::new("s1".into(), "fake".into()), Slots::default(), backend);
        app.model.busy = true;
        app.model.live_assistant_id = Some(42);
        app.model.live = Some(LiveStep { text: "before ".into(), ..Default::default() });
        app.apply(Action::SwitchSession("s2".into()));
        app.apply_frame(&Frame::Delta { session: "s1".into(), chunk: ChunkDelta::Text { t: "during".into() } });
        assert!(app.model.live.is_none());
        app.apply(Action::SwitchSession("s1".into()));
        assert!(app.model.busy);
        assert_eq!(app.model.live_assistant_id, Some(42));
        assert_eq!(app.model.live.as_ref().unwrap().text, "before during");
        app.apply(Action::SwitchSession("s2".into()));
        app.apply_frame(&Frame::TurnIdle { session: "s1".into() });
        app.apply(Action::SwitchSession("s1".into()));
        assert!(!app.model.busy);
        assert!(app.model.live.is_none());
    }

    #[test]
    fn reconcile_uses_delta_without_fetching_full_history() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct DeltaBackend { full: AtomicUsize, delta: AtomicUsize }
        impl Backend for DeltaBackend {
            fn request(&self, _: ClientRequest) {}
            fn history(&self, _: &SessionId) -> History {
                assert_eq!(self.full.fetch_add(1, Ordering::SeqCst), 0);
                prior_history("s1")
            }
            fn history_after(&self, _: &SessionId, _: &str) -> Option<Vec<rness_protocol::events::Envelope>> {
                if self.delta.fetch_add(1, Ordering::SeqCst) == 0 {
                    let mut event = prior_history("s1").envelopes[1].clone();
                    event.id = "new-event".into();
                    Some(vec![event])
                } else { Some(vec![]) }
            }
        }
        let backend = Arc::new(DeltaBackend { full: AtomicUsize::new(0), delta: AtomicUsize::new(0) });
        let mut app = App::new(Model::new("s1".into(), "fake".into()), Slots::default(), backend.clone());
        app.reconcile();
        let visits = app.model.projection_visits;
        app.reconcile();
        assert_eq!(app.model.projection_visits, visits + 1);
        app.reconcile();
        assert_eq!(app.model.projection_visits, visits + 1);
        assert_eq!(backend.full.load(Ordering::SeqCst), 1);
        assert_eq!(backend.delta.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn unchanged_reconcile_retains_render_revision() {
        let mut model = Model::new("s1".into(), "fake".into());
        let history = prior_history("s1");
        model.load_history(&history);
        let revision = model.history_revision;
        model.load_history(&history);
        assert_eq!(model.history_revision, revision);
        model.entries.push(Entry::Notice("temporary".into()));
        model.load_history(&history);
        assert_eq!(model.history_revision, revision + 1);
    }

    #[test]
    fn reopened_session_hydrates_entries_from_history() {
        let backend = Arc::new(FakeBackend { history: prior_history("s1") });
        let mut app = App::new(
            Model::new("s1".into(), "claude-sonnet-4-5".into()),
            Slots::default(),
            backend,
        );
        assert!(app.model.entries.is_empty());

        app.reconcile(); // what run() does on startup

        assert_eq!(app.model.entries.len(), 4);
        assert!(matches!(&app.model.entries[0], Entry::User { .. }));
        assert!(matches!(&app.model.entries[1], Entry::Assistant { .. }));
        let Entry::ToolResult { name, output, .. } = &app.model.entries[2] else {
            panic!("expected tool result: {:?}", app.model.entries[2]);
        };
        assert_eq!(name, "Write");
        assert_eq!(output, "wrote foo.txt");
        let Entry::Assistant { content, .. } = &app.model.entries[3] else {
            panic!("expected assistant: {:?}", app.model.entries[3]);
        };
        assert_eq!(content, &vec![ContentPart::Text { text: "listo".into() }]);
    }

    #[test]
    fn pending_approval_routes_keys_to_overlay_and_resolves() {
        use crate::modules::approval::{self, PendingApproval};
        use crossterm::event::{KeyCode, KeyEvent};
        use rness_protocol::api::{ApprovalDecision, ApprovalRequest};

        let backend = Arc::new(FakeBackend { history: prior_history("s1") });
        let mut slots = Slots::default();
        crate::modules::input::install(&mut slots);
        approval::install(&mut slots);
        let mut app = App::new(Model::new("s1".into(), "m".into()), slots, backend);

        // No question pending: keys go to the input editor, not the overlay.
        let actions = app.route_key(KeyEvent::from(KeyCode::Char('y')));
        assert!(actions.is_empty(), "editor consumes typing: {actions:?}");

        // Publish a question — the overlay selects itself (chain pattern).
        let (respond, mut answered) = tokio::sync::oneshot::channel();
        app.model.pending_approval = Some(PendingApproval {
            request: ApprovalRequest {
                session: "s1".into(),
                call: "c1".into(),
                tool: "Bash".into(),
                args: serde_json::json!({"command": "rm -rf /tmp/x"}),
            },
            respond,
        });

        let actions = app.route_key(KeyEvent::from(KeyCode::Char('y')));
        assert!(
            matches!(actions[..], [Action::ResolveApproval(ApprovalDecision::Allowed)]),
            "overlay owns focus while pending: {actions:?}"
        );
        for a in actions {
            app.apply(a);
        }
        assert!(app.model.pending_approval.is_none(), "resolved clears the question");
        assert_eq!(answered.try_recv(), Ok(ApprovalDecision::Allowed));

        // Gone: focus falls back to the editor.
        let actions = app.route_key(KeyEvent::from(KeyCode::Char('y')));
        assert!(actions.is_empty());
    }
}

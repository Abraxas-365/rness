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
    fn request(&self, request: ClientRequest);
    fn submit(&self, request: ClientRequest) -> Result<bool, String> {
        self.request(request);
        Ok(true)
    }
    fn history(&self, session: &SessionId) -> History;
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
            live: None,
            busy: false,
            model_name,
            scroll_from_bottom: 0,
            pending_approval: None,
            should_quit: false,
        }
    }

    /// Rebuild entries from durable history (the reconcile point).
    pub fn load_history(&mut self, history: &History) {
        self.entries.clear();
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
        for env in &history.envelopes {
            if shadowed.contains(env.id.as_str()) {
                if let Some(span) = anchors.get(env.id.as_str()) {
                    self.entries.push(Entry::Notice(format!(
                        "── {span} earlier events compacted into a summary ──"
                    )));
                }
                continue;
            }
            match &env.event {
                SessionEvent::UserMessage(m) => {
                    self.entries.push(Entry::User { content: m.content.clone() })
                }
                SessionEvent::AssistantMessage(m) => {
                    for part in &m.content {
                        if let ContentPart::ToolUse { call, name, .. } = part {
                            names.insert(call.clone(), name.clone());
                        }
                    }
                    self.entries.push(Entry::Assistant {
                        model: m.model.clone(),
                        content: m.content.clone(),
                    });
                }
                SessionEvent::ToolResult(r) => self.entries.push(Entry::ToolResult {
                    call: r.call.clone(),
                    name: names.get(&r.call).cloned().unwrap_or_default(),
                    output: r.output.clone(),
                    is_error: r.is_error,
                }),
                _ => {}
            }
        }
    }

    /// Apply one live frame.
    pub fn apply_frame(&mut self, frame: &Frame) -> FrameEffect {
        match frame {
            Frame::StepStarted { .. } => {
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
            Frame::StepCommitted { .. } => {
                // Durable now — reload from the log and drop the stream state.
                self.live = None;
                FrameEffect::Reconcile
            }
            Frame::TurnIdle { .. } => {
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
            backend,
        }
    }

    /// Load durable history into the model.
    pub fn reconcile(&mut self) {
        let history = self.backend.history(&self.model.session);
        self.model.load_history(&history);
    }

    pub fn apply_frame(&mut self, frame: &Frame) {
        if self.model.apply_frame(frame) == FrameEffect::Reconcile {
            self.reconcile();
        }
    }

    /// Route a terminal event; apply resulting actions.
    pub fn on_term_event(&mut self, event: TermEvent) {
        match event {
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
        // Host keymap: the rebindable chord→action table (stock seeded
        // in keymaps.rs, rewritten live by rness.keymaps).
        use crate::keymaps::HostAction;
        match self.keymap.lookup(&key) {
            Some(HostAction::CancelOrQuit) => {
                if self.model.busy {
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
            Action::Submit(text) => {
                if text.trim().is_empty() {
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
                    Ok(true) => self.model.entries.push(Entry::User { content }),
                    Ok(false) => self.model.entries.push(Entry::Notice("Command completed".into())),
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
            Action::Notice(text) => self.model.entries.push(Entry::Notice(text)),
            Action::Custom(name, payload) => {
                let ctx = Ctx { model: &self.model, theme: &self.theme };
                self.slots.broadcast(&ctx, &name, &payload);
            }
            Action::SwitchSession(session) => {
                let model_name = self.model.model_name.clone();
                self.model = Model::new(session, model_name);
                self.reconcile();
            }
        }
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let ctx = Ctx { model: &self.model, theme: &self.theme };
        self.slots.render(&ctx, area, buf);
    }
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
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    app.reconcile();

    // Crossterm events on a blocking thread → channel.
    let (tx, mut term_events) = mpsc::unbounded_channel();
    std::thread::spawn(move || loop {
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
    fn rejected_submission_is_notice_not_optimistic_message() {
        struct Reject;
        impl Backend for Reject {
            fn request(&self, _: ClientRequest) { panic!("must use acknowledged submit"); }
            fn submit(&self, _: ClientRequest) -> Result<bool, String> { Err("engine busy".into()) }
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

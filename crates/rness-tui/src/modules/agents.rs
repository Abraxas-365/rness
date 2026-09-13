//! Read-only agent monitor. The host publishes protocol data; this module never
//! requests engine actions. Each session has isolated model, cards and chat state.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
};
use rness_protocol::{api::History, events::Envelope, frames::Frame};

use crate::app::{Action, Model};
use crate::component::{Component, Ctx, KeyOutcome};
use crate::modules::{chat::Chat, tool_cards::CardCache};
use crate::slots::{Slots, OVERLAY};

const MAX_SESSIONS: usize = 256;
const MAX_LIVE_BYTES: usize = 256 * 1024;
const MAX_LIVE_TOOLS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentInfo {
    pub session: String,
    pub alias: String,
    pub parent: Option<String>,
    pub depth: usize,
    pub name: String,
    pub task: String,
    pub mode: String,
    pub status: String,
    pub elapsed_ms: u64,
    pub call: Option<String>,
}

struct Session {
    model: Model,
    history: History,
    cards: CardCache,
    generation: u64,
    activity: u64,
    seen: u64,
    commits: VecDeque<String>,
}

impl Session {
    fn reconcile(&mut self) {
        let revision = self.model.history_revision;
        // A store write can precede its synchronous frame callback. Do not
        // project an unacknowledged assistant alongside its live stream. Keep
        // the full host cursor/history, deferring only display projection.
        let deferred = self.model.live.as_ref().and_then(|live| {
            // With no initial cursor, older assistant IDs need not have been
            // observed by this process. Only the newest assistant can be the
            // pending commit, and it must reproduce the active stream.
            let (index, event, message) = self.history.envelopes.iter().enumerate().rev().find_map(|(index, event)| {
                if let rness_protocol::events::SessionEvent::AssistantMessage(message) = &event.event {
                    Some((index, event, message))
                } else { None }
            })?;
            if self.model.entry_ids.contains(&event.id) || self.commits.contains(&event.id) { return None; }
            let mut text = String::new();
            let mut thinking = String::new();
            for part in &message.content {
                match part {
                    rness_protocol::events::ContentPart::Text { text: value } => text.push_str(value),
                    rness_protocol::events::ContentPart::Thinking { text: value, .. } => thinking.push_str(value),
                    _ => {}
                }
            }
        let matches_tools = !live.tool_args.is_empty() && live.tool_args.iter().all(|(call, _)| message.content.iter().any(|part| {
                matches!(part, rness_protocol::events::ContentPart::ToolUse { call: id, .. } if id == call)
            }));
            let matches = (!live.text.is_empty() || !live.thinking.is_empty() || matches_tools)
                && (live.text.is_empty() || text.ends_with(&live.text))
                && (live.thinking.is_empty() || thinking.ends_with(&live.thinking));
            matches.then_some(index)
        });
        if let Some(end) = deferred {
            self.model.load_history(&History {
                session: self.history.session.clone(),
                envelopes: self.history.envelopes[..end].to_vec(),
            });
        } else {
            self.model.load_history(&self.history);
        }
        if self.model.history_revision != revision {
            self.activity = self.activity.wrapping_add(1);
        }
        // ToolOutput can be incremental. Only durable results prove completion.
        if let Some(live) = &mut self.model.live {
            let completed: std::collections::HashSet<_> = self
                .model
                .entries
                .iter()
                .filter_map(|entry| {
                    if let crate::app::Entry::ToolResult { call, .. } = entry {
                        Some(call)
                    } else {
                        None
                    }
                })
                .collect();
            live.running_tools
                .retain(|(call, _)| !completed.contains(call));
            live.tool_args.retain(|(call, _)| !completed.contains(call));
        }
    }
}

#[derive(Default)]
struct Monitor {
    requested: Option<(String, Option<String>)>,
    root: String,
    agents: Vec<AgentInfo>,
    sessions: HashMap<String, Session>,
    recent: VecDeque<String>,
    generation: u64,
    pending_inspect: Option<(String, Option<String>, Option<String>)>,
}

impl Monitor {
    fn resolve_inspect(&mut self) {
        let Some((parent, call, target)) = &self.pending_inspect else {
            return;
        };
        let selected = self
            .agents
            .iter()
            .find(|agent| {
                if let Some(target) = target {
                    &agent.session == target
                } else {
                    agent.parent.as_ref() == Some(parent) && call.is_some() && &agent.call == call
                }
            })
            .map(|agent| agent.session.clone());
        if let Some(selected) = selected {
            if let Some((_, detail)) = &mut self.requested {
                *detail = Some(selected);
            }
            self.pending_inspect = None;
        }
    }

    fn inspect(&mut self, root: &str, parent: &str, call: Option<String>, target: Option<String>) {
        if self.root != root {
            self.root = root.to_owned();
            self.agents.clear();
        }
        self.requested = Some((root.to_owned(), None));
        self.pending_inspect = Some((parent.to_owned(), call, target));
        self.resolve_inspect();
    }

    fn session(&mut self, id: &str) -> &mut Session {
        self.recent.retain(|s| s != id);
        self.recent.push_back(id.to_owned());
        if !self.sessions.contains_key(id) {
            while self.sessions.len() >= MAX_SESSIONS {
                let Some(oldest) = self.recent.pop_front() else {
                    break;
                };
                if self.requested.as_ref().and_then(|(_, s)| s.as_deref()) == Some(&oldest) {
                    self.recent.push_back(oldest);
                    continue;
                }
                self.sessions.remove(&oldest);
            }
            self.generation = self.generation.wrapping_add(1);
            self.sessions.insert(
                id.to_owned(),
                Session {
                    model: Model::new(id.to_owned(), String::new()),
                    history: History {
                        session: id.to_owned(),
                        envelopes: Vec::new(),
                    },
                    cards: CardCache::default(),
                    generation: self.generation,
                    activity: 0,
                    seen: 0,
                    commits: VecDeque::new(),
                },
            );
        }
        self.sessions.get_mut(id).expect("session inserted")
    }
}

/// Cloneable host bridge. Locks cover publication, projection and rendering.
/// Up to 256 recently observed/hydrated sessions are retained; selected detail
/// is pinned. Durable history is not truncated. Live text/args are bounded.
#[derive(Clone, Default)]
pub struct AgentMonitorState(Arc<Mutex<Monitor>>);

impl AgentMonitorState {
    /// Current drawer root and optional detail session; None means closed.
    pub fn requested(&self) -> Option<(String, Option<String>)> {
        self.0.lock().expect("agent monitor lock").requested.clone()
    }

    pub fn publish_agents(&self, root: impl Into<String>, agents: Vec<AgentInfo>) {
        let mut state = self.0.lock().expect("agent monitor lock");
        let root = root.into();
        // Do not let a delayed refresh replace another root's active list.
        if state.requested.as_ref().is_some_and(|(r, _)| r != &root) {
            return;
        }
        state.root = root;
        state.agents = agents;
        state.resolve_inspect();
    }

    /// Call IDs are only session-local. Never hydrate a child into main cards.
    pub fn cards(&self, session: &str) -> CardCache {
        self.0
            .lock()
            .expect("agent monitor lock")
            .session(session)
            .cards
            .clone()
    }

    /// Invalidate every retained session cache on renderer unload/reload.
    /// Outstanding publishers must use CardCache::insert_if_current.
    pub fn invalidate_cards(&self) {
        for target in self.0.lock().expect("agent monitor lock").sessions.values() {
            target.cards.invalidate();
        }
    }

    pub fn publish_history(&self, session: &str, history: History) {
        if history.session != session {
            return;
        }
        let mut state = self.0.lock().expect("agent monitor lock");
        let target = state.session(session);
        target.history = history;
        target.reconcile();
    }

    /// Serialize a snapshot read/publication with frame observation. The closure
    /// must not reenter this handle or synchronously emit frames (it runs locked).
    /// Ordinary refreshes preserve live state: only a commit/idle frame clears it.
    pub fn with_history(&self, session: &str, read: impl FnOnce() -> History) {
        let mut state = self.0.lock().expect("agent monitor lock");
        let history = read();
        if history.session != session {
            return;
        }
        let target = state.session(session);
        target.history = history;
        target.reconcile();
    }

    pub fn history_cursor(&self, session: &str) -> Option<String> {
        self.0
            .lock()
            .expect("agent monitor lock")
            .sessions
            .get(session)?
            .history
            .envelopes
            .last()
            .map(|event| event.id.clone())
    }

    /// Append only if the caller's cursor still matches. False requests a full
    /// snapshot (including after retention eviction). Compaction is reprojected
    /// by Model::load_history, preserving normal Chat identity/cache semantics.
    pub fn publish_history_after(&self, session: &str, after: &str, events: Vec<Envelope>) -> bool {
        let mut state = self.0.lock().expect("agent monitor lock");
        let Some(target) = state.sessions.get_mut(session) else {
            return false;
        };
        if target.history.envelopes.last().map(|e| e.id.as_str()) != Some(after) {
            return false;
        }
        target.history.envelopes.extend(events);
        target.reconcile();
        true
    }

    /// Append an already serialized host delta. Prefer publish_history_after
    /// when multiple refreshes can run concurrently.
    pub fn append_history(&self, session: &str, events: Vec<Envelope>) {
        let mut state = self.0.lock().expect("agent monitor lock");
        let target = state.session(session);
        let known: std::collections::HashSet<_> = target
            .history
            .envelopes
            .iter()
            .map(|e| e.id.clone())
            .collect();
        target
            .history
            .envelopes
            .extend(events.into_iter().filter(|e| !known.contains(&e.id)));
        target.reconcile();
    }

    /// Observe every frame, including before the first open. ToolOutput is
    /// rendered through host-published session cards rather than durable entries.
    pub fn observe(&self, frame: &Frame) {
        let session = match frame {
            Frame::StepStarted { session, .. }
            | Frame::ContextUsage { session, .. }
            | Frame::Delta { session, .. }
            | Frame::ToolStarted { session, .. }
            | Frame::ToolOutput { session, .. }
            | Frame::StepCommitted { session, .. }
            | Frame::TurnIdle { session }
            | Frame::CompactionStarted { session, .. }
            | Frame::CompactionFinished { session, .. }
            | Frame::HistoryChanged { session }
            | Frame::ApprovalRequested { session, .. }
            | Frame::ApprovalResolved { session, .. } => session,
        };
        let mut state = self.0.lock().expect("agent monitor lock");
        let target = state.session(session);
        target.model.apply_frame(frame);
        if let Frame::StepCommitted { event, .. } = frame {
            target.commits.push_back(event.clone());
            if target.commits.len() > 256 {
                target.commits.pop_front();
            }
        }
        if matches!(
            frame,
            Frame::StepCommitted { .. } | Frame::TurnIdle { .. } | Frame::ToolStarted { .. }
        ) {
            target.reconcile();
        }
        target.activity = target.activity.wrapping_add(1);
        if let Some(live) = &mut target.model.live {
            trim_live(&mut live.text, MAX_LIVE_BYTES);
            trim_live(&mut live.thinking, MAX_LIVE_BYTES);
            live.tool_args.truncate(MAX_LIVE_TOOLS);
            live.running_tools.truncate(MAX_LIVE_TOOLS);
            for (_, args) in &mut live.tool_args {
                trim_live(args, MAX_LIVE_BYTES / MAX_LIVE_TOOLS);
            }
        }
    }
}

fn trim_live(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    *text = text[start..].to_owned();
}

fn elapsed(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60)
    }
}

fn status_style(status: &str, theme: &crate::theme::Theme) -> ratatui::style::Style {
    match status {
        "failed" | "error" => theme.error,
        "completed" | "done" | "idle" => theme.added,
        "running" => theme.statusline_accent,
        _ => theme.dim,
    }
}

/// A fixed cell-width column, clipping whole graphemes rather than bytes.
fn column(text: &str, width: usize) -> String {
    let line = Line::raw(text.replace(['\n', '\r', '\t'], " "));
    let mut out = String::new();
    let mut used = 0;
    for span in &line.spans {
        for grapheme in span.styled_graphemes(ratatui::style::Style::default()) {
            let cells = unicode_width::UnicodeWidthStr::width(grapheme.symbol);
            if used + cells > width {
                break;
            }
            out.push_str(grapheme.symbol);
            used += cells;
        }
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// Mount below approvals (10) and extension questions (5). The main card cache
/// argument is deliberately not shared: provider call IDs can collide.
pub fn install(slots: &mut Slots, _cards: CardCache) -> AgentMonitorState {
    let state = AgentMonitorState::default();
    // Monitoring must never shadow an approval, even with custom priorities.
    slots.mount(OVERLAY, i32::MIN, Box::new(Agents::new(state.clone())));
    state
}

struct Agents {
    state: AgentMonitorState,
    chats: HashMap<String, (u64, Chat)>,
    config: serde_json::Value,
    selected: Option<String>,
    list_offset: usize,
    metadata_scroll: HashMap<String, u16>,
}

impl Agents {
    fn new(state: AgentMonitorState) -> Self {
        Self {
            state,
            chats: HashMap::new(),
            config: serde_json::json!({"tool":{"display":"preview"},"user":{"label":{"text":"Incoming message"}},"keys":{"selection_editor":false}}),
            selected: None,
            list_offset: 0,
            metadata_scroll: HashMap::new(),
        }
    }

    fn chat<'a>(
        chats: &'a mut HashMap<String, (u64, Chat)>,
        config: &serde_json::Value,
        target: &Session,
        theme: &crate::theme::Theme,
    ) -> &'a mut Chat {
        if chats
            .get(&target.model.session)
            .is_some_and(|(generation, _)| *generation != target.generation)
        {
            chats.remove(&target.model.session);
        }
        &mut chats
            .entry(target.model.session.clone())
            .or_insert_with(|| {
                let mut chat = Chat::with_cards(target.cards.clone());
                chat.on_action(
                    &Ctx {
                        model: &target.model,
                        theme,
                    },
                    "chat:messagebox-config",
                    config,
                );
                (target.generation, chat)
            })
            .1
    }

    fn navigate(&mut self, delta: isize) {
        let mut state = self.state.0.lock().expect("agent monitor lock");
        if state.agents.is_empty() {
            return;
        }
        let index = state
            .agents
            .iter()
            .position(|a| Some(&a.session) == self.selected.as_ref())
            .unwrap_or(0);
        let index = index
            .saturating_add_signed(delta)
            .min(state.agents.len() - 1);
        self.selected = Some(state.agents[index].session.clone());
        if let Some((_, detail)) = &mut state.requested {
            if detail.is_some() {
                *detail = self.selected.clone();
            }
        }
    }
}

impl Component for Agents {
    fn name(&self) -> &str {
        "agents"
    }
    fn captures_input(&self) -> bool {
        true
    }
    fn height(&self, _: &Ctx<'_>, _: u16) -> Option<u16> {
        let state = self.state.0.lock().expect("agent monitor lock");
        if state
            .requested
            .as_ref()
            .is_some_and(|(_, detail)| detail.is_some())
        {
            Some(u16::MAX)
        } else {
            Some(
                state
                    .agents
                    .len()
                    .max(1)
                    .saturating_add(3)
                    .min(usize::from(u16::MAX)) as u16,
            )
        }
    }
    fn wants(&self, ctx: &Ctx<'_>) -> bool {
        let mut state = self.state.0.lock().expect("agent monitor lock");
        if state
            .requested
            .as_ref()
            .is_some_and(|(root, _)| root != &ctx.model.session)
        {
            state.requested = None;
            state.pending_inspect = None;
        }
        state.requested.is_some()
    }
    fn binding_help(&self) -> Vec<String> {
        vec!["Agents (read-only): ↑/↓ list; Enter detail/tools; Tab next agent; Shift+Tab previous; PgUp/PgDn scroll; Alt+PgUp/PgDn full metadata; End follow; Alt+M select/copy; Esc detail → list → close".into()]
    }

    fn on_action(&mut self, ctx: &Ctx<'_>, name: &str, payload: &serde_json::Value) {
        match name {
            "viewport:wheel" => {
                let Some((root, detail)) = self.state.requested() else {
                    return;
                };
                if root != ctx.model.session {
                    return;
                }
                let up = payload["up"].as_bool().unwrap_or(false);
                if let Some(session) = detail {
                    let mut state = self.state.0.lock().expect("agent monitor lock");
                    if state.root != root || !state.agents.iter().any(|a| a.session == session) {
                        return;
                    }
                    let scroll = &mut state.session(&session).model.scroll_from_bottom;
                    *scroll = if up {
                        scroll.saturating_add(3)
                    } else {
                        scroll.saturating_sub(3)
                    };
                } else {
                    self.navigate(if up { -1 } else { 1 });
                }
            }
            "agents:inspect" => {
                let Some(parent) = payload["session"]
                    .as_str()
                    .filter(|s| *s == ctx.model.session)
                else {
                    return;
                };
                let mut state = self.state.0.lock().expect("agent monitor lock");
                state.inspect(
                    parent,
                    parent,
                    payload["call"].as_str().map(str::to_owned),
                    payload["target"].as_str().map(str::to_owned),
                );
            }
            "agents:open" => {
                let Some(root) = payload["session"]
                    .as_str()
                    .filter(|s| *s == ctx.model.session)
                else {
                    return;
                };
                let selected = payload["agent"].as_str().map(str::to_owned);
                let mut state = self.state.0.lock().expect("agent monitor lock");
                state.pending_inspect = None;
                state.requested = Some((root.to_owned(), selected.clone()));
                if state.root != root {
                    state.agents.clear();
                    state.root = root.to_owned();
                }
                self.selected =
                    selected.or_else(|| state.agents.first().map(|a| a.session.clone()));
                self.list_offset = 0;
            }
            "chat:messagebox-config" => {
                self.config = if payload.is_object() {
                    payload.clone()
                } else {
                    serde_json::json!({})
                };
                // Child input includes principal assignments and forwarded messages,
                // not necessarily text authored by the main conversation's user.
                if !self.config["user"].is_object() {
                    self.config["user"] = serde_json::json!({});
                }
                if !self.config["user"]["label"].is_object() {
                    self.config["user"]["label"] = serde_json::json!({});
                }
                self.config["user"]["label"]["text"] = "Incoming message".into();
                // Also suppress the editor hint and key when selecting messages.
                if !self.config["keys"].is_object() {
                    self.config["keys"] = serde_json::json!({});
                }
                self.config["keys"]["selection_editor"] = false.into();
                self.state.invalidate_cards();
                for (_, chat) in self.chats.values_mut() {
                    chat.on_action(ctx, name, &self.config);
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, ctx: &Ctx<'_>, key: KeyEvent) -> KeyOutcome {
        if key.kind == KeyEventKind::Release {
            return KeyOutcome::consumed();
        }
        let Some((_, detail)) = self.state.requested() else {
            return KeyOutcome::consumed();
        };
        if key.code == KeyCode::Esc {
            let mut state = self.state.0.lock().expect("agent monitor lock");
            if detail.is_some() {
                if let Some((_, detail)) = &mut state.requested {
                    *detail = None;
                }
            } else {
                state.requested = None;
                state.pending_inspect = None;
            }
            return KeyOutcome::consumed();
        }
        if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
            self.navigate(
                if key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT) {
                    -1
                } else {
                    1
                },
            );
            return KeyOutcome::consumed();
        }
        let Some(session) = detail else {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.navigate(-1),
                KeyCode::Down | KeyCode::Char('j') => self.navigate(1),
                KeyCode::Enter => {
                    let mut state = self.state.0.lock().expect("agent monitor lock");
                    let selected = self
                        .selected
                        .clone()
                        .or_else(|| state.agents.first().map(|a| a.session.clone()));
                    if let Some((_, detail)) = &mut state.requested {
                        *detail = selected;
                    }
                }
                _ => {}
            }
            return KeyOutcome::consumed();
        };
        if key.modifiers == KeyModifiers::ALT {
            let offset = self.metadata_scroll.entry(session.clone()).or_default();
            match key.code {
                KeyCode::PageUp => {
                    *offset = offset.saturating_sub(3);
                    return KeyOutcome::consumed();
                }
                KeyCode::PageDown => {
                    *offset = offset.saturating_add(3);
                    return KeyOutcome::consumed();
                }
                _ => {}
            }
        }
        let mut state = self.state.0.lock().expect("agent monitor lock");
        if state.root != ctx.model.session || !state.agents.iter().any(|a| a.session == session) {
            return KeyOutcome::consumed();
        }
        let target = state.session(&session);
        let chat = Self::chat(&mut self.chats, &self.config, target, ctx.theme);
        let scroll = &mut target.model.scroll_from_bottom;
        match key.code {
            KeyCode::PageUp => *scroll = scroll.saturating_add(10),
            KeyCode::PageDown => *scroll = scroll.saturating_sub(10),
            KeyCode::End => *scroll = 0,
            KeyCode::Up if key.modifiers.is_empty() && !chat.captures_input() => {
                *scroll = scroll.saturating_add(1)
            }
            KeyCode::Down if key.modifiers.is_empty() && !chat.captures_input() => {
                *scroll = scroll.saturating_sub(1)
            }
            _ => {
                let child_ctx = Ctx {
                    model: &target.model,
                    theme: ctx.theme,
                };
                let outcome = if !chat.captures_input()
                    && (key.code == KeyCode::Enter
                        || (key.code == KeyCode::Char('o')
                            && key.modifiers == KeyModifiers::CONTROL))
                {
                    chat.on_binding(&child_ctx, "toggle_tool")
                } else {
                    chat.on_key(&child_ctx, key)
                };
                let mut allowed = Vec::new();
                let mut inspect = None;
                for action in outcome.actions {
                    match action {
                        Action::ScrollUp(n) => {
                            target.model.scroll_from_bottom =
                                target.model.scroll_from_bottom.saturating_add(n)
                        }
                        Action::ScrollDown(n) => {
                            target.model.scroll_from_bottom =
                                target.model.scroll_from_bottom.saturating_sub(n)
                        }
                        Action::Custom(ref name, _) if name == "terminal:copy-text" => {
                            allowed.push(action)
                        }
                        Action::Custom(name, payload) if name == "agents:inspect" => {
                            inspect = Some(payload);
                        }
                        _ => {} // Never leak submit/editor/steer/stop actions.
                    }
                }
                if let Some(payload) = inspect {
                    state.inspect(
                        &ctx.model.session,
                        &session,
                        payload["call"].as_str().map(str::to_owned),
                        payload["target"].as_str().map(str::to_owned),
                    );
                }
                return KeyOutcome::act(allowed);
            }
        }
        KeyOutcome::consumed()
    }

    fn on_binding(&mut self, _: &Ctx<'_>, _: &str) -> KeyOutcome {
        KeyOutcome::consumed()
    }

    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        let mut state = self.state.0.lock().expect("agent monitor lock");
        let Some((root, detail)) = state.requested.clone() else {
            return;
        };
        if root != ctx.model.session {
            state.requested = None;
            state.pending_inspect = None;
            return;
        }
        self.chats
            .retain(|session, _| state.sessions.contains_key(session));
        Clear.render(area, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .style(ctx.theme.overlay)
            .border_style(ctx.theme.overlay_border)
            .title(Line::styled(
                format!(" Agents · {} · read-only ", state.agents.len()),
                ctx.theme.heading,
            ));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let body = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        );
        let hint = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
        if let Some(session) = detail {
            if state.root != root || !state.agents.iter().any(|a| a.session == session) {
                Paragraph::new(
                    "Waiting for an authorized agent entry (or agent not found). Esc: list",
                )
                .style(ctx.theme.dim)
                .wrap(Wrap { trim: false })
                .render(body, buf);
                return;
            }
            self.selected = Some(session.clone());
            let info = state
                .agents
                .iter()
                .find(|a| a.session == session)
                .expect("authorized agent");
            let header = Line::from(vec![
                Span::styled(format!(" {}  {}", info.alias, info.name), ctx.theme.heading),
                Span::styled(
                    format!("   {}", info.status),
                    status_style(&info.status, ctx.theme),
                ),
                Span::styled(
                    format!(" · {} · {}", elapsed(info.elapsed_ms), info.mode),
                    ctx.theme.dim,
                ),
            ]);
            if body.height > 0 {
                Paragraph::new(header).render(Rect::new(body.x, body.y, body.width, 1), buf);
            }
            // Task is always first; raw identities remain available by scrolling
            // this small metadata pane, never ahead of the assignment.
            let task_rows =
                textwrap::wrap(&info.task, usize::from(body.width.saturating_sub(2).max(1)))
                    .len()
                    .max(1);
            let metadata_height = (task_rows.min(3) as u16).min(body.height.saturating_sub(1) / 3);
            let heading = format!(
                "{}\n\nSession: {}\nParent: {} · depth {}\nCall: {}",
                info.task,
                info.session,
                info.parent.as_deref().unwrap_or("—"),
                info.depth,
                info.call.as_deref().unwrap_or("—")
            );
            let rows = heading
                .lines()
                .map(|line| {
                    textwrap::wrap(line, usize::from(body.width.max(1)))
                        .len()
                        .max(1)
                })
                .sum::<usize>();
            let offset = self.metadata_scroll.entry(session.clone()).or_default();
            *offset = (*offset).min(
                rows.saturating_sub(usize::from(metadata_height))
                    .min(usize::from(u16::MAX)) as u16,
            );
            Paragraph::new(heading)
                .scroll((*offset, 0))
                .wrap(Wrap { trim: false })
                .style(if *offset == 0 {
                    ctx.theme.assistant_text
                } else {
                    ctx.theme.dim
                })
                .render(
                    Rect::new(
                        body.x,
                        body.y.saturating_add(1),
                        body.width,
                        metadata_height,
                    ),
                    buf,
                );
            let height = (1 + metadata_height + 1).min(body.height);
            let target = state.session(&session);
            let following = target.model.scroll_from_bottom == 0;
            if following {
                target.seen = target.activity;
            }
            let new_activity = target.seen != target.activity;
            let chat = Self::chat(&mut self.chats, &self.config, target, ctx.theme);
            chat.render(
                &Ctx {
                    model: &target.model,
                    theme: ctx.theme,
                },
                Rect::new(
                    body.x,
                    body.y + height,
                    body.width,
                    body.height.saturating_sub(height),
                ),
                buf,
            );
            let follow = if following {
                "Following"
            } else if new_activity {
                "Paused + new"
            } else {
                "Paused"
            };
            let help = if hint.width < 60 {
                format!("Esc list · {follow} · End follow")
            } else if hint.width < 110 {
                format!("Esc list · {follow} · End follow · PgUp/Dn scroll · Enter tools")
            } else {
                format!("Esc list · {follow} · End follow · PgUp/Dn scroll · Enter tools · Alt+M copy · Alt+PgUp/Dn metadata")
            };
            Paragraph::new(help).style(ctx.theme.dim).render(hint, buf);
        } else {
            if !state
                .agents
                .iter()
                .any(|a| Some(&a.session) == self.selected.as_ref())
            {
                self.selected = state.agents.first().map(|a| a.session.clone());
            }
            let index = state
                .agents
                .iter()
                .position(|a| Some(&a.session) == self.selected.as_ref())
                .unwrap_or(0);
            let visible = usize::from(body.height).max(1);
            self.list_offset = self.list_offset.min(index);
            if index >= self.list_offset + visible {
                self.list_offset = index + 1 - visible;
            }
            let lines = if state.agents.is_empty() {
                vec![Line::raw("No agents published yet.")]
            } else {
                state
                    .agents
                    .iter()
                    .skip(self.list_offset)
                    .take(visible)
                    .map(|a| {
                        let selected = Some(&a.session) == self.selected.as_ref();
                        let fresh = state
                            .sessions
                            .get(&a.session)
                            .is_some_and(|s| s.activity != s.seen);
                        let width = usize::from(body.width);
                        let mut spans = vec![Span::styled(
                            if selected { "› " } else { "  " },
                            ctx.theme.statusline_accent,
                        )];
                        if width >= 60 {
                            let role_width = if width >= 90 { 20 } else { 14 };
                            spans.push(Span::styled(column(&a.alias, 6), ctx.theme.tool_name));
                            let role = format!(
                                "{}{}",
                                "  ".repeat(a.depth.saturating_sub(1).min(3)),
                                a.name
                            );
                            spans.push(Span::styled(
                                column(&role, role_width),
                                if selected {
                                    ctx.theme.heading
                                } else {
                                    ctx.theme.assistant_text
                                },
                            ));
                            spans.push(Span::styled(
                                column(&a.status, 12),
                                status_style(&a.status, ctx.theme),
                            ));
                            spans.push(Span::styled(
                                column(&elapsed(a.elapsed_ms), 9),
                                ctx.theme.dim,
                            ));
                            spans.push(Span::styled(
                                if fresh { "● " } else { "  " },
                                ctx.theme.statusline_accent,
                            ));
                            spans.push(Span::styled(
                                column(&a.task, width.saturating_sub(31 + role_width)),
                                ctx.theme.dim,
                            ));
                        } else {
                            spans.push(Span::styled(
                                column(
                                    &format!("{} {}", a.alias, a.name),
                                    width.saturating_sub(15),
                                ),
                                if selected {
                                    ctx.theme.heading
                                } else {
                                    ctx.theme.assistant_text
                                },
                            ));
                            spans.push(Span::styled(
                                column(&a.status, width.saturating_sub(2).min(12)),
                                status_style(&a.status, ctx.theme),
                            ));
                            if fresh {
                                spans.push(Span::styled("●", ctx.theme.statusline_accent));
                            }
                        }
                        Line::from(spans)
                    })
                    .collect()
            };
            Paragraph::new(lines).render(body, buf);
            Paragraph::new("Esc close · ↑/↓ select · Enter detail")
                .style(ctx.theme.dim)
                .render(hint, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Entry;
    use crate::theme::Theme;
    use rness_protocol::events::{
        AssistantMessage, ChunkDelta, ContentPart, SessionEvent, StopReason, Usage,
    };

    fn info(session: &str) -> AgentInfo {
        AgentInfo {
            session: session.into(),
            alias: session.into(),
            parent: Some("root".into()),
            depth: 1,
            name: session.into(),
            task: "Read the project".into(),
            mode: "background".into(),
            status: "running".into(),
            elapsed_ms: 123,
            call: Some("call".into()),
        }
    }
    fn event(id: &str, text: &str) -> Envelope {
        Envelope {
            id: id.into(),
            at: String::new(),
            event: SessionEvent::AssistantMessage(AssistantMessage {
                model: "m".into(),
                content: vec![ContentPart::Text { text: text.into() }],
                stop: StopReason::EndTurn,
                usage: Usage::default(),
                chunks: vec![],
            }),
        }
    }
    fn open(drawer: &mut Agents, ctx: &Ctx<'_>, agent: Option<&str>) {
        drawer.on_action(
            ctx,
            "agents:open",
            &serde_json::json!({"session":"root","agent":agent}),
        );
    }
    fn key(drawer: &mut Agents, ctx: &Ctx<'_>, code: KeyCode) -> KeyOutcome {
        drawer.on_key(ctx, KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn presentation_prioritizes_task_and_formats_duration_and_columns() {
        assert_eq!(elapsed(61_000), "1m 01s");
        assert_eq!(elapsed(3_661_000), "1h 01m");
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(column("界界界", 5).as_str()),
            5
        );
        let state = AgentMonitorState::default();
        state.publish_agents("root", vec![info("child")]);
        let mut drawer = Agents::new(state);
        let model = Model::new("root".into(), String::new());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        open(&mut drawer, &ctx, Some("child"));
        let area = Rect::new(0, 0, 90, 25);
        let mut buf = Buffer::empty(area);
        drawer.render(&ctx, area, &mut buf);
        let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(text.contains("Read the project"));
        assert!(!text.contains("Session:"));
        for _ in 0..3 {
            drawer.on_key(&ctx, KeyEvent::new(KeyCode::PageDown, KeyModifiers::ALT));
        }
        drawer.render(&ctx, area, &mut buf);
        let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(text.contains("Call:"));
        let area = Rect::new(0, 0, 72, 24);
        let mut buf = Buffer::empty(area);
        drawer.render(&ctx, area, &mut buf);
        assert!(buf
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains("Esc list"));
        key(&mut drawer, &ctx, KeyCode::Esc);
        assert_eq!(drawer.height(&ctx, 80), Some(4));
    }

    #[test]
    fn inspect_resolves_after_publication_and_rejects_unrelated_cached_sessions() {
        let mut slots = Slots::default();
        let state = install(&mut slots, CardCache::default());
        let model = Model::new("root".into(), String::new());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        state.observe(&Frame::Delta {
            session: "unrelated".into(),
            chunk: ChunkDelta::Text { t: "SECRET".into() },
        });
        slots.broadcast(
            &ctx,
            "agents:inspect",
            &serde_json::json!({"session":"root","call":"call"}),
        );
        assert_eq!(state.requested(), Some(("root".into(), None)));
        let mut wrong = info("wrong-parent");
        wrong.parent = Some("elsewhere".into());
        state.publish_agents("root", vec![wrong]);
        assert_eq!(state.requested(), Some(("root".into(), None)));
        state.publish_agents("root", vec![info("child")]);
        assert_eq!(
            state.requested(),
            Some(("root".into(), Some("child".into())))
        );
        // Nested chat inspection is resolved locally; it must not escape as a
        // parent-session action or change the outer authorized root.
        let mut drawer = Agents::new(state.clone());
        {
            let mut state = state.0.lock().unwrap();
            let child = state.session("child");
            child.model.entries.push(Entry::Assistant {
                model: "m".into(),
                content: vec![ContentPart::ToolUse {
                    call: "nested".into(),
                    name: "subagent".into(),
                    args: serde_json::json!({}),
                }],
            });
            child.model.live = Some(crate::app::LiveStep {
                running_tools: vec![("nested".into(), "subagent".into())],
                ..Default::default()
            });
        }
        key(&mut drawer, &ctx, KeyCode::Enter); // focus nested tool
        assert!(key(&mut drawer, &ctx, KeyCode::Char('i'))
            .actions
            .is_empty());
        let mut grandchild = info("grandchild");
        grandchild.parent = Some("child".into());
        grandchild.call = Some("nested".into());
        state.publish_agents("root", vec![info("child"), grandchild]);
        assert_eq!(
            state.requested(),
            Some(("root".into(), Some("grandchild".into())))
        );
        open(&mut drawer, &ctx, Some("unrelated"));
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        drawer.render(&ctx, area, &mut buf);
        assert!(!buf
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains("SECRET"));
        assert!(key(&mut drawer, &ctx, KeyCode::Enter).actions.is_empty());
        assert!(!drawer.chats.contains_key("unrelated"));
    }

    #[test]
    fn live_before_open_is_bounded_and_cards_are_session_local() {
        let state = AgentMonitorState::default();
        state.observe(&Frame::Delta {
            session: "child".into(),
            chunk: ChunkDelta::Text {
                t: "é".repeat(MAX_LIVE_BYTES),
            },
        });
        assert!(state.requested().is_none());
        assert!(
            state.0.lock().unwrap().sessions["child"]
                .model
                .live
                .as_ref()
                .unwrap()
                .text
                .len()
                <= MAX_LIVE_BYTES
        );
        state.cards("a").insert("same-call".into(), vec![]);
        assert!(!state.cards("b").contains(&"same-call".into()));
        for n in 0..MAX_SESSIONS + 5 {
            state.observe(&Frame::StepStarted {
                session: n.to_string(),
                turn: 1,
            });
        }
        assert_eq!(state.0.lock().unwrap().sessions.len(), MAX_SESSIONS);
    }

    #[test]
    fn initial_history_does_not_hide_unobserved_prior_turns_during_live_step() {
        let state = AgentMonitorState::default();
        state.observe(&Frame::StepStarted {
            session: "child".into(),
            turn: 300,
        });
        state.observe(&Frame::Delta {
            session: "child".into(),
            chunk: ChunkDelta::Text {
                t: "current".into(),
            },
        });
        let mut prior: Vec<_> = (0..300)
            .map(|n| event(&format!("prior-{n}"), "previous step"))
            .collect();
        state.publish_history(
            "child",
            History {
                session: "child".into(),
                envelopes: prior.clone(),
            },
        );
        assert_eq!(
            state.0.lock().unwrap().sessions["child"]
                .model
                .entries
                .len(),
            300
        );
        prior.push(event("current", "current"));
        state.publish_history(
            "child",
            History {
                session: "child".into(),
                envelopes: prior,
            },
        );
        assert_eq!(
            state.0.lock().unwrap().sessions["child"]
                .model
                .entries
                .len(),
            300
        );
        state.observe(&Frame::StepCommitted {
            session: "child".into(),
            event: "current".into(),
        });
        assert_eq!(
            state.0.lock().unwrap().sessions["child"]
                .model
                .entries
                .len(),
            301
        );
    }

    #[test]
    fn refresh_preserves_new_live_and_commit_clears_only_stream() {
        let state = AgentMonitorState::default();
        state.observe(&Frame::StepStarted {
            session: "child".into(),
            turn: 1,
        });
        state.observe(&Frame::Delta {
            session: "child".into(),
            chunk: ChunkDelta::Text { t: "old".into() },
        });
        // Snapshot may land before its corresponding frame callback.
        state.publish_history(
            "child",
            History {
                session: "child".into(),
                envelopes: vec![event("one", "old")],
            },
        );
        assert!(state.0.lock().unwrap().sessions["child"]
            .model
            .entries
            .is_empty());
        assert_eq!(
            state.0.lock().unwrap().sessions["child"]
                .model
                .live
                .as_ref()
                .unwrap()
                .text,
            "old"
        );
        state.observe(&Frame::StepCommitted {
            session: "child".into(),
            event: "one".into(),
        });
        assert_eq!(state.history_cursor("child"), Some("one".into()));
        assert!(state.0.lock().unwrap().sessions["child"]
            .model
            .live
            .is_none());
        state.observe(&Frame::StepStarted {
            session: "child".into(),
            turn: 2,
        });
        state.observe(&Frame::Delta {
            session: "child".into(),
            chunk: ChunkDelta::Text {
                t: "new stream".into(),
            },
        });
        state.with_history("child", || History {
            session: "child".into(),
            envelopes: vec![event("one", "old")],
        });
        assert_eq!(
            state.0.lock().unwrap().sessions["child"]
                .model
                .live
                .as_ref()
                .unwrap()
                .text,
            "new stream"
        );
        assert!(!state.publish_history_after("child", "stale", vec![event("two", "new")]));
        assert!(state.publish_history_after("child", "one", vec![event("two", "new stream")]));
        assert_eq!(
            state.0.lock().unwrap().sessions["child"]
                .model
                .entries
                .len(),
            1
        );
        state.observe(&Frame::StepCommitted {
            session: "child".into(),
            event: "two".into(),
        });
        assert_eq!(
            state.0.lock().unwrap().sessions["child"]
                .model
                .entries
                .len(),
            2
        );
    }

    #[test]
    fn navigation_escape_root_validation_and_scroll_are_isolated() {
        let state = AgentMonitorState::default();
        state.publish_agents("root", vec![info("a"), info("b")]);
        let mut drawer = Agents::new(state.clone());
        let mut model = Model::new("root".into(), "main model".into());
        model.entries.push(Entry::Notice("main untouched".into()));
        model.scroll_from_bottom = 42;
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        drawer.on_action(&ctx, "agents:open", &serde_json::json!({"session":"wrong"}));
        assert!(!drawer.wants(&ctx));
        open(&mut drawer, &ctx, None);
        drawer.on_action(&ctx, "viewport:wheel", &serde_json::json!({"up":false}));
        assert_eq!(drawer.selected.as_deref(), Some("b"));
        drawer.on_action(&ctx, "viewport:wheel", &serde_json::json!({"up":true}));
        assert_eq!(drawer.selected.as_deref(), Some("a"));
        key(&mut drawer, &ctx, KeyCode::Down);
        key(&mut drawer, &ctx, KeyCode::Enter);
        assert_eq!(state.requested(), Some(("root".into(), Some("b".into()))));
        drawer.on_action(&ctx, "viewport:wheel", &serde_json::json!({"up":true}));
        assert_eq!(
            state.0.lock().unwrap().sessions["b"]
                .model
                .scroll_from_bottom,
            3
        );
        drawer.on_action(&ctx, "viewport:wheel", &serde_json::json!({"up":false}));
        assert_eq!(
            state.0.lock().unwrap().sessions["b"]
                .model
                .scroll_from_bottom,
            0
        );
        key(&mut drawer, &ctx, KeyCode::PageUp);
        assert_eq!(
            state.0.lock().unwrap().sessions["b"]
                .model
                .scroll_from_bottom,
            10
        );
        key(&mut drawer, &ctx, KeyCode::End);
        assert_eq!(
            state.0.lock().unwrap().sessions["b"]
                .model
                .scroll_from_bottom,
            0
        );
        for code in [
            KeyCode::Char('c'),
            KeyCode::Char('x'),
            KeyCode::Char('e'),
            KeyCode::Enter,
            KeyCode::F(2),
        ] {
            let outcome = drawer.on_key(&ctx, KeyEvent::new(code, KeyModifiers::CONTROL));
            assert!(outcome.handled);
            assert!(outcome.actions.is_empty());
        }
        assert!(drawer.on_paste(&ctx, "do not submit").actions.is_empty());
        key(&mut drawer, &ctx, KeyCode::Esc);
        assert_eq!(state.requested(), Some(("root".into(), None)));
        key(&mut drawer, &ctx, KeyCode::Esc);
        assert_eq!(state.requested(), None);
        assert_eq!(model.scroll_from_bottom, 42);
        assert_eq!(model.entries, vec![Entry::Notice("main untouched".into())]);
        open(&mut drawer, &ctx, Some("a"));
        let other = Model::new("other".into(), String::new());
        assert!(!drawer.wants(&Ctx {
            model: &other,
            theme: &theme
        }));
        assert!(state.requested().is_none());
    }

    #[test]
    fn chat_expansion_copy_and_config_are_reused_without_editor_effects() {
        let state = AgentMonitorState::default();
        state.publish_agents("root", vec![info("a")]);
        let mut drawer = Agents::new(state.clone());
        let model = Model::new("root".into(), String::new());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        open(&mut drawer, &ctx, Some("a"));
        {
            let mut state = state.0.lock().unwrap();
            let child = state.session("a");
            child.model.entries.push(Entry::Assistant {
                model: "m".into(),
                content: vec![ContentPart::ToolUse {
                    call: "tool".into(),
                    name: "bash".into(),
                    args: serde_json::json!({"command":"ls"}),
                }],
            });
            child.model.entries.push(Entry::ToolResult {
                call: "tool".into(),
                name: "bash".into(),
                output: "one\ntwo\nthree\nfour\nfive\nsix".into(),
                is_error: false,
            });
            child.model.entry_ids = vec!["message".into(), "result".into()];
        }
        drawer.on_action(&ctx, "chat:messagebox-config", &serde_json::json!({"tool":{"display":"preview","preview_lines":1},"keys":{"selection_editor":"x"}}));
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        drawer.render(&ctx, area, &mut buf);
        assert!(key(&mut drawer, &ctx, KeyCode::Enter).actions.is_empty());
        drawer.render(&ctx, area, &mut buf);
        let screen = buf
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("six"), "{screen}");
        drawer.on_key(&ctx, KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT));
        assert!(key(&mut drawer, &ctx, KeyCode::Char('x'))
            .actions
            .is_empty());
        let copied = key(&mut drawer, &ctx, KeyCode::Char('y'));
        assert!(
            matches!(copied.actions.as_slice(), [Action::Custom(name, _)] if name == "terminal:copy-text")
        );
        for w in [0, 1, 8, 60] {
            let area = Rect::new(0, 0, w, 3);
            drawer.render(&ctx, area, &mut Buffer::empty(area));
        }
    }

    #[test]
    fn committed_tool_clears_live_card_and_config_invalidates_child_cards() {
        let state = AgentMonitorState::default();
        state.observe(&Frame::ToolStarted {
            session: "a".into(),
            call: "call".into(),
            name: "bash".into(),
        });
        state.observe(&Frame::ToolOutput {
            session: "a".into(),
            call: "call".into(),
            output: "partial".into(),
        });
        assert_eq!(
            state.0.lock().unwrap().sessions["a"]
                .model
                .live
                .as_ref()
                .unwrap()
                .running_tools
                .len(),
            1
        );
        let result = serde_json::from_value(serde_json::json!({
            "id":"result", "at":"", "type":"tool/result", "call":"call", "name":"bash", "output":"done", "is_error":false, "duration_ms":1
        })).unwrap();
        state.publish_history(
            "a",
            History {
                session: "a".into(),
                envelopes: vec![result],
            },
        );
        assert!(state.0.lock().unwrap().sessions["a"]
            .model
            .live
            .as_ref()
            .unwrap()
            .running_tools
            .is_empty());
        let cards = state.cards("a");
        cards.insert("call".into(), vec![]);
        let generation = cards.generation();
        let model = Model::new("root".into(), String::new());
        let theme = Theme::default();
        let mut drawer = Agents::new(state);
        drawer.on_action(
            &Ctx {
                model: &model,
                theme: &theme,
            },
            "chat:messagebox-config",
            &serde_json::json!({}),
        );
        assert!(cards.generation() > generation);
        assert!(!cards.contains(&"call".into()));
    }

    #[test]
    fn higher_priority_questions_shadow_drawer_and_selected_session_is_pinned() {
        struct Question;
        impl Component for Question {
            fn name(&self) -> &str {
                "question"
            }
            fn height(&self, _: &Ctx<'_>, _: u16) -> Option<u16> {
                Some(1)
            }
            fn render(&mut self, _: &Ctx<'_>, _: Rect, _: &mut Buffer) {}
        }
        let mut slots = Slots::default();
        let state = install(&mut slots, CardCache::default());
        let model = Model::new("root".into(), String::new());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        slots.broadcast(
            &ctx,
            "agents:open",
            &serde_json::json!({"session":"root","agent":"pinned"}),
        );
        state.cards("pinned");
        for n in 0..MAX_SESSIONS + 2 {
            state.cards(&n.to_string());
        }
        assert!(state.0.lock().unwrap().sessions.contains_key("pinned"));
        assert_eq!(slots.focused_mut(&ctx).unwrap().name(), "agents");
        slots.mount(OVERLAY, -100, Box::new(Question));
        assert_eq!(slots.focused_mut(&ctx).unwrap().name(), "question");
    }
}

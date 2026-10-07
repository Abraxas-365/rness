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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
    /// Unix milliseconds the child was created, when known.
    pub started_ms: Option<u64>,
    /// Unix milliseconds its latest turn ended; `None` while running or unknown.
    pub ended_ms: Option<u64>,
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
    pub fn publish_history_after(
        &self,
        session: &str,
        after: &str,
        events: Vec<Arc<Envelope>>,
    ) -> bool {
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
    pub fn append_history(&self, session: &str, events: Vec<Arc<Envelope>>) {
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
            | Frame::ApprovalResolved { session, .. }
            | Frame::TitleChanged { session, .. }
            | Frame::Notice { session, .. } => session,
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
        "finished" | "completed" | "done" | "idle" => theme.added,
        "running" => theme.statusline_accent,
        _ => theme.dim,
    }
}

fn status_icon(status: &str) -> &'static str {
    match status {
        "running" => "●",
        "failed" | "error" => "✗",
        "finished" | "completed" | "done" | "idle" => "✓",
        "interrupted" => "⊘",
        _ => "·",
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Compact age: `45s`, `12m`, `3h`, `2d`.
fn short_age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Replace `home` with `~` only where it is a whole path prefix: not inside
/// another path (`/srv/Users/me`) and not a sibling (`/Users/meg`).
fn replace_home(text: &str, home: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut prev: Option<char> = None;
    while let Some(at) = rest.find(home) {
        let before = rest[..at].chars().next_back().or(prev);
        let after = rest[at + home.len()..].chars().next();
        let starts = before.is_none_or(|c| c.is_whitespace() || "\"'`=:(,".contains(c));
        let ends = after.is_none_or(|c| c == '/' || c.is_whitespace() || ",.;:)\"'`".contains(c));
        out.push_str(&rest[..at]);
        out.push_str(if starts && ends { "~" } else { home });
        prev = home.chars().next_back();
        rest = &rest[at + home.len()..];
    }
    out.push_str(rest);
    out
}

/// One-line task preview: the first non-empty line, without the routine
/// "In the X codebase at /path," preamble, with the home directory as `~`.
fn short_task(task: &str, home: Option<&str>) -> String {
    let line = task
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let mut text = line;
    if let Some(rest) = text
        .strip_prefix("In the ")
        .or_else(|| text.strip_prefix("in the "))
    {
        let marker = [" codebase at ", " repo at "]
            .iter()
            .find_map(|m| rest.find(m).map(|at| at + m.len()));
        // Only "… at /path, task": the comma must end the path token itself.
        if let Some(path_start) = marker {
            let after = &rest[path_start..];
            let token_end = after.find(char::is_whitespace).unwrap_or(after.len());
            if token_end > 1 && after[..token_end].ends_with(',') {
                text = after[token_end..].trim_start();
            }
        }
    }
    let mut out = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(home) = home.filter(|h| h.len() > 1) {
        out = replace_home(&out, home);
    }
    let mut chars = out.chars();
    match chars.next() {
        Some(first) if text.len() != line.len() => first.to_uppercase().chain(chars).collect(),
        _ => out,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListFilter {
    Active,
    Recent,
    All,
}

impl ListFilter {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "active" => Some(Self::Active),
            "recent" => Some(Self::Recent),
            "all" => Some(Self::All),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Active => "running",
            Self::Recent => "recent",
            Self::All => "all",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Active => Self::Recent,
            Self::Recent => Self::All,
            Self::All => Self::Active,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ListRow {
    Section(String, usize),
    Agent(usize),
    Note(String),
}

/// Display rows plus the selectable agents (indexes into the published list)
/// in display order.
struct ListView {
    rows: Vec<ListRow>,
    order: Vec<usize>,
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

// Ordered by dispatch priority. Lists replace (rather than extend) defaults.
const MONITOR_KEYS: &[(&str, &[&str])] = &[
    ("back", &["esc"]),
    ("previous_agent", &["shift+tab"]),
    ("next_agent", &["tab"]),
    ("list_up", &["up", "k"]),
    ("list_down", &["down", "j"]),
    ("open_detail", &["enter"]),
    ("filter", &["f"]),
    ("show_all", &["a"]),
    ("metadata_up", &["alt+pageup"]),
    ("metadata_down", &["alt+pagedown"]),
    ("page_up", &["pageup"]),
    ("page_down", &["pagedown"]),
    ("follow", &["end"]),
    ("scroll_up", &["up"]),
    ("scroll_down", &["down"]),
    ("toggle_tool", &["enter", "ctrl+o"]),
];

struct Agents {
    state: AgentMonitorState,
    chats: HashMap<String, (u64, Chat)>,
    config: serde_json::Value,
    selected: Option<String>,
    list_offset: usize,
    metadata_scroll: HashMap<String, u16>,
    /// Session override of `list.filter`; reset whenever the monitor opens.
    filter: Option<ListFilter>,
    /// Until the user moves the cursor it follows the top row, so the first
    /// fresh publication after opening (or a newly started agent) is selected.
    follow_top: bool,
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
            filter: None,
            follow_top: false,
        }
    }

    fn default_filter(&self) -> ListFilter {
        self.option("list", "filter")
            .as_str()
            .and_then(ListFilter::parse)
            .unwrap_or(ListFilter::Recent)
    }

    fn filter(&self) -> ListFilter {
        self.filter.unwrap_or_else(|| self.default_filter())
    }

    fn recent(&self, name: &str, default: u64) -> u64 {
        self.config["agents"]["list"]["recent"][name]
            .as_u64()
            .unwrap_or(default)
    }

    fn list_text(&self, name: &str, default: &str, n: usize) -> String {
        self.text(name, default).replace("{n}", &n.to_string())
    }

    /// Running agents first, then the most recently finished. Ties keep
    /// creation order, so agents without timestamps stay stable.
    fn list_view(
        &self,
        agents: &[AgentInfo],
        now: u64,
        group: bool,
        filter: ListFilter,
    ) -> ListView {
        let newest = self.option("list", "order").as_str() != Some("oldest");
        let (mut running, mut finished): (Vec<usize>, Vec<usize>) =
            (0..agents.len()).partition(|&i| agents[i].status == "running");
        let finished_at = |i: usize| agents[i].ended_ms.or(agents[i].started_ms);
        running.sort_by_key(|&i| std::cmp::Reverse(agents[i].started_ms.unwrap_or(0)));
        finished.sort_by_key(|&i| std::cmp::Reverse(finished_at(i).unwrap_or(0)));
        let keep = match filter {
            ListFilter::All => finished.len(),
            ListFilter::Active => 0,
            ListFilter::Recent => {
                let window = self.recent("secs", 1800).saturating_mul(1000);
                let within = finished
                    .iter()
                    .take_while(|&&i| {
                        finished_at(i).is_some_and(|t| now.saturating_sub(t) <= window)
                    })
                    .count();
                within
                    .max(self.recent("min", 3) as usize)
                    .min(self.recent("max", 10) as usize)
                    .min(finished.len())
            }
        };
        let hidden = finished.len() - keep;
        finished.truncate(keep);
        if !newest {
            running.reverse();
            finished.reverse();
        }
        let mut rows = Vec::new();
        if group && !running.is_empty() {
            rows.push(ListRow::Section(
                self.text("section_running", "Running").to_owned(),
                running.len(),
            ));
        }
        rows.extend(running.iter().map(|&i| ListRow::Agent(i)));
        if group && !finished.is_empty() {
            let label = if filter == ListFilter::All {
                self.text("section_finished", "Finished")
            } else {
                self.text("section_recent", "Recent")
            };
            rows.push(ListRow::Section(label.to_owned(), finished.len()));
        }
        rows.extend(finished.iter().map(|&i| ListRow::Agent(i)));
        if running.is_empty() && finished.is_empty() && !agents.is_empty() {
            let note = if filter == ListFilter::Active {
                self.text("empty_active", "No running agents.")
            } else {
                self.text("empty_recent", "Nothing running or recently finished.")
            };
            rows.push(ListRow::Note(note.to_owned()));
        }
        if hidden > 0 {
            let mut note = if filter == ListFilter::Active {
                self.list_text("finished_hidden", "… {n} finished hidden", hidden)
            } else {
                self.list_text("older_hidden", "… {n} older hidden", hidden)
            };
            let hint = self.hint("show_all", "show all");
            if !hint.is_empty() {
                note = format!("{note} · {hint}");
            }
            rows.push(ListRow::Note(note));
        }
        let order = running.into_iter().chain(finished).collect();
        ListView { rows, order }
    }

    fn group(&self) -> bool {
        self.option("list", "group").as_bool().unwrap_or(true)
    }

    fn option(&self, section: &str, name: &str) -> &serde_json::Value {
        &self.config["agents"][section][name]
    }

    fn number(&self, name: &str, default: u16) -> u16 {
        self.option("layout", name)
            .as_u64()
            .map(|n| n.min(1000) as u16)
            .unwrap_or(default)
    }

    fn text<'a>(&'a self, name: &str, default: &'a str) -> &'a str {
        self.option("text", name).as_str().unwrap_or(default)
    }

    fn style(
        &self,
        theme: &crate::theme::Theme,
        name: &str,
        fallback: ratatui::style::Style,
    ) -> ratatui::style::Style {
        theme
            .resolve_style(self.option("styles", name), fallback)
            .unwrap_or(fallback)
    }

    fn keys(&self, action: &str) -> Vec<&str> {
        match self.option("keys", action) {
            serde_json::Value::Bool(false) => vec![],
            serde_json::Value::String(key) => vec![key],
            serde_json::Value::Array(keys) => keys.iter().filter_map(|v| v.as_str()).collect(),
            _ => MONITOR_KEYS
                .iter()
                .find(|(name, _)| *name == action)
                .map(|(_, keys)| keys.to_vec())
                .unwrap_or_default(),
        }
    }

    fn matches(&self, action: &str, key: KeyEvent) -> bool {
        // Crossterm reports reverse tab as BackTab, while the shared key language
        // uses shift+tab. Some terminals omit the modifier on BackTab.
        let key = if key.code == KeyCode::BackTab {
            KeyEvent::new(KeyCode::Tab, key.modifiers | KeyModifiers::SHIFT)
        } else {
            key
        };
        self.keys(action)
            .iter()
            .any(|s| crate::keys::Chord::parse(s).is_some_and(|chord| chord.matches(&key)))
    }

    fn hint(&self, action: &str, label: &str) -> String {
        let keys = self.keys(action);
        if keys.is_empty() {
            return String::new();
        }
        let keys = keys
            .iter()
            .map(|key| match *key {
                "esc" => "Esc",
                "enter" => "Enter",
                "end" => "End",
                _ => key,
            })
            .collect::<Vec<_>>()
            .join("/");
        format!("{keys} {label}")
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
        let mut order = self
            .list_view(&state.agents, now_ms(), false, self.filter())
            .order;
        let position = |order: &[usize]| {
            order
                .iter()
                .position(|&i| Some(&state.agents[i].session) == self.selected.as_ref())
        };
        // A detail opened outside the current filter still cycles through
        // everyone; in the list the cursor never lands on a hidden row.
        let detail_open = matches!(&state.requested, Some((_, Some(_))));
        if detail_open && position(&order).is_none() && self.selected.is_some() {
            order = self
                .list_view(&state.agents, now_ms(), false, ListFilter::All)
                .order;
        }
        if order.is_empty() {
            return;
        }
        let index = position(&order).unwrap_or(0);
        let index = index.saturating_add_signed(delta).min(order.len() - 1);
        self.follow_top = false;
        self.selected = Some(state.agents[order[index]].session.clone());
        if let Some((_, detail)) = &mut state.requested {
            if detail.is_some() {
                *detail = self.selected.clone();
            }
        }
    }

    fn toggle_filter(&mut self, next: ListFilter) {
        self.filter = (next != self.default_filter()).then_some(next);
        self.list_offset = 0;
    }

    /// `› ● a3   reviewer    ● Review the parser…   1m 05s   2m ago      done`
    /// Narrow panels drop duration and age; very narrow ones drop the task.
    fn agent_row(
        &self,
        ctx: &Ctx<'_>,
        a: &AgentInfo,
        width: usize,
        now: u64,
        home: Option<&str>,
        state: &Monitor,
    ) -> Line<'static> {
        let selected = Some(&a.session) == self.selected.as_ref();
        let fresh = state
            .sessions
            .get(&a.session)
            .is_some_and(|s| s.activity != s.seen);
        let status = status_style(&a.status, ctx.theme);
        let role_width = if width >= 90 { 16 } else { 12 };
        let role = format!(
            "{}{}",
            "  ".repeat(a.depth.saturating_sub(1).min(3)),
            a.name
        );
        let mut spans = vec![
            Span::styled(
                if selected { "› " } else { "  " },
                ctx.theme.statusline_accent,
            ),
            Span::styled(format!("{} ", status_icon(&a.status)), status),
            Span::styled(column(&a.alias, 5), ctx.theme.tool_name),
            Span::styled(
                column(&role, role_width),
                if selected {
                    ctx.theme.heading
                } else {
                    ctx.theme.assistant_text
                },
            ),
            Span::styled(if fresh { "● " } else { "  " }, ctx.theme.statusline_accent),
        ];
        let mut right = Vec::new();
        if width >= 60 {
            right.push((format!("{:>8}", elapsed(a.elapsed_ms)), ctx.theme.dim));
            let age = match a.ended_ms.or(a.started_ms) {
                Some(t) if a.status != "running" => self
                    .text("ago", "{age} ago")
                    .replace("{age}", &short_age(now.saturating_sub(t))),
                _ => String::new(),
            };
            right.push((format!("{age:>9}"), ctx.theme.dim));
        }
        let word = match a.status.as_str() {
            "finished" => self.text("status_finished", "done"),
            other => other,
        };
        right.push((format!("{word:>12}"), status));
        let used = 2 + 2 + 5 + role_width + 2;
        let right_width: usize = right
            .iter()
            .map(|(t, _)| unicode_width::UnicodeWidthStr::width(t.as_str()))
            .sum();
        let task_width = width.saturating_sub(used + right_width);
        if task_width >= 8 {
            spans.push(Span::styled(
                column(&short_task(&a.task, home), task_width),
                self.style(ctx.theme, "task", ctx.theme.dim),
            ));
            spans.extend(right.into_iter().map(|(t, s)| Span::styled(t, s)));
        } else if let Some((t, s)) = right.pop() {
            spans.push(Span::styled(t.trim_start().to_owned(), s));
        }
        let line = Line::from(spans);
        if selected {
            line.style(self.style(ctx.theme, "selected", ratatui::style::Style::default()))
        } else {
            line
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
            let rows = self
                .list_view(&state.agents, now_ms(), self.group(), self.filter())
                .rows
                .len();
            Some(
                rows.max(1)
                    .min(usize::from(self.number("list_rows", 20).max(1)))
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
        MONITOR_KEYS
            .iter()
            .filter_map(|(action, _)| {
                let hint = self.hint(action, action);
                (!hint.is_empty()).then(|| format!("Agents (read-only): {hint}"))
            })
            .collect()
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
                        scroll.saturating_add(self.number("wheel_lines", 3))
                    } else {
                        scroll.saturating_sub(self.number("wheel_lines", 3))
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
                self.filter = None;
                self.follow_top = selected.is_none();
                // Start on the top row: the newest running agent when any runs.
                self.selected = selected.or_else(|| {
                    self.list_view(&state.agents, now_ms(), false, self.filter())
                        .order
                        .first()
                        .map(|&i| state.agents[i].session.clone())
                });
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
                self.config["user"]["label"]["text"] = self
                    .text("incoming_label", "Incoming message")
                    .to_owned()
                    .into();
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
        if self.matches("back", key) {
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
        if self.matches("previous_agent", key) || self.matches("next_agent", key) {
            self.navigate(if self.matches("previous_agent", key) {
                -1
            } else {
                1
            });
            return KeyOutcome::consumed();
        }
        let Some(session) = detail else {
            match key.code {
                _ if self.matches("list_up", key) => self.navigate(-1),
                _ if self.matches("list_down", key) => self.navigate(1),
                _ if self.matches("filter", key) => self.toggle_filter(self.filter().next()),
                _ if self.matches("show_all", key) => {
                    let default = match self.default_filter() {
                        ListFilter::All => ListFilter::Recent,
                        other => other,
                    };
                    self.toggle_filter(if self.filter() == ListFilter::All {
                        default
                    } else {
                        ListFilter::All
                    })
                }
                _ if self.matches("open_detail", key) => {
                    let mut state = self.state.0.lock().expect("agent monitor lock");
                    let first = self
                        .list_view(&state.agents, now_ms(), false, self.filter())
                        .order
                        .first()
                        .map(|&i| state.agents[i].session.clone());
                    let selected = self.selected.clone().or(first);
                    self.follow_top = false;
                    if let Some((_, detail)) = &mut state.requested {
                        *detail = selected;
                    }
                }
                _ => {}
            }
            return KeyOutcome::consumed();
        };
        if self.matches("metadata_up", key) || self.matches("metadata_down", key) {
            let up = self.matches("metadata_up", key);
            let lines = self.number("metadata_scroll_lines", 3);
            let offset = self.metadata_scroll.entry(session.clone()).or_default();
            *offset = if up {
                offset.saturating_sub(lines)
            } else {
                offset.saturating_add(lines)
            };
            return KeyOutcome::consumed();
        }
        let page_lines = self.number("page_lines", 10);
        let page_up = self.matches("page_up", key);
        let page_down = self.matches("page_down", key);
        let follow = self.matches("follow", key);
        let scroll_up = self.matches("scroll_up", key);
        let scroll_down = self.matches("scroll_down", key);
        let toggle_tool = self.matches("toggle_tool", key);
        let mut state = self.state.0.lock().expect("agent monitor lock");
        if state.root != ctx.model.session || !state.agents.iter().any(|a| a.session == session) {
            return KeyOutcome::consumed();
        }
        let target = state.session(&session);
        let chat = Self::chat(&mut self.chats, &self.config, target, ctx.theme);
        let scroll = &mut target.model.scroll_from_bottom;
        match key.code {
            _ if page_up => *scroll = scroll.saturating_add(page_lines),
            _ if page_down => *scroll = scroll.saturating_sub(page_lines),
            _ if follow => *scroll = 0,
            _ if scroll_up && !chat.captures_input() => *scroll = scroll.saturating_add(1),
            _ if scroll_down && !chat.captures_input() => *scroll = scroll.saturating_sub(1),
            _ => {
                let child_ctx = Ctx {
                    model: &target.model,
                    theme: ctx.theme,
                };
                let outcome = if !chat.captures_input() && toggle_tool {
                    chat.on_binding(&child_ctx, "toggle_tool")
                } else if !chat.captures_input()
                    && chat
                        .bindings()
                        .iter()
                        .any(|binding| binding.matches("toggle_tool", &key))
                {
                    KeyOutcome::consumed()
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
            .style(self.style(ctx.theme, "frame", ctx.theme.overlay))
            .border_style(self.style(ctx.theme, "border", ctx.theme.overlay_border))
            .title(Line::styled(
                {
                    let running = state
                        .agents
                        .iter()
                        .filter(|a| a.status == "running")
                        .count();
                    let mut title = format!(" {} · ", self.text("title", "Agents"));
                    if running > 0 {
                        title.push_str(&format!("{running} running · "));
                    }
                    title.push_str(&format!("{} · read-only ", state.agents.len()));
                    title
                },
                self.style(ctx.theme, "heading", ctx.theme.heading),
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
                Paragraph::new(self.text(
                    "waiting",
                    "Waiting for an authorized agent entry (or agent not found).",
                ))
                .style(ctx.theme.dim)
                .wrap(Wrap { trim: false })
                .render(body, buf);
                Paragraph::new(self.hint("back", "list"))
                    .style(self.style(ctx.theme, "hint", ctx.theme.dim))
                    .render(hint, buf);
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
            let metadata_height = (task_rows.min(usize::from(self.number("metadata_rows", 3)))
                as u16)
                .min(body.height.saturating_sub(1) / 3);
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
                self.text("following", "Following")
            } else if new_activity {
                self.text("paused_new", "Paused + new")
            } else {
                self.text("paused", "Paused")
            };
            let help = [
                self.hint("back", "list"),
                follow.to_owned(),
                self.hint("follow", "follow"),
                self.hint("page_up", "scroll up"),
                self.hint("page_down", "scroll down"),
                self.hint("toggle_tool", "tools"),
                self.hint("metadata_down", "metadata"),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
            Paragraph::new(help)
                .style(self.style(ctx.theme, "hint", ctx.theme.dim))
                .render(hint, buf);
        } else {
            let now = now_ms();
            let view = self.list_view(&state.agents, now, self.group(), self.filter());
            let in_view = |session: Option<&String>| {
                view.order
                    .iter()
                    .any(|&i| Some(&state.agents[i].session) == session)
            };
            if self.follow_top || !in_view(self.selected.as_ref()) {
                self.selected = view.order.first().map(|&i| state.agents[i].session.clone());
            }
            let index = view
                .rows
                .iter()
                .position(|row| {
                    matches!(row, ListRow::Agent(i)
                        if Some(&state.agents[*i].session) == self.selected.as_ref())
                })
                .unwrap_or(0);
            let visible = usize::from(body.height).max(1);
            // Keep the selected row's section header and a trailing note visible.
            let mut top = index;
            if top > 0 && matches!(view.rows[top - 1], ListRow::Section(..)) {
                top -= 1;
            }
            self.list_offset = self.list_offset.min(top);
            let mut bottom = index;
            if view
                .rows
                .get(index + 1)
                .is_some_and(|r| matches!(r, ListRow::Note(_)))
            {
                bottom += 1;
            }
            if bottom >= self.list_offset + visible {
                self.list_offset = (bottom + 1).saturating_sub(visible).min(index);
            }
            let home = std::env::var("HOME").ok();
            let width = usize::from(body.width);
            let section_style = self.style(ctx.theme, "section", ctx.theme.heading);
            let lines: Vec<Line> = if state.agents.is_empty() {
                vec![Line::raw(self.text("empty", "No agents published yet."))]
            } else {
                view.rows
                    .iter()
                    .skip(self.list_offset)
                    .take(visible)
                    .map(|row| match row {
                        ListRow::Section(label, count) => Line::from(vec![Span::styled(
                            format!("{label} · {count}"),
                            section_style,
                        )]),
                        ListRow::Note(text) => {
                            Line::from(vec![Span::styled(format!("  {text}"), ctx.theme.dim)])
                        }
                        ListRow::Agent(i) => {
                            let a = &state.agents[*i];
                            self.agent_row(ctx, a, width, now, home.as_deref(), &state)
                        }
                    })
                    .collect()
            };
            Paragraph::new(lines).render(body, buf);
            let filter = if self.filter() == self.default_filter() {
                self.hint("filter", "filter")
            } else {
                self.hint("filter", &format!("filter: {}", self.filter().name()))
            };
            Paragraph::new(
                [
                    self.hint("list_up", "previous"),
                    self.hint("list_down", "next"),
                    self.hint("open_detail", "detail"),
                    filter,
                    self.hint("back", "close"),
                ]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            )
            .style(self.style(ctx.theme, "hint", ctx.theme.dim))
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
            ..Default::default()
        }
    }
    fn event(id: &str, text: &str) -> Arc<Envelope> {
        Arc::new(Envelope {
            id: id.into(),
            at: String::new(),
            event: SessionEvent::AssistantMessage(AssistantMessage {
                model: "m".into(),
                content: vec![ContentPart::Text { text: text.into() }],
                stop: StopReason::EndTurn,
                usage: Usage::default(),
                estimated_input: 0,
                chunks: vec![],
            }),
        })
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
    fn monitor_options_rebind_disable_render_and_keep_child_scroll_isolated() {
        let state = AgentMonitorState::default();
        state.publish_agents("root", vec![info("a"), info("b")]);
        let mut drawer = Agents::new(state.clone());
        let model = Model::new("root".into(), String::new());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        drawer.on_action(
            &ctx,
            "chat:messagebox-config",
            &serde_json::json!({
                "agents": {
                    "keys": {"back":"q", "list_down":["n", "ctrl+j"], "next_agent":false,
                        "page_up":"u", "toggle_tool":false},
                    "layout": {"list_rows":1, "page_lines":7, "wheel_lines":2, "metadata_rows":0},
                    "text": {"title":"Workers", "incoming_label":"Assignment"},
                    "styles": {"border":{"fg":"red"}}
                }
            }),
        );
        assert_eq!(drawer.config["user"]["label"]["text"], "Assignment");
        assert_eq!(drawer.config["keys"]["selection_editor"], false);
        open(&mut drawer, &ctx, None);
        assert_eq!(drawer.height(&ctx, 80), Some(4));
        key(&mut drawer, &ctx, KeyCode::Down);
        key(&mut drawer, &ctx, KeyCode::Tab);
        assert_eq!(drawer.selected.as_deref(), Some("a"));
        key(&mut drawer, &ctx, KeyCode::Char('n'));
        assert_eq!(drawer.selected.as_deref(), Some("b"));
        let area = Rect::new(0, 0, 90, 20);
        let mut buf = Buffer::empty(area);
        drawer.render(&ctx, area, &mut buf);
        let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(
            text.contains("Workers") && text.contains("q close") && !text.contains("Esc close")
        );
        assert_eq!(buf[(0, 1)].fg, ratatui::style::Color::Red);
        assert!(!drawer
            .binding_help()
            .iter()
            .any(|s| s.contains("next_agent")));
        key(&mut drawer, &ctx, KeyCode::Enter);
        key(&mut drawer, &ctx, KeyCode::PageUp);
        key(&mut drawer, &ctx, KeyCode::Char('u'));
        drawer.on_action(&ctx, "viewport:wheel", &serde_json::json!({"up":true}));
        assert_eq!(
            state.0.lock().unwrap().sessions["b"]
                .model
                .scroll_from_bottom,
            9
        );
        assert_eq!(model.scroll_from_bottom, 0);
        key(&mut drawer, &ctx, KeyCode::Esc);
        assert_eq!(state.requested().unwrap().1.as_deref(), Some("b"));
        key(&mut drawer, &ctx, KeyCode::Char('q'));
        assert_eq!(state.requested().unwrap().1, None);
        for width in [0, 1, 8, 60] {
            let area = Rect::new(0, 0, width, 3);
            drawer.render(&ctx, area, &mut Buffer::empty(area));
        }
        drawer.on_action(&ctx, "chat:messagebox-config", &serde_json::json!({}));
        assert_eq!(drawer.config["user"]["label"]["text"], "Incoming message");
        key(&mut drawer, &ctx, KeyCode::Esc);
        assert!(state.requested().is_none());
        assert!(drawer.matches(
            "previous_agent",
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)
        ));
    }

    #[test]
    fn waiting_detail_renders_configured_back_hint() {
        let mut drawer = Agents::new(AgentMonitorState::default());
        let model = Model::new("root".into(), String::new());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        drawer.on_action(
            &ctx,
            "chat:messagebox-config",
            &serde_json::json!({
                "agents": {
                    "keys": {"back": "q"},
                    "styles": {"hint": {"fg": "red"}}
                }
            }),
        );
        open(&mut drawer, &ctx, Some("missing"));
        let area = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(area);
        drawer.render(&ctx, area, &mut buf);
        let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(text.contains("Waiting for an authorized agent entry"));
        assert!(!text.contains("Esc"));
        let footer = (1..79).map(|x| buf[(x, 8)].symbol()).collect::<String>();
        assert_eq!(footer.trim(), "q list");
        assert_eq!(buf[(1, 8)].fg, ratatui::style::Color::Red);
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
        // One "Running · 1" section header plus the agent row, border and footer.
        assert_eq!(drawer.height(&ctx, 80), Some(5));
    }

    fn finished(session: &str, ended_ago_s: u64, now: u64) -> AgentInfo {
        AgentInfo {
            status: "finished".into(),
            started_ms: Some(now - ended_ago_s * 1000 - 5000),
            ended_ms: Some(now - ended_ago_s * 1000),
            ..info(session)
        }
    }

    fn screen(drawer: &mut Agents, ctx: &Ctx<'_>, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        drawer.render(ctx, area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn list_puts_running_first_hides_old_finished_and_filters() {
        let now = now_ms();
        let mut agents = vec![
            finished("a1", 7200, now),
            finished("a2", 60, now),
            AgentInfo {
                started_ms: Some(now - 30_000),
                task: "In the rness codebase at /tmp/x, review the parser.\nMore.".into(),
                ..info("a3")
            },
            finished("a4", 3 * 86_400, now),
            finished("a5", 4 * 86_400, now),
            AgentInfo {
                status: "error".into(),
                ..finished("a6", 120, now)
            },
        ];
        agents.push(AgentInfo {
            started_ms: Some(now - 1000),
            ..info("a7")
        });
        let state = AgentMonitorState::default();
        state.publish_agents("root", agents);
        let mut drawer = Agents::new(state.clone());
        let model = Model::new("root".into(), String::new());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        drawer.on_action(
            &ctx,
            "chat:messagebox-config",
            &serde_json::json!({"agents":{"list":{"recent":{"secs":600,"min":1,"max":5}}}}),
        );
        open(&mut drawer, &ctx, None);
        // Cursor starts on the newest running agent.
        assert_eq!(drawer.selected.as_deref(), Some("a7"));
        // Running(2) + header + recent(a2, a6) + header + hidden note.
        assert_eq!(drawer.height(&ctx, 80), Some(7 + 3));
        let lines = screen(&mut drawer, &ctx, 100, 10);
        assert!(lines[0].contains("2 running · 7 · read-only"), "{lines:?}");
        assert!(lines[1].contains("Running · 2"), "{lines:?}");
        assert!(lines[2].starts_with("│› ● a7"), "{lines:?}");
        assert!(
            lines[3].contains("a3") && lines[3].contains("Review the parser."),
            "{lines:?}"
        );
        assert!(
            !lines[3].contains("codebase") && !lines[3].contains("More"),
            "{lines:?}"
        );
        assert!(lines[4].contains("Recent · 2"), "{lines:?}");
        assert!(
            lines[5].contains("✓ a2") && lines[5].contains("1m ago") && lines[5].contains("done")
        );
        assert!(
            lines[6].contains("✗ a6") && lines[6].contains("2m ago") && lines[6].contains("error")
        );
        assert!(
            lines[7].contains("… 3 older hidden · a show all"),
            "{lines:?}"
        );
        assert!(
            lines[8].contains("f filter") && lines[8].contains("Esc close"),
            "{lines:?}"
        );

        // Navigation follows display order and skips headers.
        for _ in 0..3 {
            key(&mut drawer, &ctx, KeyCode::Down);
        }
        assert_eq!(drawer.selected.as_deref(), Some("a6"));
        key(&mut drawer, &ctx, KeyCode::Down);
        assert_eq!(drawer.selected.as_deref(), Some("a6"));

        // a shows everything, newest finished first.
        key(&mut drawer, &ctx, KeyCode::Char('a'));
        assert_eq!(drawer.height(&ctx, 80), Some(12));
        let all = screen(&mut drawer, &ctx, 100, 12);
        assert!(all[4].contains("Finished · 5"), "{all:?}");
        let order: Vec<_> = all[5..10]
            .iter()
            .map(|l| l.split_whitespace().nth(2).unwrap_or_default().to_owned())
            .collect();
        assert_eq!(order, ["a2", "a6", "a1", "a4", "a5"], "{all:?}");
        assert!(
            all[9].contains("4d ago") && all[10].contains("f filter: all"),
            "{all:?}"
        );

        // f cycles all → running only; finished are summarized.
        key(&mut drawer, &ctx, KeyCode::Char('f'));
        assert_eq!(drawer.height(&ctx, 80), Some(7));
        let active = screen(&mut drawer, &ctx, 100, 7);
        assert!(
            active[3].contains("a3") && active[4].contains("5 finished hidden"),
            "{active:?}"
        );
        assert!(active[5].contains("f filter: running"), "{active:?}");
        // The selection fell outside the filter and moved to the top row.
        assert_eq!(drawer.selected.as_deref(), Some("a7"));
        key(&mut drawer, &ctx, KeyCode::Char('f'));
        assert!(screen(&mut drawer, &ctx, 100, 10)[4].contains("Recent · 2"));

        // Reopening restores the configured default filter.
        key(&mut drawer, &ctx, KeyCode::Char('a'));
        key(&mut drawer, &ctx, KeyCode::Esc);
        open(&mut drawer, &ctx, None);
        assert_eq!(drawer.height(&ctx, 80), Some(10));

        // Nothing recent: the note names the recent window, not "running".
        let quiet = AgentMonitorState::default();
        quiet.publish_agents("root", vec![finished("old", 7200, now)]);
        let mut quiet_drawer = Agents::new(quiet);
        quiet_drawer.on_action(
            &ctx,
            "chat:messagebox-config",
            &serde_json::json!({"agents":{"list":{"filter":"all","recent":{"secs":60,"min":0,"max":5}}}}),
        );
        open(&mut quiet_drawer, &ctx, None);
        assert!(screen(&mut quiet_drawer, &ctx, 100, 6)[2].contains("old"));
        // With an "all" default, a still toggles to the recent window.
        key(&mut quiet_drawer, &ctx, KeyCode::Char('a'));
        let recent = screen(&mut quiet_drawer, &ctx, 100, 6);
        assert!(
            recent[1].contains("Nothing running or recently finished."),
            "{recent:?}"
        );
        // Narrow panels keep icon, alias, role and status; never panic.
        let narrow = screen(&mut drawer, &ctx, 40, 10);
        assert!(
            narrow[2].contains("a7") && narrow[2].contains("running"),
            "{narrow:?}"
        );
        assert!(!narrow[5].contains("ago"), "{narrow:?}");
        for width in [0, 1, 8, 20, 59, 61] {
            screen(&mut drawer, &ctx, width, 6);
        }

        // Configured order, grouping and filter.
        drawer.on_action(
            &ctx,
            "chat:messagebox-config",
            &serde_json::json!({"agents":{"list":{"filter":"all","group":false,"order":"oldest"},
                "keys":{"show_all":false}}}),
        );
        open(&mut drawer, &ctx, None);
        let plain = screen(&mut drawer, &ctx, 100, 10);
        assert!(
            plain[1].contains("a3") && plain[2].contains("a7"),
            "{plain:?}"
        );
        assert!(
            plain[3].contains("a5") && plain[7].contains("a2"),
            "{plain:?}"
        );
        assert!(!plain.join("\n").contains("Running ·"), "{plain:?}");
    }

    #[test]
    fn cursor_follows_top_row_until_the_user_moves_it() {
        let now = now_ms();
        let state = AgentMonitorState::default();
        let mut drawer = Agents::new(state.clone());
        let model = Model::new("root".into(), String::new());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        // Opened before the host publishes (a stale list was empty).
        open(&mut drawer, &ctx, None);
        state.publish_agents(
            "root",
            vec![finished("old", 600, now), finished("new", 5, now)],
        );
        screen(&mut drawer, &ctx, 100, 8);
        assert_eq!(drawer.selected.as_deref(), Some("new"));
        // A newly started agent becomes the top row and takes the cursor.
        let running = AgentInfo {
            started_ms: Some(now),
            ..info("live")
        };
        state.publish_agents(
            "root",
            vec![
                finished("old", 600, now),
                finished("new", 5, now),
                running.clone(),
            ],
        );
        screen(&mut drawer, &ctx, 100, 8);
        assert_eq!(drawer.selected.as_deref(), Some("live"));
        // Once the user picks a row it stays put through later publications.
        key(&mut drawer, &ctx, KeyCode::Down);
        assert_eq!(drawer.selected.as_deref(), Some("new"));
        let second = AgentInfo {
            started_ms: Some(now + 1),
            ..info("live2")
        };
        state.publish_agents(
            "root",
            vec![
                finished("old", 600, now),
                finished("new", 5, now),
                running,
                second,
            ],
        );
        screen(&mut drawer, &ctx, 100, 9);
        assert_eq!(drawer.selected.as_deref(), Some("new"));
    }

    #[test]
    fn short_task_and_ages() {
        assert_eq!(
            short_task(
                "In the rness codebase at /Users/me/rness, find X.",
                Some("/Users/me")
            ),
            "Find X."
        );
        assert_eq!(
            short_task("\n  Check /Users/me/a   now\nsecond", Some("/Users/me")),
            "Check ~/a now"
        );
        assert_eq!(
            short_task("In the end, it works", None),
            "In the end, it works"
        );
        // The comma must end the path; otherwise the task text is kept whole.
        assert_eq!(
            short_task(
                "In the rness codebase at /Users/me/rness. Review a.rs, then report.",
                Some("/Users/me")
            ),
            "In the rness codebase at ~/rness. Review a.rs, then report."
        );
        // Home is replaced only as a whole path prefix.
        assert_eq!(
            short_task(
                "cat /Users/meg/x /srv/Users/me/y /Users/me",
                Some("/Users/me")
            ),
            "cat /Users/meg/x /srv/Users/me/y ~"
        );
        assert_eq!(short_age(59_000), "59s");
        assert_eq!(short_age(3_600_000), "1h");
        assert_eq!(short_age(2 * 86_400_000), "2d");
        assert_eq!(status_icon("interrupted"), "⊘");
        let theme = Theme::default();
        assert_eq!(status_style("finished", &theme), theme.added);
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
        let collapsed_config = serde_json::json!({"tool":{"display":"preview","preview_lines":1},"keys":{"selection_editor":"x"},"agents":{"keys":{"toggle_tool":false}}});
        drawer.on_action(&ctx, "chat:messagebox-config", &collapsed_config);
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        drawer.render(&ctx, area, &mut buf);
        key(&mut drawer, &ctx, KeyCode::Enter);
        drawer.on_key(
            &ctx,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        );
        drawer.render(&ctx, area, &mut buf);
        let screen = buf
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            !screen.contains("six"),
            "disabled tool binding expanded output: {screen}"
        );
        let mut rebound = collapsed_config;
        rebound["agents"]["keys"]["toggle_tool"] = "t".into();
        drawer.on_action(&ctx, "chat:messagebox-config", &rebound);
        drawer.render(&ctx, area, &mut buf);
        assert!(key(&mut drawer, &ctx, KeyCode::Char('t'))
            .actions
            .is_empty());
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

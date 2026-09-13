//! Reduction at writer-owned model-step boundaries. Policy is supplied by Lua.
use crate::session::{branch::SessionStore, log::SessionLog, projection::{ModelContext, ModelTurn}, replay::replay};
use crate::tools::ToolSpec;
use crate::turn::{provider::{Provider, StepOutcome, StepRequest}, TurnError};
use rness_protocol::events::{Compaction, ContentPart, SessionEvent, Prune};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub meter: Meter,
    #[serde(default)]
    pub summary_selection: Option<rness_protocol::events::ModelSelection>,
    pub threshold_tokens: u64,
    pub retain_tokens: u64,
    pub summary_tokens: u32,
    pub system_prompt: String,
    pub prompt: String,
    pub max_overflow_retries: u32,
    pub max_compactions: u32,
    pub prune_threshold: usize,
    pub prune_head: usize,
    pub prune_tail: usize,
}
impl Policy {
    pub fn validate(&self) -> Result<(), String> {
        if self.meter.bytes_per_token == 0 || self.meter.bytes_per_token > 16
            || self.meter.message_tokens > 1_000_000 || self.meter.image_tokens > 1_000_000
            || self.meter.output_reserve >= self.threshold_tokens {
            return Err("invalid route measurement budgets".into());
        }
        if self.summary_selection.as_ref().is_some_and(|s| s.route.trim().is_empty() || s.model.trim().is_empty()) {
            return Err("summary selection requires route and model".into());
        }
        if self.threshold_tokens == 0 || self.retain_tokens >= self.threshold_tokens
            || self.summary_tokens == 0 || self.system_prompt.trim().is_empty() || self.prompt.trim().is_empty()
            || self.max_compactions == 0
            || self.max_compactions > 10 || self.max_overflow_retries > 10
            || self.prune_head.saturating_add(self.prune_tail).saturating_add(128) >= self.prune_threshold {
            return Err("invalid compaction prompts, budgets, or retry limits".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meter {
    pub bytes_per_token: u64,
    pub message_tokens: u64,
    pub image_tokens: u64,
    pub output_reserve: u64,
}
impl Default for Meter {
    fn default() -> Self {
        Self { bytes_per_token: 4, message_tokens: 8, image_tokens: 4096, output_reserve: 0 }
    }
}
impl Meter {
    fn text(&self, text: &str) -> u64 { (text.len() as u64).div_ceil(self.bytes_per_token.max(1)) }
    fn part(&self, part: &ContentPart) -> u64 {
        match part {
            ContentPart::Image { .. } => self.image_tokens,
            _ => self.text(&serde_json::to_string(part).expect("content serialization")),
        }
    }
    fn turn(&self, turn: &ModelTurn) -> u64 {
        self.message_tokens + match turn {
            ModelTurn::User { content } | ModelTurn::Assistant { content } => content.iter().map(|p| self.part(p)).sum::<u64>(),
            ModelTurn::ToolResults { results } => results.iter().map(|r| {
                self.text(&r.output) + self.text(&r.call) + self.text(&r.name)
                    + r.content.iter().filter(|p| matches!(p, rness_protocol::events::ToolResultContentPart::Image { .. })).count() as u64 * self.image_tokens
            }).sum::<u64>(),
        }
    }
    pub fn measure(&self, context: &ModelContext, system: &str, tools: &[ToolSpec]) -> u64 {
        context.turns.iter().map(|t| self.turn(t)).sum::<u64>() + self.text(system)
            + tools.iter().map(|t| self.text(&t.name) + self.text(&t.description)
                + self.text(&t.input_schema.to_string()) + self.message_tokens).sum::<u64>()
    }
    pub fn pressure(&self, context: &ModelContext, system: &str, tools: &[ToolSpec]) -> u64 {
        self.measure(context, system, tools).saturating_add(self.output_reserve.max(u64::from(context.config.max_output_tokens.unwrap_or(0))))
    }
}
/// Default heuristic retained for callers without an explicit route policy.
pub fn measure(context: &ModelContext, system: &str, tools: &[ToolSpec]) -> u64 {
    Meter::default().measure(context, system, tools)
}

// ModelContext sources has one id per message, but one per RESULT in a group.
fn source_count(turn: &ModelTurn) -> usize {
    match turn { ModelTurn::ToolResults { results } => results.len(), _ => 1 }
}
fn prefix(context: &ModelContext, retain: u64, meter: &Meter) -> usize {
    let mut kept = 0;
    let mut cut = context.turns.len();
    while cut > 0 && (kept < retain || cut == context.turns.len()) {
        cut -= 1;
        kept += meter.turn(&context.turns[cut]);
    }
    // Never split an assistant's calls from the following results.
    if cut < context.turns.len() && matches!(context.turns[cut], ModelTurn::ToolResults { .. }) {
        cut = cut.saturating_sub(1);
    }
    cut
}

/// Close interrupted audit spans on the existing session writer. Never repeat
/// a paid request or discard a checkpoint that landed before the interruption.
pub fn recover(log: &mut SessionLog) -> Result<(), crate::session::log::LogError> {
    let history = log.read_all()?;
    let finished: std::collections::HashSet<_> = history.iter().filter_map(|e| match &e.event {
        SessionEvent::CompactionFinished { started, .. } => Some(started.clone()), _ => None,
    }).collect();
    for (index, event) in history.iter().enumerate() {
        if !matches!(event.event, SessionEvent::CompactionStarted { .. }) || finished.contains(&event.id) { continue; }
        let committed = history[index + 1..].iter()
            .take_while(|e| !matches!(e.event, SessionEvent::CompactionStarted { .. }))
            .any(|e| matches!(e.event, SessionEvent::Compaction(_)));
        log.append(&SessionEvent::CompactionFinished { started: event.id.clone(),
            outcome: if committed { "committed_before_interruption" } else { "interrupted" }.into(),
            usage: Default::default(), chunks: vec![] })?;
    }
    Ok(())
}

/// Returns true only after a durable model-visible reduction. Runs on the
/// turn's existing log writer; never opens a competing session writer.
#[allow(clippy::too_many_arguments)]
pub async fn reduce(store: &SessionStore, log: &mut SessionLog, provider: &dyn Provider,
    system: &str, tools: &[ToolSpec], policy: &Policy, overflow: bool, cancel: &CancellationToken,
) -> Result<bool, TurnError> {
    reduce_with_progress(store, log, provider, system, tools, policy, overflow, cancel, &|_| {}).await
}

#[allow(clippy::too_many_arguments)]
pub async fn reduce_with_progress(store: &SessionStore, log: &mut SessionLog, provider: &dyn Provider,
    system: &str, tools: &[ToolSpec], policy: &Policy, overflow: bool, cancel: &CancellationToken,
    progress: &(dyn Fn(CompactionProgress) + Send + Sync),
) -> Result<bool, TurnError> {
    reduce_region_with_progress(store, log, provider, system, tools, policy, overflow, None, cancel, progress).await
}

pub enum CompactionProgress {
    Measuring { estimated_tokens: u64, threshold_tokens: u64 },
    Summarizing { events: usize, estimated_tokens: u64 },
}

impl CompactionProgress {
    pub fn frame(self, session: rness_protocol::events::SessionId) -> rness_protocol::frames::Frame {
        use rness_protocol::frames::Frame;
        match self {
            Self::Measuring { estimated_tokens, threshold_tokens } => Frame::ContextUsage {
                session, estimated_tokens, threshold_tokens: Some(threshold_tokens),
            },
            Self::Summarizing { events, estimated_tokens } => Frame::CompactionStarted {
                session, events, estimated_tokens,
            },
        }
    }
}

/// Explicit half-open region of model messages; endpoints preserve tool pairs.
#[allow(clippy::too_many_arguments)]
pub async fn reduce_region(store: &SessionStore, log: &mut SessionLog, provider: &dyn Provider,
    system: &str, tools: &[ToolSpec], policy: &Policy, overflow: bool,
    region: Option<std::ops::Range<usize>>, cancel: &CancellationToken,
) -> Result<bool, TurnError> {
    reduce_region_with_progress(store, log, provider, system, tools, policy, overflow, region, cancel, &|_| {}).await
}

#[allow(clippy::too_many_arguments)]
pub async fn reduce_region_with_progress(store: &SessionStore, log: &mut SessionLog, provider: &dyn Provider,
    system: &str, tools: &[ToolSpec], policy: &Policy, overflow: bool,
    region: Option<std::ops::Range<usize>>, cancel: &CancellationToken,
    progress: &(dyn Fn(CompactionProgress) + Send + Sync),
) -> Result<bool, TurnError> {
    policy.validate().map_err(|last| TurnError::ModelExhausted { attempts: 0, last })?;
    let mut changed = false;
    for _ in 0..policy.max_compactions {
        if cancel.is_cancelled() { return Ok(changed); }
        let mut replayed = replay(store, log.session())?;
        if let Some(r) = &region {
            if r.start >= r.end || r.end > replayed.context.turns.len()
                || matches!(replayed.context.turns.get(r.start), Some(ModelTurn::ToolResults { .. }))
                || matches!(replayed.context.turns.get(r.end), Some(ModelTurn::ToolResults { .. })) {
                return Err(TurnError::ModelExhausted { attempts: 0, last: "invalid compaction region or split tool pair".into() });
            }
        }
        let cut = prefix(&replayed.context, policy.retain_tokens, &policy.meter);
        let n: usize = replayed.context.turns[..cut].iter().map(source_count).sum();
        let eligible: std::collections::HashSet<_> = replayed.context.sources[..n].iter().cloned().collect();
        for event in &replayed.history {
            if region.is_some() { break; }
            if cancel.is_cancelled() { return Ok(changed); }
            if !eligible.contains(&event.id) { continue; }
            let SessionEvent::ToolResult(result) = &event.event else { continue };
            if result.content.iter().any(|p| matches!(p, rness_protocol::events::ToolResultContentPart::Image { .. })) { continue; }
            let chars = result.output.chars().count();
            if chars <= policy.prune_threshold { continue; }
            let head: String = result.output.chars().take(policy.prune_head).collect();
            let tail: String = result.output.chars().skip(chars - policy.prune_tail).collect();
            let mut replacement = result.clone();
            replacement.content.clear();
            replacement.output = format!("{head}\n[tool result middle pruned]\n{tail}");
            log.append(&SessionEvent::Prune(Prune { replaces: event.id.clone(), result: replacement }))?;
            changed = true;
        }
        replayed = replay(store, log.session())?;
        let pressure = policy.meter.pressure(&replayed.context, system, tools);
        // Manual regions lack the main request's system/tools; do not publish
        // their partial measurement as the automatic compaction estimate.
        if region.is_none() {
            progress(CompactionProgress::Measuring { estimated_tokens: pressure, threshold_tokens: policy.threshold_tokens });
        }
        if region.is_none() && ((!overflow && pressure < policy.threshold_tokens) || (changed && overflow)) { break; }
        let cut = region.as_ref().map_or_else(|| prefix(&replayed.context, policy.retain_tokens, &policy.meter), |r| r.end);
        let start = region.as_ref().map_or(0, |r| r.start);
        if cut == 0 { break; }
        let first: usize = replayed.context.turns[..start].iter().map(source_count).sum();
        let n: usize = replayed.context.turns[..cut].iter().map(source_count).sum();
        let mut selected: std::collections::HashSet<_> = replayed.context.sources[first..n].iter().cloned().collect();
        // Checkpoints and prunes may themselves be folded. Claim their ancestors
        // so older full-fidelity content cannot reappear on subsequent replay.
        loop {
            let before = selected.len();
            for e in &replayed.history {
                if selected.contains(&e.id) {
                    match &e.event {
                        SessionEvent::Compaction(c) => selected.extend(c.replaces.iter().cloned()),
                        SessionEvent::Prune(p) => { selected.insert(p.replaces.clone()); },
                        _ => {}
                    }
                }
            }
            if before == selected.len() { break; }
        }
        let replaces: Vec<_> = replayed.history.iter().filter(|e| selected.contains(&e.id)).map(|e| e.id.clone()).collect();
        let mut context = ModelContext { turns: replayed.context.turns[start..cut].to_vec(), ..Default::default() };
        let before = policy.meter.measure(&context, "", &[]);
        progress(CompactionProgress::Summarizing { events: replaces.len(), estimated_tokens: before });
        context.config = if let Some(selection) = &policy.summary_selection {
            rness_protocol::events::CallConfig { selection: Some(selection.clone()), ..Default::default() }
        } else { replayed.context.config.clone() };
        if provider.summary_supports_max_output_tokens() {
            context.config.max_output_tokens = Some(policy.summary_tokens);
        } else {
            context.config.max_output_tokens = None;
        }
        context.turns.push(ModelTurn::User { content: vec![ContentPart::Text { text: policy.prompt.clone() }] });
        let request = StepRequest { context: &context, system: &policy.system_prompt, tools: &[], on_delta: None };
        let started = log.append(&SessionEvent::CompactionStarted {
            model: provider.summary_model().into(), sources: replayed.context.sources[first..n].to_vec(),
            estimated_input: measure(&context, request.system, &[]),
            request: serde_json::json!({"system": request.system, "turns": context.turns,
                "config": context.config, "tools": []}),
        })?;
        let (sender, mut captures) = tokio::sync::mpsc::channel::<crate::turn::provider::WireCapture>(1);
        let call = crate::turn::provider::WIRE_CAPTURE.scope(sender, provider.summarize_step(request, cancel));
        tokio::pin!(call);
        let outcome = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break StepOutcome::Cancelled { partial: vec![] },
                Some(capture) = captures.recv() => {
                    match log.append(&SessionEvent::CompactionRequest { started: started.id.clone(), body: capture.body }) {
                        Ok(_) => { let _ = capture.ack.send(Ok(())); }
                        Err(error) => {
                            let _ = capture.ack.send(Err(error.to_string()));
                            return Err(error.into());
                        }
                    }
                }
                result = &mut call => break result,
            }
        };
        let (usage, chunks) = match &outcome {
            StepOutcome::Committed(m) => (m.usage, m.chunks.clone()),
            StepOutcome::Cancelled { partial } | StepOutcome::Failed { partial, .. } => (Default::default(), partial.clone()),
        };
        let message = match outcome {
            StepOutcome::Committed(message) => message,
            other => {
                let outcome = match other {
                    StepOutcome::Failed { error, .. } => format!("failed: {}: {}", error.code, error.message),
                    _ => "cancelled".into(),
                };
                log.append(&SessionEvent::CompactionFinished { started: started.id, outcome, usage, chunks })?;
                break;
            }
        };
        let summary = message.content.iter().filter_map(|p| match p { ContentPart::Text { text } => Some(text.as_str()), _ => None }).collect::<Vec<_>>().join("\n");
        let summary_context = ModelContext { turns: vec![ModelTurn::User { content: vec![ContentPart::Text { text: format!("[conversation summary — earlier history compacted]\n{summary}") }] }], ..Default::default() };
        let rejection = if cancel.is_cancelled() { Some("cancelled") }
            else if summary.trim().is_empty() { Some("empty") }
            else if policy.meter.measure(&summary_context, "", &[]) >= before { Some("non_shrinking") }
            else { None };
        if let Some(outcome) = rejection {
            log.append(&SessionEvent::CompactionFinished { started: started.id, outcome: outcome.into(), usage, chunks })?;
            break;
        }
        log.append(&SessionEvent::Compaction(Compaction { replaces, summary, model: provider.summary_model().into() }))?;
        log.append(&SessionEvent::CompactionFinished { started: started.id, outcome: "committed".into(), usage, chunks })?;
        changed = true;
        if overflow || region.is_some() { break; }
    }
    Ok(changed)
}

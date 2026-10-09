//! Reduction at writer-owned model-step boundaries. Policy is supplied by Lua.
use crate::session::{
    branch::SessionStore,
    log::SessionLog,
    projection::{ModelContext, ModelTurn},
    replay::ReplayCache,
};
use crate::tools::ToolSpec;
use crate::turn::{
    provider::{Provider, StepOutcome, StepRequest},
    TurnError,
};
use rness_protocol::events::{Compaction, ContentPart, Prune, SessionEvent};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, remote = "Self")]
pub struct Policy {
    #[serde(default)]
    pub meter: Meter,
    #[serde(default)]
    pub summary_profile: Option<String>,
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
impl serde::Serialize for Policy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Self::serialize(self, serializer)
    }
}
impl<'de> serde::Deserialize<'de> for Policy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("summary_selection").is_some() {
            return Err(serde::de::Error::custom(
                "summary_selection was removed; declare a profile and set summary_profile",
            ));
        }
        Self::deserialize(value).map_err(serde::de::Error::custom)
    }
}
impl Policy {
    pub fn validate(&self) -> Result<(), String> {
        if self.meter.bytes_per_token == 0
            || self.meter.bytes_per_token > 16
            || self.meter.message_tokens > 1_000_000
            || self.meter.image_tokens > 1_000_000
            || self.meter.output_reserve >= self.threshold_tokens
        {
            return Err("invalid route measurement budgets".into());
        }
        if self
            .summary_profile
            .as_ref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err("summary_profile must be a nonempty profile name".into());
        }
        if self.threshold_tokens == 0
            || self.retain_tokens >= self.threshold_tokens
            || self.summary_tokens == 0
            || self.system_prompt.trim().is_empty()
            || self.prompt.trim().is_empty()
            || self.max_compactions == 0
            || self.max_compactions > 10
            || self.max_overflow_retries > 10
            || self
                .prune_head
                .saturating_add(self.prune_tail)
                .saturating_add(128)
                >= self.prune_threshold
        {
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
        Self {
            bytes_per_token: 4,
            message_tokens: 8,
            image_tokens: 4096,
            output_reserve: 0,
        }
    }
}
impl Meter {
    fn text(&self, text: &str) -> u64 {
        (text.len() as u64).div_ceil(self.bytes_per_token.max(1))
    }
    fn part(&self, part: &ContentPart) -> u64 {
        match part {
            ContentPart::Image { .. } => self.image_tokens,
            _ => self.text(&serde_json::to_string(part).expect("content serialization")),
        }
    }
    fn turn(&self, turn: &ModelTurn) -> u64 {
        self.message_tokens
            + match turn {
                ModelTurn::User { content } | ModelTurn::Assistant { content } => {
                    content.iter().map(|p| self.part(p)).sum::<u64>()
                }
                ModelTurn::ToolResults { results } => results
                    .iter()
                    .map(|r| {
                        self.text(&r.output)
                            + self.text(&r.call)
                            + self.text(&r.name)
                            + r.content
                                .iter()
                                .filter(|p| {
                                    matches!(
                                        p,
                                        rness_protocol::events::ToolResultContentPart::Image { .. }
                                    )
                                })
                                .count() as u64
                                * self.image_tokens
                    })
                    .sum::<u64>(),
            }
    }
    pub fn measure(&self, context: &ModelContext, system: &str, tools: &[ToolSpec]) -> u64 {
        context.turns.iter().map(|t| self.turn(t)).sum::<u64>()
            + self.text(system)
            + tools
                .iter()
                .map(|t| {
                    self.text(&t.name)
                        + self.text(&t.description)
                        + self.text(&t.input_schema.to_string())
                        + self.message_tokens
                })
                .sum::<u64>()
    }
    fn reserve(&self, context: &ModelContext) -> u64 {
        self.output_reserve
            .max(u64::from(context.config.max_output_tokens.unwrap_or(0)))
    }
    pub fn pressure(&self, context: &ModelContext, system: &str, tools: &[ToolSpec]) -> u64 {
        self.measure(context, system, tools)
            .saturating_add(self.reserve(context))
    }
    /// Pressure with the input estimate scaled by an observed
    /// real/estimated calibration ratio (see [`calibration`]). The output
    /// reserve is real tokens and is never scaled.
    pub fn calibrated_pressure(
        &self,
        context: &ModelContext,
        system: &str,
        tools: &[ToolSpec],
        ratio: Option<f64>,
    ) -> u64 {
        let measured = self.measure(context, system, tools);
        let measured = ratio.map_or(measured, |r| (measured as f64 * r).round() as u64);
        measured.saturating_add(self.reserve(context))
    }
}

/// Ratio of provider-reported input tokens (including cache reads/writes)
/// to the meter estimate recorded for the same request, from the latest
/// committed step carrying both. The ratio compares one request with its
/// own estimate, so it stays valid across compactions — unlike the raw
/// provider count, which describes a context that may no longer exist.
/// Clamped to guard against degenerate provider reports; `None` until a
/// step records an estimate (legacy logs, fresh sessions).
pub fn calibration(
    history: &[impl std::borrow::Borrow<rness_protocol::events::Envelope>],
) -> Option<f64> {
    history.iter().rev().find_map(|e| match &e.borrow().event {
        SessionEvent::AssistantMessage(m) if m.estimated_input > 0 => {
            let real =
                m.usage.input_tokens + m.usage.cache_read_tokens + m.usage.cache_write_tokens;
            (real > 0).then(|| (real as f64 / m.estimated_input as f64).clamp(0.25, 4.0))
        }
        _ => None,
    })
}
/// Default heuristic retained for callers without an explicit route policy.
pub fn measure(context: &ModelContext, system: &str, tools: &[ToolSpec]) -> u64 {
    Meter::default().measure(context, system, tools)
}

// ModelContext sources has one id per message, but one per RESULT in a group.
fn source_count(turn: &ModelTurn) -> usize {
    match turn {
        ModelTurn::ToolResults { results } => results.len(),
        _ => 1,
    }
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
///
/// Also closes a trailing unclosed turn (a process killed mid-turn leaves
/// `turn/started N` as the last turn marker) with `turn/ended N` outcome
/// `failed`: the turn did not complete and nothing cancelled it, so it reads
/// like any other failed turn. Only the TRAILING turn is closed — a writer
/// holding the session lock never leaves an earlier one open, and appending
/// an end marker for a mid-log turn would misplace it.
pub fn recover(log: &mut SessionLog) -> Result<(), crate::session::log::LogError> {
    let history = log.read_all_elided()?;
    let finished: std::collections::HashSet<_> = history
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::CompactionFinished { started, .. } => Some(started.clone()),
            _ => None,
        })
        .collect();
    for (index, event) in history.iter().enumerate() {
        if !matches!(event.event, SessionEvent::CompactionStarted { .. })
            || finished.contains(&event.id)
        {
            continue;
        }
        let committed = history[index + 1..]
            .iter()
            .take_while(|e| !matches!(e.event, SessionEvent::CompactionStarted { .. }))
            .any(|e| matches!(e.event, SessionEvent::Compaction(_)));
        log.append(&SessionEvent::CompactionFinished {
            started: event.id.clone(),
            outcome: if committed {
                "committed_before_interruption"
            } else {
                "interrupted"
            }
            .into(),
            usage: Default::default(),
            chunks: vec![],
        })?;
    }
    let trailing = history.iter().rev().find_map(|e| match &e.event {
        SessionEvent::TurnStarted { turn } => Some(Some(*turn)),
        SessionEvent::TurnEnded { .. } => Some(None),
        _ => None,
    });
    if let Some(Some(turn)) = trailing {
        log.append(&SessionEvent::TurnEnded {
            turn,
            outcome: rness_protocol::events::TurnOutcome::Failed,
        })?;
    }
    Ok(())
}

/// Returns true only after a durable model-visible reduction. Runs on the
/// turn's existing log writer; never opens a competing session writer.
#[allow(clippy::too_many_arguments)]
pub async fn reduce(
    store: &SessionStore,
    log: &mut SessionLog,
    provider: &dyn Provider,
    system: &str,
    tools: &[ToolSpec],
    policy: &Policy,
    overflow: bool,
    cancel: &CancellationToken,
) -> Result<bool, TurnError> {
    reduce_with_progress(
        store,
        log,
        provider,
        system,
        tools,
        policy,
        overflow,
        cancel,
        &|_| {},
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn reduce_with_progress(
    store: &SessionStore,
    log: &mut SessionLog,
    provider: &dyn Provider,
    system: &str,
    tools: &[ToolSpec],
    policy: &Policy,
    overflow: bool,
    cancel: &CancellationToken,
    progress: &(dyn Fn(CompactionProgress) + Send + Sync),
) -> Result<bool, TurnError> {
    reduce_region_with_progress(
        store, log, provider, system, tools, policy, overflow, None, cancel, progress,
    )
    .await
}

pub enum CompactionProgress {
    Measuring {
        estimated_tokens: u64,
        threshold_tokens: u64,
    },
    Summarizing {
        events: usize,
        estimated_tokens: u64,
    },
}

impl CompactionProgress {
    pub fn frame(
        self,
        session: rness_protocol::events::SessionId,
    ) -> rness_protocol::frames::Frame {
        use rness_protocol::frames::Frame;
        match self {
            Self::Measuring {
                estimated_tokens,
                threshold_tokens,
            } => Frame::ContextUsage {
                session,
                estimated_tokens,
                threshold_tokens: Some(threshold_tokens),
            },
            Self::Summarizing {
                events,
                estimated_tokens,
            } => Frame::CompactionStarted {
                session,
                events,
                estimated_tokens,
            },
        }
    }
}

/// Explicit half-open region of model messages; endpoints preserve tool pairs.
#[allow(clippy::too_many_arguments)]
pub async fn reduce_region(
    store: &SessionStore,
    log: &mut SessionLog,
    provider: &dyn Provider,
    system: &str,
    tools: &[ToolSpec],
    policy: &Policy,
    overflow: bool,
    region: Option<std::ops::Range<usize>>,
    cancel: &CancellationToken,
) -> Result<bool, TurnError> {
    reduce_region_with_progress(
        store,
        log,
        provider,
        system,
        tools,
        policy,
        overflow,
        region,
        cancel,
        &|_| {},
    )
    .await
}

/// Bounds of one prune batch: what a crash or cancel can lose, and the
/// latency of one commit.
const PRUNE_BATCH_EVENTS: usize = 512;
const PRUNE_BATCH_BYTES: usize = 8 << 20;

#[derive(Default)]
struct PruneBatch {
    events: Vec<SessionEvent>,
    bytes: usize,
    committed: bool,
}

impl PruneBatch {
    fn push(&mut self, log: &mut SessionLog, event: SessionEvent) -> Result<(), TurnError> {
        if let SessionEvent::Prune(prune) = &event {
            self.bytes += prune.result.output.len() + prune.replaces.len() + 256;
        }
        self.events.push(event);
        if self.events.len() >= PRUNE_BATCH_EVENTS || self.bytes >= PRUNE_BATCH_BYTES {
            self.flush(log)?;
        }
        Ok(())
    }

    /// Commit pending events; returns whether anything was ever committed.
    fn flush(&mut self, log: &mut SessionLog) -> Result<bool, TurnError> {
        if !self.events.is_empty() {
            log.append_batch(&self.events)?;
            self.events.clear();
            self.bytes = 0;
            self.committed = true;
        }
        Ok(self.committed)
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn reduce_region_with_progress(
    store: &SessionStore,
    log: &mut SessionLog,
    provider: &dyn Provider,
    system: &str,
    tools: &[ToolSpec],
    policy: &Policy,
    overflow: bool,
    region: Option<std::ops::Range<usize>>,
    cancel: &CancellationToken,
    progress: &(dyn Fn(CompactionProgress) + Send + Sync),
) -> Result<bool, TurnError> {
    reduce_cached(
        store,
        log,
        &mut ReplayCache::default(),
        provider,
        system,
        tools,
        policy,
        overflow,
        region,
        cancel,
        progress,
    )
    .await
}

/// [`reduce_region_with_progress`] replaying through the caller's `cache`:
/// a no-op pass costs no replay when the caller already derived this step's
/// context, and the post-prune replay only happens when a prune landed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn reduce_cached(
    store: &SessionStore,
    log: &mut SessionLog,
    cache: &mut ReplayCache,
    provider: &dyn Provider,
    system: &str,
    tools: &[ToolSpec],
    policy: &Policy,
    overflow: bool,
    region: Option<std::ops::Range<usize>>,
    cancel: &CancellationToken,
    progress: &(dyn Fn(CompactionProgress) + Send + Sync),
) -> Result<bool, TurnError> {
    policy
        .validate()
        .map_err(|last| TurnError::ModelExhausted { attempts: 0, last })?;
    let mut changed = false;
    for _ in 0..policy.max_compactions {
        if cancel.is_cancelled() {
            return Ok(changed);
        }
        let mut replayed = cache.get(store, log)?;
        replayed.require_known()?;
        if let Some(r) = &region {
            if r.start >= r.end
                || r.end > replayed.context.turns.len()
                || matches!(
                    replayed.context.turns.get(r.start),
                    Some(ModelTurn::ToolResults { .. })
                )
                || matches!(
                    replayed.context.turns.get(r.end),
                    Some(ModelTurn::ToolResults { .. })
                )
            {
                return Err(TurnError::ModelExhausted {
                    attempts: 0,
                    last: "invalid compaction region or split tool pair".into(),
                });
            }
        }
        let cut = prefix(&replayed.context, policy.retain_tokens, &policy.meter);
        let n: usize = replayed.context.turns[..cut].iter().map(source_count).sum();
        let eligible: std::collections::HashSet<_> =
            replayed.context.sources[..n].iter().cloned().collect();
        // Prunes are independent events: collect and commit them in batches
        // (one durability sync per batch, plan P6). Cancellation flushes what
        // was collected, like the former per-event path kept its progress.
        let mut batch = PruneBatch::default();
        for event in &replayed.history {
            if region.is_some() {
                break;
            }
            if cancel.is_cancelled() {
                changed |= batch.flush(log)?;
                return Ok(changed);
            }
            if !eligible.contains(&event.id) {
                continue;
            }
            let SessionEvent::ToolResult(result) = &event.event else {
                continue;
            };
            if result.content.iter().any(|p| {
                matches!(
                    p,
                    rness_protocol::events::ToolResultContentPart::Image { .. }
                )
            }) {
                continue;
            }
            let chars = result.output.chars().count();
            if chars <= policy.prune_threshold {
                continue;
            }
            let head: String = result.output.chars().take(policy.prune_head).collect();
            let tail: String = result
                .output
                .chars()
                .skip(chars - policy.prune_tail)
                .collect();
            let mut replacement = result.clone();
            replacement.content.clear();
            replacement.output = format!("{head}\n[tool result middle pruned]\n{tail}");
            batch.push(
                log,
                SessionEvent::Prune(Prune {
                    replaces: event.id.clone(),
                    result: replacement,
                }),
            )?;
        }
        changed |= batch.flush(log)?;
        replayed = cache.get(store, log)?;
        // Calibrated: scale the heuristic by the observed real/estimated
        // ratio of the latest committed step, so the threshold compares
        // against something close to what the provider will actually count.
        let pressure = policy.meter.calibrated_pressure(
            &replayed.context,
            system,
            tools,
            calibration(&replayed.history),
        );
        // Manual regions lack the main request's system/tools; do not publish
        // their partial measurement as the automatic compaction estimate.
        if region.is_none() {
            progress(CompactionProgress::Measuring {
                estimated_tokens: pressure,
                threshold_tokens: policy.threshold_tokens,
            });
        }
        if region.is_none()
            && ((!overflow && pressure < policy.threshold_tokens) || (changed && overflow))
        {
            break;
        }
        let cut = region.as_ref().map_or_else(
            || prefix(&replayed.context, policy.retain_tokens, &policy.meter),
            |r| r.end,
        );
        let start = region.as_ref().map_or(0, |r| r.start);
        if cut == 0 {
            break;
        }
        let first: usize = replayed.context.turns[..start]
            .iter()
            .map(source_count)
            .sum();
        let n: usize = replayed.context.turns[..cut].iter().map(source_count).sum();
        // Direct sources only: what the model saw in the span (earlier
        // checkpoints and prunes are cited by their own id). Readers expand
        // what those folded in turn via `shadowed_by_checkpoints`.
        let selected: std::collections::HashSet<_> =
            replayed.context.sources[first..n].iter().cloned().collect();
        let replaces: Vec<_> = replayed
            .history
            .iter()
            .filter(|e| selected.contains(&e.id))
            .map(|e| e.id.clone())
            .collect();
        let mut context = ModelContext {
            turns: replayed.context.turns[start..cut].to_vec(),
            ..Default::default()
        };
        let before = policy.meter.measure(&context, "", &[]);
        progress(CompactionProgress::Summarizing {
            events: replaces.len(),
            estimated_tokens: before,
        });
        context.config = provider
            .summary_config()
            .cloned()
            .unwrap_or_else(|| replayed.context.config.clone());
        if provider.summary_supports_max_output_tokens() {
            context.config.max_output_tokens = Some(policy.summary_tokens);
        } else {
            context.config.max_output_tokens = None;
        }
        context.turns.push(ModelTurn::User {
            content: vec![ContentPart::Text {
                text: policy.prompt.clone(),
            }],
        });
        let request = StepRequest {
            context: &context,
            system: &policy.system_prompt,
            tools: &[],
            on_delta: None,
        };
        let started = log.append(&SessionEvent::CompactionStarted {
            model: provider.summary_model().into(),
            sources: replayed.context.sources[first..n].to_vec(),
            estimated_input: measure(&context, request.system, &[]),
            // Not stored: the input is reconstructable from `sources` + code.
            request: serde_json::Value::Null,
        })?;
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => StepOutcome::Cancelled { partial: vec![] },
            result = provider.summarize_step(request, cancel) => result,
        };
        let (usage, chunks) = match &outcome {
            StepOutcome::Committed(m) => (m.usage, m.chunks.clone()),
            StepOutcome::Cancelled { partial } | StepOutcome::Failed { partial, .. } => {
                (Default::default(), partial.clone())
            }
        };
        let message = match outcome {
            StepOutcome::Committed(message) => message,
            other => {
                let outcome = match other {
                    StepOutcome::Failed { error, .. } => {
                        format!("failed: {}: {}", error.code, error.message)
                    }
                    _ => "cancelled".into(),
                };
                log.append(&SessionEvent::CompactionFinished {
                    started: started.id,
                    outcome,
                    usage,
                    chunks,
                })?;
                break;
            }
        };
        let summary = message
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let summary_context = ModelContext {
            turns: vec![ModelTurn::User {
                content: vec![ContentPart::Text {
                    text: format!("[conversation summary — earlier history compacted]\n{summary}"),
                }],
            }],
            ..Default::default()
        };
        let rejection = if cancel.is_cancelled() {
            Some("cancelled")
        } else if summary.trim().is_empty() {
            Some("empty")
        } else if policy.meter.measure(&summary_context, "", &[]) >= before {
            Some("non_shrinking")
        } else {
            None
        };
        if let Some(outcome) = rejection {
            log.append(&SessionEvent::CompactionFinished {
                started: started.id,
                outcome: outcome.into(),
                usage,
                chunks,
            })?;
            break;
        }
        log.append(&SessionEvent::Compaction(Compaction {
            replaces,
            summary,
            model: provider.summary_model().into(),
        }))?;
        log.append(&SessionEvent::CompactionFinished {
            started: started.id,
            outcome: "committed".into(),
            usage,
            chunks,
        })?;
        changed = true;
        if overflow || region.is_some() {
            break;
        }
    }
    Ok(changed)
}

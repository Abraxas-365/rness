//! Projections: derived views over a session's history. Never stored —
//! always recomputed from the log (or cached disposably, invariant #11).
//!
//! Two projections matter at this layer:
//! - [`Transcript`]: what a frontend shows (includes attempts, tool detail)
//! - [`ModelContext`]: exactly what the next model request replays —
//!   committed messages and tool results only. Attempts NEVER appear
//!   (invariant #5); anything here is reconstructable from the log by
//!   construction (invariant #1), which [`crate::invariants`] asserts.

use std::collections::HashSet;

use rness_protocol::events::{
    AssistantAttempt, CallConfig, ContentPart, Envelope, EventId, SessionEvent, ToolResult, Usage,
};

/// One entry of the conversation as the model will see it.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelTurn {
    User { content: Vec<ContentPart> },
    Assistant { content: Vec<ContentPart> },
    /// Results for the tool calls of the preceding assistant turn,
    /// in model (call) order.
    ToolResults { results: Vec<ToolResult> },
}

/// The derived model-request input: ordered turns plus bookkeeping.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelContext {
    pub turns: Vec<ModelTurn>,
    /// Event ids that produced `turns` — the traceability chain the
    /// invariant checker verifies against the log.
    pub sources: Vec<EventId>,
    /// Cumulative usage over committed assistant messages.
    pub usage: Usage,
    /// Effective request controls: the latest `request/config` event.
    /// Default (all None) when the log never set one.
    pub config: CallConfig,
}

/// Frontend-facing view; keeps what the model context drops.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    pub items: Vec<TranscriptItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    User { event: EventId, content: Vec<ContentPart> },
    Assistant { event: EventId, model: String, content: Vec<ContentPart> },
    Attempt { event: EventId, attempt: AssistantAttempt },
    Tool { event: EventId, result: ToolResult },
    /// A compaction checkpoint: `shadowed` events before it are replayed
    /// to the model as `summary` instead.
    Compaction { event: EventId, summary: String, shadowed: usize },
}

/// Compaction plan for a history: which events are shadowed, and where
/// each LIVE checkpoint's summary anchors (the first shadowed event's
/// position — the summary replays where the folded span began, not where
/// the checkpoint was appended). Honors re-compaction: a checkpoint that
/// is itself in a later checkpoint's `replaces` list is inert.
struct CompactionPlan {
    shadowed: HashSet<EventId>,
    /// first-shadowed-event id -> (checkpoint event id, summary, span len)
    anchors: std::collections::HashMap<EventId, (EventId, String, usize)>,
    /// original tool/result id -> (prune event id, pruned result).
    /// Replayed in the original's place; latest prune wins.
    pruned: std::collections::HashMap<EventId, (EventId, ToolResult)>,
}

fn compaction_plan(history: &[Envelope]) -> CompactionPlan {
    let mut shadowed: HashSet<EventId> = HashSet::new();
    let mut anchors = std::collections::HashMap::new();
    let mut pruned = std::collections::HashMap::new();
    // Later checkpoints win: walk in reverse, skip checkpoints (and
    // prunes) already shadowed by a newer one.
    for env in history.iter().rev() {
        match &env.event {
            SessionEvent::Compaction(c) => {
                if shadowed.contains(&env.id) {
                    continue;
                }
                shadowed.extend(c.replaces.iter().cloned());
                if let Some(first) = c.replaces.first() {
                    anchors.insert(
                        first.clone(),
                        (env.id.clone(), c.summary.clone(), c.replaces.len()),
                    );
                }
            }
            SessionEvent::Prune(pr) => {
                if shadowed.contains(&env.id) || pruned.contains_key(&pr.replaces) {
                    continue; // folded into a summary, or a later prune won
                }
                pruned.insert(pr.replaces.clone(), (env.id.clone(), pr.result.clone()));
            }
            _ => {}
        }
    }
    CompactionPlan { shadowed, anchors, pruned }
}

/// Derive the model context from a history (as returned by
/// `SessionStore::history` — header first, events in order).
pub fn model_context(history: &[Envelope]) -> ModelContext {
    let mut ctx = ModelContext::default();
    let mut pending_tools: Vec<(EventId, ToolResult)> = Vec::new();
    let plan = compaction_plan(history);

    let flush_tools = |ctx: &mut ModelContext, pending: &mut Vec<(EventId, ToolResult)>| {
        if pending.is_empty() {
            return;
        }
        let (ids, results): (Vec<_>, Vec<_>) = pending.drain(..).unzip();
        ctx.sources.extend(ids);
        ctx.turns.push(ModelTurn::ToolResults { results });
    };

    for env in history {
        if plan.shadowed.contains(&env.id) {
            // The summary replays where the folded span began.
            if let Some((ckpt, summary, _)) = plan.anchors.get(&env.id) {
                flush_tools(&mut ctx, &mut pending_tools);
                ctx.sources.push(ckpt.clone());
                ctx.turns.push(ModelTurn::User {
                    content: vec![ContentPart::Text {
                        text: format!(
                            "[conversation summary — earlier history compacted]\n{summary}"
                        ),
                    }],
                });
            }
            continue;
        }
        match &env.event {
            SessionEvent::UserMessage(m) => {
                flush_tools(&mut ctx, &mut pending_tools);
                ctx.sources.push(env.id.clone());
                ctx.turns.push(ModelTurn::User { content: m.content.clone() });
            }
            SessionEvent::AssistantMessage(m) => {
                flush_tools(&mut ctx, &mut pending_tools);
                ctx.sources.push(env.id.clone());
                ctx.turns.push(ModelTurn::Assistant { content: m.content.clone() });
                ctx.usage.input_tokens += m.usage.input_tokens;
                ctx.usage.output_tokens += m.usage.output_tokens;
                ctx.usage.cache_read_tokens += m.usage.cache_read_tokens;
                ctx.usage.cache_write_tokens += m.usage.cache_write_tokens;
            }
            SessionEvent::ToolResult(r) => match plan.pruned.get(&env.id) {
                // Pruned: the shorter replacement replays here, cited by
                // the prune event's id (invariant #1 traceability).
                Some((prune_id, result)) => {
                    pending_tools.push((prune_id.clone(), result.clone()));
                }
                None => pending_tools.push((env.id.clone(), r.clone())),
            },
            // Live checkpoints replayed at their anchor above; inert
            // (re-shadowed) ones are skipped by the shadow check.
            SessionEvent::Compaction(_) => {}
            // Prunes replay at their target's position, never here.
            SessionEvent::Prune(_) => {}
            // Request-header state: the latest event wins (dsh model).
            // Never model-visible — it shapes the request envelope.
            SessionEvent::RequestConfig(c) => ctx.config = c.clone(),
            // Structure and trace events never reach the model.
            SessionEvent::Header(_)
            | SessionEvent::AssistantAttempt(_)
            | SessionEvent::TurnStarted { .. }
            | SessionEvent::TurnEnded { .. } => {}
        }
    }
    flush_tools(&mut ctx, &mut pending_tools);
    ctx
}

/// Derive the frontend transcript (attempts and all). Shadowed events
/// are dropped and the checkpoint appears where they were — the
/// transcript mirrors what the model now sees, plus trace items.
pub fn transcript(history: &[Envelope]) -> Transcript {
    let mut t = Transcript::default();
    let plan = compaction_plan(history);
    for env in history {
        if plan.shadowed.contains(&env.id) {
            if let Some((ckpt, summary, span)) = plan.anchors.get(&env.id) {
                t.items.push(TranscriptItem::Compaction {
                    event: ckpt.clone(),
                    summary: summary.clone(),
                    shadowed: *span,
                });
            }
            continue;
        }
        match &env.event {
            SessionEvent::UserMessage(m) => t.items.push(TranscriptItem::User {
                event: env.id.clone(),
                content: m.content.clone(),
            }),
            SessionEvent::AssistantMessage(m) => t.items.push(TranscriptItem::Assistant {
                event: env.id.clone(),
                model: m.model.clone(),
                content: m.content.clone(),
            }),
            SessionEvent::AssistantAttempt(a) => t.items.push(TranscriptItem::Attempt {
                event: env.id.clone(),
                attempt: a.clone(),
            }),
            SessionEvent::ToolResult(r) => {
                let (event, result) = match plan.pruned.get(&env.id) {
                    Some((prune_id, pruned)) => (prune_id.clone(), pruned.clone()),
                    None => (env.id.clone(), r.clone()),
                };
                t.items.push(TranscriptItem::Tool { event, result });
            }
            SessionEvent::Compaction(_) => {} // replayed at its anchor
            SessionEvent::Prune(_) => {} // replayed at its target
            SessionEvent::Header(_)
            | SessionEvent::RequestConfig(_)
            | SessionEvent::TurnStarted { .. }
            | SessionEvent::TurnEnded { .. } => {}
        }
    }
    t
}

//! Approval overlay: shows the paused sensitive tool call published in
//! `Model::pending_approval` and answers it with a one-shot y/n.
//!
//! Chain pattern: this component is mounted at startup like any other and
//! `wants()` selects it into the overlay slot only while a question is
//! pending — the shell never mounts/unmounts it. Under the default
//! `allow` policy it never appears.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};
use rness_protocol::api::{ApprovalDecision, ApprovalRequest};
use tokio::sync::oneshot;

use crate::app::Action;
use crate::component::{Component, Ctx, KeyOutcome};
use crate::slots::{Slots, OVERLAY};

/// A pending question plus the wire to answer it. Dropping it unanswered
/// (quit, turn cancelled) resolves as Cancelled engine-side.
pub struct PendingApproval {
    pub request: ApprovalRequest,
    pub respond: oneshot::Sender<ApprovalDecision>,
}

impl std::fmt::Debug for PendingApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingApproval").field("request", &self.request).finish_non_exhaustive()
    }
}

pub struct ApprovalOverlay;

/// Mount the module (composition-root seam, same as future Lua installs).
pub fn install(slots: &mut Slots) {
    slots.mount(OVERLAY, 10, Box::new(ApprovalOverlay));
}

impl Component for ApprovalOverlay {
    fn name(&self) -> &str {
        "approval"
    }

    fn binding_help(&self) -> Vec<String> {
        vec!["Y / Enter: allow; N / Escape: reject. All other keys are captured.".into()]
    }

    fn wants(&self, ctx: &Ctx<'_>) -> bool {
        ctx.model.pending_approval.is_some()
    }

    fn height(&self, ctx: &Ctx<'_>, _width: u16) -> Option<u16> {
        let args_lines = ctx
            .model
            .pending_approval
            .as_ref()
            .map(|p| render_args(&p.request).len() as u16)
            .unwrap_or(0);
        Some((args_lines + 4).min(14)) // borders + title + prompt
    }

    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        let Some(pending) = &ctx.model.pending_approval else { return };
        Clear.render(area, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" approve: {} ", pending.request.tool));
        let inner = block.inner(area);
        block.render(area, buf);

        let mut lines: Vec<Line> = render_args(&pending.request);
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("[y]", ctx.theme.tool_name),
            Span::raw(" allow once   "),
            Span::styled("[n]", ctx.theme.error),
            Span::raw(" reject"),
        ]));
        Paragraph::new(lines).render(inner, buf);
    }

    fn on_key(&mut self, _ctx: &Ctx<'_>, key: KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                KeyOutcome::act(vec![Action::ResolveApproval(ApprovalDecision::Allowed)])
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                KeyOutcome::act(vec![Action::ResolveApproval(ApprovalDecision::Rejected)])
            }
            // Modal: swallow everything else.
            _ => KeyOutcome::consumed(),
        }
    }
}

fn render_args(request: &ApprovalRequest) -> Vec<Line<'static>> {
    const MAX_LINES: usize = 8;
    const MAX_WIDTH: usize = 100;
    let pretty =
        serde_json::to_string_pretty(&request.args).unwrap_or_else(|_| request.args.to_string());
    let mut lines: Vec<Line<'static>> = pretty
        .lines()
        .take(MAX_LINES)
        .map(|l| {
            let mut l = l.to_string();
            if l.len() > MAX_WIDTH {
                let mut end = MAX_WIDTH;
                while !l.is_char_boundary(end) {
                    end -= 1;
                }
                l.truncate(end);
                l.push('…');
            }
            Line::raw(l)
        })
        .collect();
    if pretty.lines().count() > MAX_LINES {
        lines.push(Line::raw("…"));
    }
    lines
}

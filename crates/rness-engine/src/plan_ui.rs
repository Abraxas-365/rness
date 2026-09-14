//! Lua-configurable plan review presentation and editor behavior.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use crate::questions::ui::{Padding, QuestionUiConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlanReviewConfig {
    pub title: String,
    pub height: u16,
    pub width: u16,
    pub padding: Padding,
    pub border: String,
    pub styles: BTreeMap<String, serde_json::Value>,
    pub show_help: bool,
    pub edit_enabled: bool,
    pub approve_after_edit: bool,
    pub editor: Option<Vec<String>>,
    pub labels: PlanLabels,
    pub keys: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlanLabels {
    pub approve: String,
    pub feedback: String,
    pub edit: String,
    pub edit_and_approve: String,
    pub feedback_prompt: String,
}
impl Default for PlanLabels {
    fn default() -> Self { Self {
        approve: "Approve".into(), feedback: "Request changes".into(),
        edit: "Open in editor".into(), edit_and_approve: "Edit & approve".into(),
        feedback_prompt: "Feedback".into(),
    } }
}
fn default_keys() -> BTreeMap<String, String> {
    [("approve", "a"), ("edit", "e"), ("feedback", "r"), ("up", "up"),
        ("down", "down"), ("page_up", "pageup"), ("page_down", "pagedown"),
        ("first", "home"), ("last", "end"), ("dismiss", "esc"), ("cancel", "ctrl+c"),
        ("feedback_submit", "enter"), ("feedback_back", "esc")]
        .into_iter().map(|(k,v)| (k.into(),v.into())).collect()
}
impl Default for PlanReviewConfig {
    fn default() -> Self { Self {
        title: "Plan review".into(), height: 30, width: 100,
        padding: Padding { left: 1, right: 1, top: 0, bottom: 0 },
        border: "rounded".into(), styles: BTreeMap::new(), show_help: true,
        edit_enabled: true, approve_after_edit: true, editor: None,
        labels: PlanLabels::default(), keys: default_keys(),
    } }
}
impl PlanReviewConfig {
    pub fn normalize(&mut self) {
        for (key, value) in default_keys() { self.keys.entry(key).or_insert(value); }
    }
    pub fn ui(&self) -> QuestionUiConfig {
        QuestionUiConfig { width: Some(self.width), padding: self.padding.clone(),
            border: self.border.clone(), styles: self.styles.clone(), show_help: self.show_help,
            ..Default::default() }
    }
    pub fn validate(&self) -> Result<(), String> {
        self.ui().validate()?;
        let border = if self.border == "none" { 0 } else { 2 };
        if self.height < 10 || self.height.saturating_sub(self.padding.top + self.padding.bottom + border) < 6
            || self.width.saturating_sub(self.padding.left + self.padding.right + border) < 24 {
            return Err("plan review dimensions must leave at least 24 columns and 6 rows after padding/border".into());
        }
        for text in [&self.title, &self.labels.approve, &self.labels.feedback, &self.labels.edit,
            &self.labels.edit_and_approve, &self.labels.feedback_prompt] {
            if text.trim().is_empty() || text.chars().count() > 120 || text.chars().any(char::is_control) {
                return Err("plan review labels must be nonempty single-line text of at most 120 characters".into());
            }
        }
        if self.editor.as_ref().is_some_and(|argv| argv.first().is_none_or(|s| s.trim().is_empty()) || argv.iter().any(|s| s.contains('\0'))) {
            return Err("plan.review.editor must be a nonempty executable argv".into());
        }
        let defaults = default_keys();
        let mut seen = std::collections::HashSet::new();
        for (action, key) in &self.keys {
            if !defaults.contains_key(action) { return Err(format!("unknown plan review action: {action}")); }
            let chord = rness_kernel::presentation::canonical_chord(key)?;
            let code = chord.rsplit('+').next().unwrap();
            let control = chord.split('+').any(|s| s == "ctrl");
            let alt = chord.split('+').any(|s| s == "alt");
            if control && code == "c" && action != "cancel" { return Err("Ctrl+C is reserved for cancellation".into()); }
            if action.starts_with("feedback_") {
                if (control && code == "c") || (!control && !alt && (code.len() == 1 || code == "space")) || code == "backspace" {
                    return Err("feedback controls must not conflict with text editing or cancellation".into());
                }
                if self.keys.get("cancel").is_some_and(|key| rness_kernel::presentation::canonical_chord(key).ok().as_ref() == Some(&chord)) {
                    return Err("feedback controls must not conflict with cancellation".into());
                }
                continue;
            }
            if !seen.insert(chord) { return Err("duplicate plan review key bindings".into()); }
        }
        if self.keys.get("feedback_submit").zip(self.keys.get("feedback_back")).is_some_and(|(a,b)|
            rness_kernel::presentation::canonical_chord(a).ok() == rness_kernel::presentation::canonical_chord(b).ok()) {
            return Err("duplicate feedback key bindings".into());
        }
        Ok(())
    }
}

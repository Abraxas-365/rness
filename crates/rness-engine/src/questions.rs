//! Explicit, cancellable human questions shared by local and remote frontends.
use std::{collections::BTreeMap, sync::{Arc, Mutex}};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionItem { pub label: String, #[serde(default)] pub description: String }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub question: String,
    #[serde(default)] pub header: String,
    #[serde(default)] pub options: Vec<OptionItem>,
    #[serde(default)] pub multi_select: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer { pub id: String, pub selected: Vec<String>, pub custom: Option<String> }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answers { pub answers: Vec<Answer> }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request { pub session: String, pub call: String, pub questions: Vec<Question> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input { questions: Vec<Question> }
struct Pending { request: Request, tx: oneshot::Sender<Answers> }
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionEvent {
    QuestionRequested { session: String, call: String, questions: Vec<Question> },
    QuestionResolved { session: String, call: String },
}
impl QuestionEvent {
    pub fn session(&self) -> &str { match self { Self::QuestionRequested { session, .. } | Self::QuestionResolved { session, .. } => session } }
}
#[derive(Clone, Debug)]
pub struct OverlayConfig {
    pub priority: i32,
    pub height: u16,
    pub title: String,
}
impl Default for OverlayConfig {
    fn default() -> Self { Self { priority: 99, height: 20, title: "AskUser".into() } }
}

pub struct Questions {
    events: tokio::sync::broadcast::Sender<QuestionEvent>,
    pending: Mutex<BTreeMap<(String, String), Pending>>,
    available: std::sync::atomic::AtomicBool,
    config: Mutex<OverlayConfig>,
    owner: Mutex<Option<String>>,
    registration: Mutex<Option<(std::sync::Weak<crate::tools::ToolRegistry>, std::sync::Weak<Questions>, Option<std::sync::Weak<dyn crate::tools::Tool>>)>>,
}
impl Default for Questions {
    fn default() -> Self { Self { pending: Default::default(), available: Default::default(), events: tokio::sync::broadcast::channel(128).0, config: Default::default(), owner: Default::default(), registration: Default::default() } }
}
impl Questions {
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<QuestionEvent> { self.events.subscribe() }
    fn resolved(&self, session: &str, call: &str) { let _ = self.events.send(QuestionEvent::QuestionResolved { session: session.into(), call: call.into() }); }
    pub fn bind_registry(self: &Arc<Self>, registry: &Arc<crate::tools::ToolRegistry>) {
        *self.registration.lock().unwrap() = Some((Arc::downgrade(registry), Arc::downgrade(self), None));
        self.set_available(self.is_available());
    }
    pub fn set_available(&self, available: bool) {
        let mut registration = self.registration.lock().unwrap();
        if let Some((registry, broker, installed)) = registration.as_mut() {
            if let Some(registry) = registry.upgrade() {
                if available && installed.as_ref().and_then(|tool| tool.upgrade()).is_none() {
                    let tool: Arc<dyn crate::tools::Tool> = Arc::new(AskUser(broker.upgrade().expect("live broker")));
                    if let Err(error) = registry.try_register(tool.clone()) {
                        tracing::warn!("{error}");
                        return;
                    }
                    *installed = Some(Arc::downgrade(&tool));
                } else if !available {
                    if let Some(tool) = installed.take().and_then(|tool| tool.upgrade()) { registry.unregister_if_current(&tool); }
                }
            }
        }
        self.available.store(available, std::sync::atomic::Ordering::SeqCst);
        if !available {
            let mut pending = self.pending.lock().unwrap();
            for (session, call) in pending.keys() { self.resolved(session, call); }
            pending.clear();
        }
    }
    pub fn is_available(&self) -> bool { self.available.load(std::sync::atomic::Ordering::SeqCst) }
    pub fn set_overlay_config(&self, config: OverlayConfig) { *self.config.lock().unwrap() = config; }
    pub fn overlay_config(&self) -> OverlayConfig { self.config.lock().unwrap().clone() }
    pub fn set_owner(&self, owner: Option<String>) { *self.owner.lock().unwrap() = owner; }
    pub fn owner(&self) -> Option<String> { self.owner.lock().unwrap().clone() }
    pub fn pending(&self) -> Vec<Request> { self.pending.lock().unwrap().values().map(|p| p.request.clone()).collect() }
    pub fn resolve(&self, session: &str, call: &str, answers: Answers) -> Result<(), String> {
        let key = (session.to_owned(), call.to_owned());
        let mut pending = self.pending.lock().unwrap();
        let item = pending.get(&key).ok_or("question no longer pending")?;
        if answers.answers.len() != item.request.questions.len() { return Err("answer every question exactly once".into()); }
        let mut ids = std::collections::HashSet::new();
        for answer in &answers.answers {
            let question = item.request.questions.iter().find(|q| q.id == answer.id).ok_or("unknown question id")?;
            if !ids.insert(&answer.id) || (!question.multi_select && answer.selected.len() > 1) { return Err("duplicate answer or too many selections".into()); }
            let mut selections = std::collections::HashSet::new();
            for selected in &answer.selected {
                if !selections.insert(selected) || !question.options.iter().any(|o| &o.label == selected) { return Err("invalid selection".into()); }
            }
            if answer.selected.is_empty() && answer.custom.as_ref().map_or(true, |s| s.trim().is_empty()) { return Err("empty answer".into()); }
        }
        let result = pending.remove(&key).unwrap().tx.send(answers).map_err(|_| "question cancelled".into());
        self.resolved(session, call);
        result
    }
    pub fn dismiss(&self, session: &str, call: &str) -> bool {
        let mut pending = self.pending.lock().unwrap();
        let removed = pending.remove(&(session.into(), call.into())).is_some();
        if removed { self.resolved(session, call); }
        removed
    }
    async fn ask(&self, request: Request, cancel: &CancellationToken) -> Result<String, String> {
        let (tx, rx) = oneshot::channel();
        let key = (request.session.clone(), request.call.clone());
        {
            let mut pending = self.pending.lock().unwrap();
            if !self.available.load(std::sync::atomic::Ordering::SeqCst) { return Err("no question frontend available".into()); }
            if pending.contains_key(&key) { return Err("duplicate pending question call".into()); }
            let event = QuestionEvent::QuestionRequested { session: request.session.clone(), call: request.call.clone(), questions: request.questions.clone() };
            pending.insert(key.clone(), Pending { request, tx });
            let _ = self.events.send(event);
        }
        struct Guard<'a>(&'a Questions, (String, String));
        impl Drop for Guard<'_> { fn drop(&mut self) { self.0.dismiss(&self.1.0, &self.1.1); } }
        let _guard = Guard(self, key);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err("question cancelled".into()),
            result = rx => serde_json::to_string(&result.map_err(|_| "question dismissed or frontend disconnected")?).map_err(|e| e.to_string()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolRegistry, ToolCall};
    #[tokio::test]
    async fn dispatch_validates_answers_and_cleans_up_cancellation() {
        let questions = Arc::new(Questions::default());
        let registry = Arc::new(ToolRegistry::default());
        registry.register(Arc::new(AskUser(questions.clone())));
        let call = ToolCall { call: "c".into(), name: "AskUser".into(), args: serde_json::json!({"questions":[{"id":"q","question":"Choose","options":[{"label":"A"}]}]}) };
        let result = registry.dispatch(&"s".into(), &[call.clone()], 1, &CancellationToken::new()).await;
        assert!(result[0].is_error);
        questions.set_available(true);
        for cancel_it in [false, true] {
            let token = CancellationToken::new();
            let cancel = token.clone();
            let registry = registry.clone(); let call = call.clone();
            let worker = tokio::spawn(async move { registry.dispatch(&"s".into(), &[call], 1, &token).await });
            tokio::time::timeout(std::time::Duration::from_secs(2), async { while questions.pending().is_empty() { tokio::task::yield_now().await; } }).await.unwrap();
            assert!(questions.resolve("s", "c", Answers { answers: vec![] }).is_err());
            assert_eq!(questions.pending().len(), 1);
            if cancel_it { cancel.cancel(); } else {
                questions.resolve("s", "c", Answers { answers: vec![Answer { id: "q".into(), selected: vec!["A".into()], custom: None }] }).unwrap();
            }
            let result = worker.await.unwrap();
            assert_eq!(result[0].is_error, cancel_it);
            if !cancel_it { assert!(result[0].output.contains("A")); }
            assert!(questions.pending().is_empty());
        }
    }
}

pub struct AskUser(pub Arc<Questions>);
#[async_trait::async_trait]
impl crate::tools::Tool for AskUser {
    fn name(&self) -> &str { "AskUser" }
    fn description(&self) -> &str { "Ask the user for missing information or decisions. Provide stable question IDs and optional choices; custom text is always allowed." }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","additionalProperties":false,"required":["questions"],"properties":{"questions":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"object","additionalProperties":false,"required":["id","question"],"properties":{"id":{"type":"string"},"question":{"type":"string"},"header":{"type":"string"},"multi_select":{"type":"boolean"},"options":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["label"],"properties":{"label":{"type":"string"},"description":{"type":"string"}}}}}}}}})
    }
    async fn execute(&self, _: serde_json::Value) -> Result<String, String> { Err("AskUser requires agent dispatch context".into()) }
    async fn execute_call(&self, session: &String, call: &str, args: serde_json::Value, cancel: &CancellationToken) -> Result<String, String> {
        let input: Input = serde_json::from_value(args).map_err(|e| e.to_string())?;
        if input.questions.is_empty() || input.questions.len() > 16 { return Err("expected 1 to 16 questions".into()); }
        let mut ids = std::collections::HashSet::new();
        for q in &input.questions {
            if q.id.trim().is_empty() || q.question.trim().is_empty() || !ids.insert(&q.id) { return Err("questions need unique nonempty IDs and text".into()); }
            let mut labels = std::collections::HashSet::new();
            if q.options.iter().any(|o| o.label.trim().is_empty() || !labels.insert(&o.label)) { return Err("options need unique nonempty labels".into()); }
        }
        self.0.ask(Request { session: session.clone(), call: call.into(), questions: input.questions }, cancel).await
    }
}

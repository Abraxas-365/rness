//! Durable session events → ACP `session/update` payloads.
//!
//! Only committed facts are translated (like dsh's ACP surface): reasoning,
//! assistant text, tool calls and tool results, read back from the session
//! log after they are appended. Live token deltas are deliberately not
//! forwarded, so what a client sees is exactly what the log holds and a
//! reconnect/`session/load` replays the same updates.

use base64::Engine as _;
use rness_protocol::events::{
    ContentPart, Envelope, MessageSource, SessionEvent, ToolResultContentPart, UserIntent,
};
use serde_json::{json, Value};

/// Image bytes for a tool-result attachment, when the caller can read them.
pub type ImageReader<'a> = Box<dyn Fn(&str) -> Option<(String, Vec<u8>)> + 'a>;

pub struct Translate<'a> {
    /// Replay human prompts as `user_message_chunk` (`session/load`).
    pub include_user: bool,
    /// Session workspace, to make relative tool paths absolute.
    pub workspace: Option<&'a str>,
    /// Context window of the selected model, for `usage_update`.
    pub context_window: Option<u32>,
    pub images: ImageReader<'a>,
}

impl Translate<'_> {
    pub fn updates(&self, envelope: &Envelope) -> Vec<Value> {
        match &envelope.event {
            SessionEvent::UserMessage(message) if self.include_user => {
                let human = message.intent != UserIntent::Inject
                    && matches!(
                        message.source,
                        None | Some(MessageSource::ExternalPrompt { .. })
                    );
                if !human {
                    return Vec::new();
                }
                message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text { text } => Some(chunk(
                            "user_message_chunk",
                            &envelope.id,
                            json!({"type": "text", "text": text}),
                        )),
                        _ => None,
                    })
                    .collect()
            }
            SessionEvent::AssistantMessage(message) => {
                let mut out: Vec<Value> = message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Thinking { text, .. } if !text.is_empty() => Some(chunk(
                            "agent_thought_chunk",
                            &envelope.id,
                            json!({"type": "text", "text": text}),
                        )),
                        ContentPart::Text { text } if !text.is_empty() => Some(chunk(
                            "agent_message_chunk",
                            &envelope.id,
                            json!({"type": "text", "text": text}),
                        )),
                        ContentPart::ToolUse { call, name, args } => {
                            Some(self.tool_call(call, name, args))
                        }
                        _ => None,
                    })
                    .collect();
                if let Some(size) = self.context_window {
                    let usage = message.usage;
                    let used = usage.input_tokens
                        + usage.cache_read_tokens
                        + usage.cache_write_tokens
                        + usage.output_tokens;
                    if used > 0 {
                        out.push(json!({
                            "sessionUpdate": "usage_update",
                            "used": used,
                            "size": size,
                        }));
                    }
                }
                out
            }
            SessionEvent::ToolResult(result) => {
                let mut content = Vec::new();
                if result.content.is_empty() {
                    if !result.output.is_empty() {
                        content.push(text_content(&result.output));
                    }
                } else {
                    for part in &result.content {
                        match part {
                            ToolResultContentPart::Text { text } => {
                                content.push(text_content(text))
                            }
                            ToolResultContentPart::Image { attachment } => {
                                if let Some((mime, data)) = (self.images)(&attachment.id) {
                                    content.push(json!({
                                        "type": "content",
                                        "content": {
                                            "type": "image",
                                            "mimeType": mime,
                                            "data": base64::engine::general_purpose::STANDARD
                                                .encode(data),
                                        },
                                    }));
                                }
                            }
                        }
                    }
                }
                vec![json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": result.call,
                    "status": if result.is_error { "failed" } else { "completed" },
                    "content": content,
                })]
            }
            _ => Vec::new(),
        }
    }

    pub fn tool_call(&self, call: &str, name: &str, args: &Value) -> Value {
        let mut update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": call,
            "title": tool_title(name, args),
            "kind": tool_kind(name),
            "status": "in_progress",
            "rawInput": args,
        });
        if let Some(path) = args.get("path").and_then(Value::as_str) {
            let path = match self.workspace {
                Some(root) if !std::path::Path::new(path).is_absolute() => {
                    std::path::Path::new(root).join(path).display().to_string()
                }
                _ => path.to_string(),
            };
            update["locations"] = json!([{ "path": path }]);
        }
        update
    }
}

fn chunk(kind: &str, message: &str, content: Value) -> Value {
    json!({ "sessionUpdate": kind, "messageId": message, "content": content })
}

fn text_content(text: &str) -> Value {
    json!({ "type": "content", "content": { "type": "text", "text": text } })
}

/// ACP tool kinds for rness's built-in tools; everything else is `other`.
pub fn tool_kind(name: &str) -> &'static str {
    match name {
        "Read" | "read_image" => "read",
        "Edit" | "Write" => "edit",
        "Bash" => "execute",
        "Grep" | "Glob" | "web_search" => "search",
        "web_fetch" => "fetch",
        _ => "other",
    }
}

/// A short human label: the tool name plus its most telling argument.
pub fn tool_title(name: &str, args: &Value) -> String {
    let detail = ["description", "path", "pattern", "command", "url", "query"]
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .map(|detail| detail.lines().next().unwrap_or("").trim());
    match detail {
        Some(detail) if !detail.is_empty() => {
            let detail: String = detail.chars().take(120).collect();
            format!("{name}: {detail}")
        }
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rness_protocol::events::{AssistantMessage, StopReason, ToolResult, Usage};

    fn env(id: &str, event: SessionEvent) -> Envelope {
        Envelope {
            id: id.into(),
            at: "2026-01-01T00:00:00Z".into(),
            event,
        }
    }

    fn translate() -> Translate<'static> {
        Translate {
            include_user: false,
            workspace: Some("/work"),
            context_window: Some(1000),
            images: Box::new(|_| None),
        }
    }

    #[test]
    fn assistant_message_becomes_thought_text_tool_and_usage() {
        let updates = translate().updates(&env(
            "E1",
            SessionEvent::AssistantMessage(AssistantMessage {
                model: "m".into(),
                content: vec![
                    ContentPart::Thinking {
                        text: "hmm".into(),
                        signature: None,
                    },
                    ContentPart::Text { text: "hi".into() },
                    ContentPart::ToolUse {
                        call: "c1".into(),
                        name: "Read".into(),
                        args: json!({"path": "src/lib.rs"}),
                    },
                ],
                stop: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
                estimated_input: 0,
                chunks: vec![],
            }),
        ));
        assert_eq!(updates[0]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(updates[0]["messageId"], "E1");
        assert_eq!(updates[1]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(updates[1]["content"]["text"], "hi");
        assert_eq!(updates[2]["sessionUpdate"], "tool_call");
        assert_eq!(updates[2]["kind"], "read");
        assert_eq!(updates[2]["title"], "Read: src/lib.rs");
        assert_eq!(updates[2]["locations"][0]["path"], "/work/src/lib.rs");
        assert_eq!(
            updates[3],
            json!({"sessionUpdate":"usage_update","used":15,"size":1000})
        );
    }

    #[test]
    fn tool_result_maps_status_and_text() {
        let updates = translate().updates(&env(
            "E2",
            SessionEvent::ToolResult(
                serde_json::from_value::<ToolResult>(json!({
                    "call": "c1", "name": "Bash", "output": "boom",
                    "is_error": true, "duration_ms": 1
                }))
                .unwrap(),
            ),
        ));
        assert_eq!(
            updates,
            vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "c1",
                "status": "failed",
                "content": [{"type":"content","content":{"type":"text","text":"boom"}}],
            })]
        );
    }

    #[test]
    fn user_messages_only_on_replay_and_only_human() {
        let message = |source| {
            env(
                "U",
                SessionEvent::UserMessage(rness_protocol::events::UserMessage {
                    intent: UserIntent::Followup,
                    content: vec![ContentPart::Text { text: "q".into() }],
                    source,
                }),
            )
        };
        assert!(translate().updates(&message(None)).is_empty());
        let replay = Translate {
            include_user: true,
            ..translate()
        };
        assert_eq!(
            replay.updates(&message(None))[0]["sessionUpdate"],
            "user_message_chunk"
        );
        assert!(replay
            .updates(&message(Some(MessageSource::Instructions {
                identity: "x".into()
            })))
            .is_empty());
    }
}

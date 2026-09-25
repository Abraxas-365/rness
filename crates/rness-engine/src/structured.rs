//! Child-scoped structured output (dsh `subagent-in-process-driver/structured.ts`
//! + `dsh-tools/json-schema.ts`).
//!
//! A delegation that asks for a schema gets an in-memory
//! [`StructuredAttachment`] keyed by the child session. While attached, that
//! child's turn loop (and only that child's) exposes a `structured_output`
//! tool whose parameters are the caller's schema, appends
//! [`INSTRUCTION`] to its system prompt, commits a value only from the
//! authoritative successful tool result, ends the turn on capture, and
//! refuses every later call in the same response. Nothing is durable: the
//! owner detaches when the run settles, leaving no residue in the session
//! log format or `CallConfig`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::tools::Tool;
use rness_protocol::events::{SessionId, ToolCallId};

/// The model-facing tool name a structured child must call to finish.
pub const TOOL: &str = "structured_output";

/// Trailing system-prompt section for a structured child (dsh wording).
pub const INSTRUCTION: &str = "When you have your final answer, you MUST report it by calling the `structured_output` tool with arguments matching its parameter schema exactly. Do not finish with a plain text answer: only the tool call counts as your result.";

const DESCRIPTION: &str = "Report your final structured result. Call this exactly once, when your answer is complete; the arguments must match this tool's parameter schema exactly.";

/// Result text of an accepted capture.
pub const RECORDED: &str = "Structured output recorded.";

/// Guard verdict for calls after a capture.
pub fn guard_message(tool: &str) -> String {
    format!("structured output already recorded: the run is complete, so `{tool}` is not executed")
}

const TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];
const CONSTRAINTS: [&str; 8] = [
    "type",
    "oneOf",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
];
const ANNOTATIONS: [&str; 4] = ["description", "title", "default", "examples"];
const ONE_OF_SIBLINGS: [&str; 6] = [
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
];
/// Nesting ceiling: rejects pathological schemas before recursion can hurt.
const MAX_DEPTH: usize = 64;

/// Check `schema` against the enforced subset with an object root
/// (tool arguments are objects). Returns every violation, not just the first.
pub fn check_object_schema(schema: &Value) -> Result<(), Vec<String>> {
    let mut violations = Vec::new();
    check_node(schema, "schema", 0, &mut violations);
    if violations.is_empty() && schema.get("type").and_then(Value::as_str) != Some("object") {
        violations
            .push("schema.type must be \"object\" (structured output is object-rooted)".into());
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

fn scalar_matches(ty: &str, value: &Value) -> bool {
    match ty {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => is_integer(value),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn is_integer(value: &Value) -> bool {
    match value {
        Value::Number(n) => {
            n.is_i64()
                || n.is_u64()
                || n.as_f64()
                    .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
        }
        _ => false,
    }
}

fn check_node(node: &Value, path: &str, depth: usize, out: &mut Vec<String>) {
    let Some(obj) = node.as_object() else {
        out.push(format!("{path} must be a schema object"));
        return;
    };
    if depth > MAX_DEPTH {
        out.push(format!("{path} nests deeper than {MAX_DEPTH} levels"));
        return;
    }
    for key in obj.keys() {
        if !CONSTRAINTS.contains(&key.as_str()) && !ANNOTATIONS.contains(&key.as_str()) {
            out.push(format!("{path}.{key} is not a supported keyword (subset: type/oneOf/properties/required/additionalProperties/items/enum/const + annotations)"));
        }
    }
    for key in ["description", "title"] {
        if obj.get(key).is_some_and(|v| !v.is_string()) {
            out.push(format!("{path}.{key} must be a string"));
        }
    }
    let (ty, one_of) = (obj.get("type"), obj.get("oneOf"));
    match (ty, one_of) {
        (Some(_), Some(_)) => out.push(format!("{path} cannot declare both type and oneOf")),
        (None, None) => {
            for key in ONE_OF_SIBLINGS {
                if obj.contains_key(key) {
                    out.push(format!("{path}.{key} requires type or oneOf"));
                }
            }
        }
        (None, Some(branches)) => {
            match branches.as_array().filter(|b| b.len() >= 2) {
                Some(branches) => {
                    for (i, branch) in branches.iter().enumerate() {
                        check_node(branch, &format!("{path}.oneOf[{i}]"), depth + 1, out);
                    }
                }
                None => out.push(format!(
                    "{path}.oneOf must be an array of at least two schemas"
                )),
            }
            for key in ONE_OF_SIBLINGS {
                if obj.contains_key(key) {
                    out.push(format!("{path}.{key} is not supported beside oneOf"));
                }
            }
        }
        (Some(ty), None) => check_typed(obj, ty, path, depth, out),
    }
}

fn check_typed(
    obj: &Map<String, Value>,
    ty: &Value,
    path: &str,
    depth: usize,
    out: &mut Vec<String>,
) {
    let Some(ty) = ty.as_str().filter(|t| TYPES.contains(t)) else {
        out.push(if ty.is_array() {
            format!("{path}.type must be a single type string (type arrays are not supported)")
        } else {
            format!("{path}.type must be one of {}", TYPES.join("/"))
        });
        return;
    };
    let scalar = !matches!(ty, "object" | "array");
    for (key, ok) in [
        ("properties", ty == "object"),
        ("required", ty == "object"),
        ("additionalProperties", ty == "object"),
        ("items", ty == "array"),
        ("enum", scalar),
        ("const", scalar),
    ] {
        if obj.contains_key(key) && !ok {
            out.push(format!("{path}.{key} is not supported on type \"{ty}\""));
        }
    }
    match ty {
        "object" => {
            let properties = obj.get("properties");
            if let Some(props) = properties {
                match props.as_object() {
                    Some(props) => {
                        for (name, child) in props {
                            check_node(child, &format!("{path}.properties.{name}"), depth + 1, out);
                        }
                    }
                    None => out.push(format!("{path}.properties must be an object of schemas")),
                }
            }
            if let Some(required) = obj.get("required") {
                match required
                    .as_array()
                    .filter(|r| r.iter().all(Value::is_string))
                {
                    Some(required) => {
                        for name in required.iter().filter_map(Value::as_str) {
                            if !properties
                                .and_then(Value::as_object)
                                .is_some_and(|p| p.contains_key(name))
                            {
                                out.push(format!(
                                    "{path}.required names \"{name}\" which is not in properties"
                                ));
                            }
                        }
                    }
                    None => out.push(format!("{path}.required must be an array of strings")),
                }
            }
            if obj
                .get("additionalProperties")
                .is_some_and(|v| !v.is_boolean())
            {
                out.push(format!("{path}.additionalProperties must be a boolean"));
            }
        }
        "array" => {
            if let Some(items) = obj.get("items") {
                check_node(items, &format!("{path}.items"), depth + 1, out);
            }
        }
        _ => {
            let allowed = obj.get("enum");
            let enum_ok = allowed
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty() && a.iter().all(|v| scalar_matches(ty, v)));
            if allowed.is_some() && enum_ok.is_none() {
                out.push(format!(
                    "{path}.enum must be a non-empty array of {ty} values"
                ));
            }
            if let Some(c) = obj.get("const") {
                if !scalar_matches(ty, c) {
                    out.push(format!("{path}.const must be a {ty} value"));
                } else if enum_ok.is_some_and(|a| !a.contains(c)) {
                    out.push(format!(
                        "{path}.const must be one of {path}.enum when both are declared"
                    ));
                }
            }
        }
    }
}

/// Validate `value` against a schema that already passed
/// [`check_object_schema`]. Returns path-qualified violations (empty = valid).
pub fn validate(schema: &Value, value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    validate_at(schema, value, "", &mut out);
    out
}

fn shown(path: &str) -> &str {
    if path.is_empty() {
        "arguments"
    } else {
        path
    }
}

fn child_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn validate_at(schema: &Value, value: &Value, path: &str, out: &mut Vec<String>) {
    let Some(node) = schema.as_object() else {
        return;
    };
    if let Some(branches) = node.get("oneOf").and_then(Value::as_array) {
        let matches = branches
            .iter()
            .filter(|branch| {
                let mut scratch = Vec::new();
                validate_at(branch, value, path, &mut scratch);
                scratch.is_empty()
            })
            .count();
        if matches != 1 {
            out.push(format!(
                "\"{}\" must match exactly one oneOf branch (matched {matches})",
                shown(path)
            ));
        }
        return;
    }
    let Some(ty) = node.get("type").and_then(Value::as_str) else {
        return; // annotation-only: any JSON value
    };
    let at = shown(path);
    match ty {
        "object" => {
            let Some(map) = value.as_object() else {
                out.push(format!("\"{at}\" must be an object"));
                return;
            };
            let props = node.get("properties").and_then(Value::as_object);
            for name in node
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !map.contains_key(name) {
                    out.push(format!(
                        "missing required property \"{}\"",
                        child_path(path, name)
                    ));
                }
            }
            if let Some(props) = props {
                for (name, child) in props {
                    if let Some(v) = map.get(name) {
                        validate_at(child, v, &child_path(path, name), out);
                    }
                }
            }
            if node.get("additionalProperties") == Some(&Value::Bool(false)) {
                for key in map.keys() {
                    if !props.is_some_and(|p| p.contains_key(key)) {
                        out.push(format!(
                            "\"{}\" is not a declared property (additionalProperties: false)",
                            child_path(path, key)
                        ));
                    }
                }
            }
        }
        "array" => {
            let Some(items) = value.as_array() else {
                out.push(format!("\"{at}\" must be an array"));
                return;
            };
            if let Some(item) = node.get("items") {
                for (i, v) in items.iter().enumerate() {
                    validate_at(item, v, &format!("{path}[{i}]"), out);
                }
            }
        }
        scalar => {
            if !scalar_matches(scalar, value) {
                let article = if scalar == "integer" { "an" } else { "a" };
                out.push(if scalar == "null" {
                    format!("\"{at}\" must be null")
                } else {
                    format!("\"{at}\" must be {article} {scalar}")
                });
                return;
            }
            if let Some(allowed) = node.get("enum").and_then(Value::as_array) {
                if !allowed.iter().any(|a| json_eq(a, value)) {
                    out.push(format!(
                        "\"{at}\" must be one of {}",
                        Value::Array(allowed.clone())
                    ));
                    return;
                }
            }
            if let Some(c) = node.get("const") {
                if !json_eq(c, value) {
                    out.push(format!("\"{at}\" must be {c}"));
                }
            }
        }
    }
}

/// Numeric equality across integer/float representations (`1 == 1.0`).
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// One structured run's live state. Values are staged per tool call by the
/// tool body and committed only by the turn loop from the authoritative
/// (post-hook) result — a hook that turns the call into an error means no
/// capture (dsh two-phase commit).
pub struct StructuredAttachment {
    schema: Value,
    staged: Mutex<HashMap<ToolCallId, Value>>,
    captured: Mutex<Option<Value>>,
}

impl StructuredAttachment {
    pub fn schema(&self) -> &Value {
        &self.schema
    }

    /// The committed value, once a valid call's final result succeeded.
    pub fn captured(&self) -> Option<Value> {
        self.captured.lock().expect("structured lock").clone()
    }

    pub fn is_captured(&self) -> bool {
        self.captured.lock().expect("structured lock").is_some()
    }

    /// Settle one `structured_output` call from its authoritative result.
    /// The stage is always consumed; only a success commits, first wins.
    pub fn settle(&self, call: &str, succeeded: bool) {
        let staged = self.staged.lock().expect("structured lock").remove(call);
        if let (Some(value), true) = (staged, succeeded) {
            let mut captured = self.captured.lock().expect("structured lock");
            if captured.is_none() {
                *captured = Some(value);
            }
        }
    }

    /// The child-scoped capture tool for this attachment.
    pub fn tool(self: &Arc<Self>) -> Arc<dyn Tool> {
        Arc::new(CaptureTool(Arc::clone(self)))
    }
}

/// In-memory attachments by child session. Shared by every clone of a
/// [`crate::tools::ToolRegistry`] so the child's scoped turn registry sees
/// what the delegating runtime attached.
#[derive(Default)]
pub struct StructuredOutputs(Mutex<HashMap<SessionId, Arc<StructuredAttachment>>>);

impl StructuredOutputs {
    /// Attach a schema that already passed [`check_object_schema`]. The
    /// returned guard detaches on drop (settle, error, or cancellation).
    pub fn attach(self: &Arc<Self>, session: &SessionId, schema: Value) -> Attached {
        let attachment = Arc::new(StructuredAttachment {
            schema,
            staged: Mutex::default(),
            captured: Mutex::default(),
        });
        self.0
            .lock()
            .expect("structured lock")
            .insert(session.clone(), Arc::clone(&attachment));
        Attached {
            owner: Arc::clone(self),
            session: session.clone(),
            attachment,
        }
    }

    pub fn get(&self, session: &str) -> Option<Arc<StructuredAttachment>> {
        self.0
            .lock()
            .expect("structured lock")
            .get(session)
            .cloned()
    }

    pub fn is_attached(&self, session: &str) -> bool {
        self.0
            .lock()
            .expect("structured lock")
            .contains_key(session)
    }
}

/// Ownership of one attachment; dropping it removes the registration.
pub struct Attached {
    owner: Arc<StructuredOutputs>,
    session: SessionId,
    attachment: Arc<StructuredAttachment>,
}

impl Attached {
    pub fn attachment(&self) -> &Arc<StructuredAttachment> {
        &self.attachment
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let mut map = self.owner.0.lock().expect("structured lock");
        if map
            .get(&self.session)
            .is_some_and(|current| Arc::ptr_eq(current, &self.attachment))
        {
            map.remove(&self.session);
        }
    }
}

struct CaptureTool(Arc<StructuredAttachment>);

#[async_trait]
impl Tool for CaptureTool {
    fn name(&self) -> &str {
        TOOL
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn input_schema(&self) -> Value {
        self.0.schema.clone()
    }
    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("structured_output requires a call id".into())
    }
    async fn execute_call(
        &self,
        _session: &SessionId,
        call: &str,
        args: Value,
        _cancel: &CancellationToken,
    ) -> Result<String, String> {
        let violations = validate(&self.0.schema, &args);
        if !violations.is_empty() {
            return Err(format!(
                "invalid structured output — fix these and call structured_output again:\n- {}",
                violations.join("\n- ")
            ));
        }
        self.0
            .staged
            .lock()
            .expect("structured lock")
            .insert(call.to_string(), args);
        Ok(RECORDED.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type":"object",
            "properties":{
                "issues":{"type":"array","items":{"type":"object","properties":{
                    "line":{"type":"integer"},
                    "severity":{"type":"string","enum":["low","high"]}
                },"required":["line"],"additionalProperties":false}},
                "summary":{"type":"string","description":"short"},
                "status":{"oneOf":[{"type":"string","const":"ok"},{"type":"null"}]}
            },
            "required":["issues"],
            "additionalProperties":false
        })
    }

    #[test]
    fn accepts_subset_and_rejects_everything_else() {
        assert!(check_object_schema(&schema()).is_ok());
        assert!(check_object_schema(&json!({"type":"object"})).is_ok());
        let bad = check_object_schema(&json!({
            "type":"object",
            "properties":{
                "a":{"type":"string","pattern":"x"},
                "b":{"type":["string","null"]},
                "c":{"type":"string","enum":[1]},
                "d":{"oneOf":[{"type":"string"}]},
                "e":{"type":"string","items":{}}
            },
            "required":["zz"],
            "additionalProperties":{}
        }))
        .unwrap_err();
        for needle in [
            "properties.a.pattern is not a supported keyword",
            "properties.b.type must be a single type string",
            "properties.c.enum must be a non-empty array of string values",
            "properties.d.oneOf must be an array of at least two schemas",
            "properties.e.items is not supported on type \"string\"",
            "required names \"zz\"",
            "additionalProperties must be a boolean",
        ] {
            assert!(bad.iter().any(|v| v.contains(needle)), "{needle}: {bad:?}");
        }
        assert!(
            check_object_schema(&json!({"type":"array"})).unwrap_err()[0].contains("object-rooted")
        );
        assert!(check_object_schema(&json!("x")).is_err());
    }

    #[test]
    fn validates_with_path_qualified_violations() {
        let s = schema();
        assert!(validate(
            &s,
            &json!({"issues":[{"line":3,"severity":"low"}],"status":null})
        )
        .is_empty());
        assert!(validate(&s, &json!({"issues":[{"line":3.0}]})).is_empty());
        let v = validate(
            &s,
            &json!({"issues":[{"line":"x"},{"severity":"mid","extra":1}],"status":"bad","other":1}),
        );
        for needle in [
            "\"issues[0].line\" must be an integer",
            "missing required property \"issues[1].line\"",
            "\"issues[1].severity\" must be one of [\"low\",\"high\"]",
            "\"issues[1].extra\" is not a declared property",
            "\"status\" must match exactly one oneOf branch (matched 0)",
            "\"other\" is not a declared property",
        ] {
            assert!(v.iter().any(|x| x.contains(needle)), "{needle}: {v:?}");
        }
        assert_eq!(
            validate(&s, &json!([1])),
            vec!["\"arguments\" must be an object".to_string()]
        );
    }

    #[tokio::test]
    async fn capture_is_two_phase_and_detaches_on_drop() {
        let outputs = Arc::new(StructuredOutputs::default());
        let session: SessionId = "child".into();
        let attached = outputs.attach(&session, schema());
        let tool = attached.attachment().tool();
        let cancel = CancellationToken::new();
        let err = tool
            .execute_call(&session, "c1", json!({"issues":"no"}), &cancel)
            .await
            .unwrap_err();
        assert!(err.contains("\"issues\" must be an array"));
        attached.attachment().settle("c1", false);
        assert!(!attached.attachment().is_captured());
        tool.execute_call(&session, "c2", json!({"issues":[]}), &cancel)
            .await
            .unwrap();
        // A hook that failed the result: stage consumed, nothing committed.
        attached.attachment().settle("c2", false);
        assert!(!attached.attachment().is_captured());
        tool.execute_call(&session, "c3", json!({"issues":[{"line":1}]}), &cancel)
            .await
            .unwrap();
        attached.attachment().settle("c3", true);
        assert_eq!(
            attached.attachment().captured(),
            Some(json!({"issues":[{"line":1}]}))
        );
        assert!(outputs.is_attached(&session));
        drop(attached);
        assert!(!outputs.is_attached(&session));
    }
}

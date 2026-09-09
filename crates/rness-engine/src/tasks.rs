//! Typed task snapshots; projection reads full ancestry, independently of compaction.
use rness_protocol::events::{TaskSnapshot, TaskStatus};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TasksConfig {
    pub allow_parallel_in_progress: bool,
}
impl Default for TasksConfig {
    fn default() -> Self { Self { allow_parallel_in_progress: true } }
}

pub fn validate(snapshot: &TaskSnapshot, config: &TasksConfig) -> Result<(), String> {
    let mut ids = std::collections::HashSet::new();
    for task in &snapshot.tasks {
        if task.id.trim().is_empty() || task.content.trim().is_empty() || !ids.insert(&task.id) {
            return Err("tasks require unique nonempty IDs and nonempty content".into());
        }
    }
    if !config.allow_parallel_in_progress && snapshot.tasks.iter().filter(|task| task.status == TaskStatus::InProgress).count() > 1 {
        return Err("only one task may be in_progress with this plugin configuration".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_snapshots_without_restricting_parallel_work_by_default() {
        let parse = |value| serde_json::from_value::<TaskSnapshot>(value).unwrap();
        let mut snapshot = parse(serde_json::json!({"tasks":[
            {"id":"a","content":"first","status":"in_progress"},
            {"id":"b","content":"second","status":"in_progress"}
        ]}));
        assert!(validate(&snapshot, &TasksConfig::default()).is_ok());
        assert!(validate(&snapshot, &TasksConfig { allow_parallel_in_progress: false }).is_err());
        snapshot.tasks[1].id = "a".into();
        assert!(validate(&snapshot, &TasksConfig::default()).is_err());
        snapshot.tasks.pop();
        snapshot.tasks[0].content = " \n".into();
        assert!(validate(&snapshot, &TasksConfig::default()).is_err());
        assert!(validate(&TaskSnapshot::default(), &TasksConfig::default()).is_ok());
        assert!(serde_json::from_value::<TaskSnapshot>(serde_json::json!({"tasks":[{"id":"a","content":"x","status":"wrong"}]})).is_err());
    }
}

pub struct TaskWrite(pub TasksConfig);
#[async_trait::async_trait]
impl crate::tools::Tool for TaskWrite {
    fn name(&self) -> &str { "TaskWrite" }
    fn description(&self) -> &str {
        "Replace this session's entire task list. Preserve IDs when updating tasks. Omitted tasks are removed; an empty list clears it. Statuses: pending, in_progress, completed."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","additionalProperties":false,"required":["tasks"],"properties":{"tasks":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["id","content","status"],"properties":{"id":{"type":"string","minLength":1},"content":{"type":"string","minLength":1},"status":{"type":"string","enum":["pending","in_progress","completed"]}}}}}})
    }
    async fn execute(&self, _: serde_json::Value) -> Result<String, String> {
        Err("TaskWrite requires durable agent dispatch".into())
    }
    async fn execute_with_tasks(&self, _: &String, _: &str, args: serde_json::Value, cancel: &tokio_util::sync::CancellationToken) -> Result<(String, Option<TaskSnapshot>), String> {
        if cancel.is_cancelled() { return Err("task update cancelled".into()); }
        let snapshot: TaskSnapshot = serde_json::from_value(args).map_err(|error| error.to_string())?;
        validate(&snapshot, &self.0)?;
        let output = serde_json::to_string(&snapshot).map_err(|error| error.to_string())?;
        Ok((output, Some(snapshot)))
    }
}

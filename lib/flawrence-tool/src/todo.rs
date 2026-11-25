use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::tool_args::optional_string_field_from_value;
use crate::{ToolContext, ToolDef};
use flawrence_chat::UsageByModel;

/// Deserialize the todo `id` field strictly: accept only a string (or
/// null/absent), erroring with a teaching message otherwise (see
/// [`crate::tool_args`] and `docs/SPEC_TOOL_ID_ROBUSTNESS.md`).
fn deserialize_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    optional_string_field_from_value(value, "id")
}

#[derive(Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "snake_case")]
pub enum TodoOperation {
    /// Add a new todo item.
    Add,
    /// Update the content of an existing todo by ID.
    Update,
    /// Delete a todo item by ID.
    Delete,
    /// Mark a todo item as complete by ID.
    Complete,
    /// List all todo items with their status.
    List,
}

#[derive(Deserialize, JsonSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct TodoArgs {
    /// The operation to perform.
    pub operation: TodoOperation,
    /// Content for the todo item. Required for add and update; null for every other
    /// operation.
    #[schemars(length(max = 500))]
    pub content: Option<String>,
    /// ID of the todo item, **as a string**. Required for update and delete.
    /// Null means "all pending items" for complete, and is ignored by add and
    /// list.
    ///
    /// Always pass IDs as quoted strings. The IDs shown in tool output look
    /// like `t1` — pass them back as the string `"t1"`, never as a bare
    /// number or unquoted token. Example: `{"id": "t1", "operation": "complete"}`.
    #[schemars(length(max = 50))]
    #[serde(deserialize_with = "deserialize_id", default)]
    pub id: Option<String>,
}

impl ToolDef for TodoArgs {
    const NAME: &'static str = "todo";
    const DESCRIPTION: &'static str = "Manage a todo list for the current task. Use to track required work items. \
        Operations: add (content required), update (id + content required), delete (id required), \
        complete (optional id; without id completes all pending items), list (no args). \
        The list starts empty unless your agent defines todos: only items you can see via `list` exist. \
        Any todo that IS pending blocks task completion, so finish the ones you added — but if you \
        added none, there is nothing to complete and nothing blocking you.";

    async fn execute(ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        let mut todo_list = ctx.todo_list.lock().await;

        match args.operation {
            TodoOperation::Add => {
                let content = args
                    .content
                    .ok_or_else(|| anyhow::anyhow!("'content' is required for add operation"))?;
                let id = todo_list.add(content.clone(), args.id);
                let list = todo_list.format();
                Ok((
                    format!("Added todo '{}' (id: \"{}\")\n\n{}", content, id, list),
                    None,
                ))
            }
            TodoOperation::Update => {
                let id = args
                    .id
                    .ok_or_else(|| anyhow::anyhow!("'id' is required for update operation"))?;
                let content = args
                    .content
                    .ok_or_else(|| anyhow::anyhow!("'content' is required for update operation"))?;
                todo_list
                    .update(&id, content)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let list = todo_list.format();
                Ok((format!("Updated todo (id: \"{}\").\n\n{}", id, list), None))
            }
            TodoOperation::Delete => {
                let id = args
                    .id
                    .ok_or_else(|| anyhow::anyhow!("'id' is required for delete operation"))?;
                todo_list
                    .delete(&id)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let list = todo_list.format();
                Ok((format!("Deleted todo (id: \"{}\").\n\n{}", id, list), None))
            }
            TodoOperation::Complete => {
                let message = if let Some(id) = args.id {
                    todo_list
                        .complete_or_record(&id, args.content.clone())
                        .map_err(|e| anyhow::anyhow!("{}", e))?;
                    format!("Completed todo '{}'.", id)
                } else {
                    let completed = todo_list.complete_all();
                    format!("Completed all pending todos ({}).", completed)
                };
                let list = todo_list.format();
                Ok((format!("{}\n\n{}", message, list), None))
            }
            TodoOperation::List => {
                let list = todo_list.format();
                Ok((list, None))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo_list::TodoList;

    #[test]
    fn test_deserialize_id_accepts_string() {
        let args: TodoArgs = serde_json::from_str(r#"{"operation":"list","id":"abc"}"#).unwrap();
        assert_eq!(args.id.as_deref(), Some("abc"));
    }

    #[test]
    fn test_deserialize_id_rejects_integer_with_teaching_message() {
        // Strict: an integer is NOT coerced. The error must name the field,
        // report the type, and give the exact corrected form so a weak model
        // can self-correct.
        let err =
            serde_json::from_str::<TodoArgs>(r#"{"operation":"complete","id":1}"#).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'id' must be a string"), "got: {msg}");
        assert!(msg.contains("number"), "got: {msg}");
        assert!(msg.contains("\"id\": \"1\""), "got: {msg}");
    }

    #[test]
    fn test_deserialize_id_rejects_large_integer_with_corrected_value() {
        let err =
            serde_json::from_str::<TodoArgs>(r#"{"operation":"delete","id":42}"#).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("number"), "got: {msg}");
        assert!(msg.contains("\"id\": \"42\""), "got: {msg}");
    }

    #[test]
    fn test_deserialize_id_rejects_bool_with_teaching_message() {
        let err =
            serde_json::from_str::<TodoArgs>(r#"{"operation":"list","id":true}"#).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'id' must be a string"), "got: {msg}");
        assert!(msg.contains("boolean"), "got: {msg}");
        assert!(msg.contains("string"), "got: {msg}");
    }

    #[test]
    fn test_deserialize_id_rejects_array_with_teaching_message() {
        let res: Result<TodoArgs, _> = serde_json::from_str(r#"{"operation":"list","id":[1,2]}"#);
        let err = res.unwrap_err().to_string();
        assert!(err.contains("array"), "got: {err}");
        assert!(err.contains("string"), "got: {err}");
    }

    #[test]
    fn test_deserialize_id_absent_is_none() {
        let args: TodoArgs = serde_json::from_str(r#"{"operation":"list"}"#).unwrap();
        assert!(args.id.is_none());
    }

    #[test]
    fn test_deserialize_id_null_is_none() {
        let args: TodoArgs = serde_json::from_str(r#"{"operation":"list","id":null}"#).unwrap();
        assert!(args.id.is_none());
    }

    #[test]
    fn test_auto_id_is_prefixed_and_quoted_in_output() {
        // Layer 1: auto-generated IDs are prefixed (t1) and rendered quoted,
        // so the model does not round-trip them as integers.
        let mut list = TodoList::new(&[]);
        let id = list.add("task".into(), None);
        assert_eq!(id, "t1");
        let formatted = list.format();
        assert!(formatted.contains("(id: \"t1\")"), "got: {formatted}");
    }

    #[test]
    fn test_complete_with_string_id_matches_auto_generated() {
        // End-to-end with the strict-teaching design: the agent passes the
        // string "t1" it saw in the quoted output, and it completes the todo.
        let mut list = TodoList::new(&[]);
        let id = list.add("task".into(), None);
        assert_eq!(id, "t1");
        let args: TodoArgs = serde_json::from_str(r#"{"operation":"complete","id":"t1"}"#).unwrap();
        list.complete(args.id.as_deref().unwrap()).unwrap();
        assert!(list.all_complete());
    }
}

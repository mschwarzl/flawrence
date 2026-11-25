use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared todo list type for passing between tasks and tools.
pub type SharedTodoList = Arc<Mutex<TodoList>>;

/// Status of a todo item.
#[derive(Debug, Clone, PartialEq)]
pub enum TodoStatus {
    Pending,
    Complete,
}

impl std::fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoStatus::Pending => write!(f, "pending"),
            TodoStatus::Complete => write!(f, "complete"),
        }
    }
}

/// A single todo item.
#[derive(Debug, Clone)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

/// A list of todo items scoped to a single task.
#[derive(Debug, Default)]
pub struct TodoList {
    items: Vec<TodoItem>,
    next_id: u32,
}

impl TodoList {
    /// Create a new TodoList, optionally seeded with pending items from agent config.
    pub fn new(initial_items: &[String]) -> Self {
        let mut list = Self {
            items: Vec::new(),
            next_id: 1,
        };
        for content in initial_items {
            list.add(content.clone(), None);
        }
        list
    }

    /// Create a new shared TodoList.
    pub fn new_shared(initial_items: &[String]) -> SharedTodoList {
        Arc::new(Mutex::new(Self::new(initial_items)))
    }

    /// Add a new todo item. Returns the assigned ID.
    ///
    /// Auto-generated IDs are prefixed with `t` (e.g. `t1`, `t2`) so they are
    /// unambiguously strings and weak models do not round-trip them as JSON
    /// integers. Caller-provided IDs are used verbatim.
    pub fn add(&mut self, content: String, id: Option<String>) -> String {
        let id = id.unwrap_or_else(|| {
            let id = format!("t{}", self.next_id);
            self.next_id += 1;
            id
        });
        // Ensure next_id stays ahead of any user-provided numeric IDs
        self.reserve_id(&id);
        // Re-adding an existing id must not duplicate it. A duplicate is
        // unreachable by `complete`, which marks only the first match, so the
        // twin stays pending and blocks task completion forever.
        if let Some(existing) = self.items.iter_mut().find(|i| i.id == id) {
            existing.content = content;
            return id;
        }
        self.items.push(TodoItem {
            id: id.clone(),
            content,
            status: TodoStatus::Pending,
        });
        id
    }

    /// Record `id` as done, whether or not it is tracked yet.
    ///
    /// A turn's tool calls run concurrently (`join_all` in the task loop), so a
    /// model that emits `add` and `complete` for one id in the same turn can
    /// have the `complete` execute first. Failing it makes the result depend on
    /// scheduling; recording the work as already finished converges on the same
    /// list under either order, and `content` is what the call already carries,
    /// so nothing is invented. Without content there is nothing to record and
    /// the caller does need to hear about it.
    pub fn complete_or_record(&mut self, id: &str, content: Option<String>) -> Result<(), String> {
        if self.complete(id).is_ok() {
            return Ok(());
        }
        let Some(content) = content else {
            return Err(format!("Todo '{}' not found. {}", id, self.list_hint()));
        };
        self.reserve_id(id);
        self.items.push(TodoItem {
            id: id.to_string(),
            content,
            status: TodoStatus::Complete,
        });
        Ok(())
    }

    /// Keep the generated-id counter ahead of any caller-supplied numeric id,
    /// so a later auto-generated id cannot collide with one already in use.
    fn reserve_id(&mut self, id: &str) {
        if let Ok(n) = id.parse::<u32>()
            && n >= self.next_id
        {
            self.next_id = n + 1;
        }
    }

    /// Update the content of a todo item by ID.
    pub fn update(&mut self, id: &str, content: String) -> Result<(), String> {
        let item = self.items.iter_mut().find(|i| i.id == id);
        match item {
            Some(item) => {
                item.content = content;
                Ok(())
            }
            None => Err(format!("Todo '{}' not found. {}", id, self.list_hint())),
        }
    }

    /// Delete a todo item by ID.
    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        let idx = self.items.iter().position(|i| i.id == id);
        match idx {
            Some(idx) => {
                self.items.remove(idx);
                Ok(())
            }
            None => Err(format!("Todo '{}' not found. {}", id, self.list_hint())),
        }
    }

    /// Mark a todo item as complete by ID.
    pub fn complete(&mut self, id: &str) -> Result<(), String> {
        let item = self.items.iter_mut().find(|i| i.id == id);
        match item {
            Some(item) => {
                item.status = TodoStatus::Complete;
                Ok(())
            }
            None => Err(format!("Todo '{}' not found. {}", id, self.list_hint())),
        }
    }

    /// Force-complete every pending todo. Used when a provider drops the
    /// optional ID, and by the task loop's bounded exit-guard escape to
    /// release a stuck agent rather than looping until `MAX_ITERATIONS`.
    /// Returns the number of items that were pending.
    pub fn complete_all(&mut self) -> usize {
        let pending = self.pending().len();
        for item in self.items.iter_mut() {
            item.status = TodoStatus::Complete;
        }
        pending
    }

    /// Check if all items are complete (or list is empty).
    pub fn all_complete(&self) -> bool {
        self.items.iter().all(|i| i.status == TodoStatus::Complete)
    }

    /// Get pending items.
    pub fn pending(&self) -> Vec<&TodoItem> {
        self.items
            .iter()
            .filter(|i| i.status == TodoStatus::Pending)
            .collect()
    }

    /// Format the todo list for display.
    pub fn format(&self) -> String {
        if self.items.is_empty() {
            return "No todos.".to_string();
        }
        self.items
            .iter()
            .map(|item| {
                let marker = match item.status {
                    TodoStatus::Pending => "[ ]",
                    TodoStatus::Complete => "[x]",
                };
                format!("{} {} (id: \"{}\")", marker, item.content, item.id)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Format pending items only (for exit guard messages).
    pub fn format_pending(&self) -> String {
        let pending = self.pending();
        if pending.is_empty() {
            return "All todos complete.".to_string();
        }
        pending
            .iter()
            .map(|item| format!("- {} (id: \"{}\")", item.content, item.id))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether the list has any items at all.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn list_hint(&self) -> String {
        if self.items.is_empty() {
            "No todos exist, so there is nothing to complete and nothing blocking completion of \
             this task. Use operation=add if you want to track work."
                .to_string()
        } else {
            let ids: Vec<String> = self.items.iter().map(|i| format!("\"{}\"", i.id)).collect();
            format!("Available IDs: {}", ids.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    /// A turn's tool calls run concurrently, so `add` and `complete` for one id
    /// can execute in either order. Both must converge on a single completed
    /// item: a stranded duplicate stays pending and blocks the task forever.
    #[test]
    fn add_and_complete_converge_in_either_order() {
        // complete lands first (the observed failure)
        let mut first = TodoList::default();
        first
            .complete_or_record("1", Some("Verify range checks".to_string()))
            .expect("recorded");
        first.add("Verify range checks".to_string(), Some("1".to_string()));
        assert_eq!(first.items.len(), 1, "{:?}", first.items);
        assert_eq!(first.items[0].status, TodoStatus::Complete);

        // add lands first (the intended order)
        let mut second = TodoList::default();
        second.add("Verify range checks".to_string(), Some("1".to_string()));
        second
            .complete_or_record("1", Some("Verify range checks".to_string()))
            .expect("completed");
        assert_eq!(second.items.len(), 1, "{:?}", second.items);
        assert_eq!(second.items[0].status, TodoStatus::Complete);
    }

    /// Re-adding an id must not create a twin. `complete` marks only the first
    /// match, so a duplicate would remain pending and block task completion.
    #[test]
    fn re_adding_an_id_updates_it_instead_of_duplicating() {
        let mut list = TodoList::default();
        list.add("first wording".to_string(), Some("t1".to_string()));
        list.add("second wording".to_string(), Some("t1".to_string()));
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].content, "second wording");
        list.complete("t1").expect("completed");
        assert!(list.items.iter().all(|i| i.status == TodoStatus::Complete));
    }

    /// Completing an id that was never tracked, with no content to record, is
    /// still worth telling the caller about — there is nothing to converge on.
    #[test]
    fn completing_an_unknown_id_without_content_still_reports() {
        let mut list = TodoList::default();
        let error = list
            .complete_or_record("t1", None)
            .expect_err("should report");
        assert!(error.contains("t1"), "{error}");
        assert!(list.items.is_empty());
    }

    use super::*;

    #[test]
    fn test_new_with_initial_items() {
        let list = TodoList::new(&["item 1".into(), "item 2".into()]);
        assert_eq!(list.items.len(), 2);
        assert!(list.items.iter().all(|i| i.status == TodoStatus::Pending));
    }

    #[test]
    fn test_add_and_complete() {
        let mut list = TodoList::default();
        let id = list.add("test task".into(), None);
        assert!(!list.all_complete());
        list.complete(&id).unwrap();
        assert!(list.all_complete());
    }

    #[test]
    fn test_complete_all() {
        let mut list = TodoList::new(&["one".into(), "two".into()]);
        assert_eq!(list.complete_all(), 2);
        assert!(list.all_complete());
        assert_eq!(list.complete_all(), 0);
    }

    #[test]
    fn test_delete() {
        let mut list = TodoList::default();
        let id = list.add("test".into(), None);
        assert!(!list.is_empty());
        list.delete(&id).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_update() {
        let mut list = TodoList::default();
        let id = list.add("old".into(), None);
        list.update(&id, "new".into()).unwrap();
        assert_eq!(list.items[0].content, "new");
    }

    #[test]
    fn test_empty_list_is_all_complete() {
        let list = TodoList::default();
        assert!(list.all_complete());
    }

    #[test]
    fn test_custom_id() {
        let mut list = TodoList::default();
        let id = list.add("custom".into(), Some("my_id".into()));
        assert_eq!(id, "my_id");
    }

    #[test]
    fn test_auto_id_is_prefixed() {
        // Auto-generated IDs are prefixed with `t` so they are unambiguously
        // strings and weak models do not round-trip them as JSON integers.
        let mut list = TodoList::new(&[]);
        assert_eq!(list.add("a".into(), None), "t1");
        assert_eq!(list.add("b".into(), None), "t2");
    }

    #[test]
    fn test_format_renders_ids_quoted() {
        let mut list = TodoList::new(&[]);
        list.add("task".into(), None);
        let formatted = list.format();
        assert!(formatted.contains("(id: \"t1\")"), "got: {formatted}");
    }

    #[test]
    fn test_list_hint_renders_ids_quoted() {
        let mut list = TodoList::new(&[]);
        list.add("task".into(), None);
        let hint = list.list_hint();
        assert!(hint.contains("\"t1\""), "got: {hint}");
    }

    #[test]
    fn test_format_pending_renders_ids_quoted() {
        let mut list = TodoList::new(&[]);
        list.add("task".into(), None);
        let pending = list.format_pending();
        assert!(pending.contains("(id: \"t1\")"), "got: {pending}");
    }
}

//! Event log for streaming task execution traces.
//!
//! A single [`EventLog`] is created at the root of a task tree and shared
//! through `Arc` to every child task. All emit sites — both in the task
//! loop and inside subagent spawns — feed the same log, so the resulting
//! transcript captures the entire hierarchy ordered by emission time.
//!
//! ## Concurrency model
//!
//! - **History** retains the newest `FLAWRENCE_WEB_MAX_EVENTS` events (default
//!   1,000) in a ring buffer. Evicted payloads are released.
//! - **Live subscribers** wait on a [`tokio::sync::Notify`]. After each
//!   emit they wake up, read the new tail of history under the read lock,
//!   yield each event, and go back to sleep. Slow subscribers resume at the
//!   oldest retained event if they fall behind the window.
//! - **Sequence numbers** are assigned under the history lock and are
//!   monotonic across the entire task tree.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs::{self, File},
    io::{BufWriter, Write},
    num::NonZeroUsize,
    path::Path,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, RwLock};
use tokio_stream::Stream;

use flawrence_chat::ToolCall;

/// Convenience alias — the same shape as `SharedArtifactStore`.
pub type SharedEventLog = Arc<EventLog>;

/// A summary of a single tool call as emitted in `AssistantMessage` events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

impl From<&ToolCall> for ToolCallSummary {
    fn from(tc: &ToolCall) -> Self {
        Self {
            call_id: tc.call_id.clone(),
            name: tc.fn_name.clone(),
            arguments: tc.fn_arguments.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerDecision {
    Approved,
    Rejected,
}

/// One event in the task transcript.
///
/// Tagged-enum serialisation gives every variant a `"type"` discriminator
/// that downstream consumers (SSE clients, transcript readers) can switch
/// on without inspecting fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TaskEvent {
    /// Emitted exactly once at the start of every task loop.
    TaskStarted {
        parent_task_id: Option<String>,
        agent: String,
        query: String,
        title: String,
        iter_cap: u32,
    },
    /// Emitted at the top of every loop iteration.
    IterationStarted { iteration: u32 },
    /// The model's reply for this iteration. Whitespace-only text is
    /// already filtered upstream; if both `content` and `reasoning` are
    /// empty and `tool_calls` is non-empty, this is a tool-only turn.
    AssistantMessage {
        content: String,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCallSummary>,
    },
    /// Emitted just before each tool's `execute` runs.
    ToolCallStarted {
        call_id: String,
        tool: String,
        args: serde_json::Value,
    },
    /// Emitted when a tool's `execute` returns (or errors).
    ToolCallCompleted {
        call_id: String,
        tool: String,
        ok: bool,
        output: serde_json::Value,
        duration_ms: u64,
    },
    /// Emitted from the parent's loop, just before the child's loop boots.
    SubagentSpawned {
        child_task_id: String,
        agent: String,
        title: String,
        query: String,
    },
    /// Emitted from the parent's loop after the child's loop returns.
    SubagentCompleted {
        child_task_id: String,
        output: String,
        usage: serde_json::Value,
    },
    /// Single emission per API response that included usage (server-side
    /// totals for prompt/completion tokens).
    TokenUsage {
        model: String,
        prompt_tokens: u32,
        completion_tokens: u32,
        cumulative_cost_usd: f64,
    },
    /// Output reviewer outcome for a `submit_result` call.
    ReviewerOutcome {
        decision: ReviewerDecision,
        reason: String,
    },
    /// Non-fatal runtime warning surfaced live to the UI transcript,
    /// e.g. an API call that failed and is being retried. Lets the user
    /// SEE stalls/timeouts instead of a silently hung run.
    Warning { message: String },
    /// Terminal — task finished with a result.
    TaskCompleted {
        output: serde_json::Value,
        elapsed_secs: f64,
        usage: serde_json::Value,
    },
    /// Terminal — task ended with an error.
    TaskFailed { error: String },
}

impl TaskEvent {
    /// Returns true when the variant is a terminal state.
    /// SSE handlers use this to know when to close the stream.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::TaskCompleted { .. } | Self::TaskFailed { .. })
    }
}

/// A timestamped, sequenced wrapper around a [`TaskEvent`].
///
/// `task_id` lives here (not inside `TaskEvent`) so every variant carries
/// it without repetition. Use it to attribute an event to the exact loop
/// that emitted it; use `TaskStarted::parent_task_id` and the
/// `Subagent*` variants to rebuild the tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequencedEvent {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub task_id: String,
    pub event: TaskEvent,
}

/// Dashboard history limit when `FLAWRENCE_WEB_MAX_EVENTS` is absent or invalid.
pub const DEFAULT_MAX_EVENTS: NonZeroUsize = NonZeroUsize::new(1_000).unwrap();
const MAX_DASHBOARD_EVENT_BYTES: usize = 64 * 1024;
const EVENT_PREVIEW_BYTES: usize = 4 * 1024;

#[derive(Default)]
struct EventPreview {
    bytes: Vec<u8>,
    seen_bytes: usize,
}

impl Write for EventPreview {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let length = EVENT_PREVIEW_BYTES
            .saturating_sub(self.bytes.len())
            .min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..length]);
        self.seen_bytes = self.seen_bytes.saturating_add(bytes.len());
        if self.seen_bytes > MAX_DASHBOARD_EVENT_BYTES {
            return Err(std::io::Error::other(
                "dashboard event exceeds display limit",
            ));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn dashboard_event(event: TaskEvent) -> TaskEvent {
    // ponytail: fixed 64 KiB display ceiling; keep full payloads in artifacts and disk transcripts.
    let mut preview = EventPreview::default();
    if serde_json::to_writer(&mut preview, &event).is_ok() {
        return event;
    }
    let message = format!(
        "Large dashboard event shortened (over 64 KiB). See related artifacts \
         or a configured disk transcript for full output.\nPreview:\n{}",
        String::from_utf8_lossy(&preview.bytes)
    );
    match event {
        TaskEvent::TaskCompleted { elapsed_secs, .. } => TaskEvent::TaskCompleted {
            output: json!(message),
            elapsed_secs,
            usage: json!({}),
        },
        TaskEvent::TaskFailed { .. } => TaskEvent::TaskFailed { error: message },
        _ => TaskEvent::Warning { message },
    }
}

fn configured_max_events(raw: Option<&str>) -> NonZeroUsize {
    raw.and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| {
            if raw.is_some() {
                tracing::warn!(
                    "FLAWRENCE_WEB_MAX_EVENTS must be a positive integer; using {DEFAULT_MAX_EVENTS}"
                );
            }
            DEFAULT_MAX_EVENTS
        })
}

#[derive(Default)]
struct History {
    events: VecDeque<SequencedEvent>,
    next_seq: u64,
    // Checkpoints need child status even after the detailed trace has rolled off.
    children: BTreeMap<String, (String, Value)>,
    finished: HashSet<String>,
    transcript: Option<TranscriptWriter>,
    transcript_error: Option<String>,
}

struct TranscriptWriter {
    writer: BufWriter<File>,
    has_events: bool,
}

impl TranscriptWriter {
    fn append(&mut self, event: &SequencedEvent) -> anyhow::Result<()> {
        if self.has_events {
            self.writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut self.writer, event)?;
        self.has_events = true;
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<()> {
        self.writer.write_all(b"],\"final\":true}")?;
        self.writer.flush()?;
        Ok(())
    }
}

impl History {
    fn snapshot_since(&self, since: Option<u64>) -> Vec<SequencedEvent> {
        self.events
            .iter()
            .filter(|event| since.is_none_or(|seq| event.seq > seq))
            .cloned()
            .collect()
    }

    fn record_child_status(&mut self, task_id: &str, event: &TaskEvent) {
        match event {
            TaskEvent::SubagentSpawned {
                child_task_id,
                agent,
                title,
                query,
            } => {
                self.children.insert(child_task_id.clone(), (task_id.to_string(),
                    json!({"agent": agent, "title": title, "query": query, "status": "started"})));
            }
            TaskEvent::TaskCompleted { .. } | TaskEvent::TaskFailed { .. } => {
                self.finished.insert(task_id.to_string());
                if let Some((_, child)) = self.children.get_mut(task_id) {
                    child["status"] = json!(if matches!(event, TaskEvent::TaskFailed { .. }) {
                        "failed"
                    } else {
                        "completed"
                    });
                    if let TaskEvent::TaskFailed { error } = event {
                        child["error"] = json!(error);
                    }
                }
            }
            TaskEvent::SubagentCompleted { child_task_id, .. } => {
                if let Some((parent, child)) = self.children.get_mut(child_task_id)
                    && parent == task_id
                    && child["status"] == "started"
                {
                    child["status"] = json!("returned_or_cancelled");
                }
            }
            _ => {}
        }
    }
}

/// A bounded recent event log shared across the entire task tree.
///
/// Artifacts and compact checkpoint child-status metadata are independent of
/// event retention. Detailed event payloads are limited to 64 KiB, with larger events replaced by
/// a 4 KiB preview. The agents' working state and full disk transcript are not
/// truncated. This does not limit the total memory used by an analysis.
pub struct EventLog {
    history: RwLock<History>,
    notify: Notify,
    max_events: NonZeroUsize,
}

impl EventLog {
    /// Retain the newest `FLAWRENCE_WEB_MAX_EVENTS` events, defaulting to 1,000.
    ///
    /// Missing, zero, or invalid values use the default.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Self::new_with_limit(configured_max_events(
            std::env::var("FLAWRENCE_WEB_MAX_EVENTS").ok().as_deref(),
        ))
    }

    /// Create a log with an explicit positive retention limit.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::num::NonZeroUsize;
    /// use flawrence_tool::event_log::EventLog;
    ///
    /// let log = EventLog::new_with_limit(NonZeroUsize::MIN);
    /// assert_eq!(log.max_events(), 1);
    /// ```
    #[must_use]
    pub fn new_with_limit(max_events: NonZeroUsize) -> Arc<Self> {
        Arc::new(Self {
            history: RwLock::new(History::default()),
            notify: Notify::new(),
            max_events,
        })
    }

    /// Maximum number of detailed events retained by this log.
    #[must_use]
    pub fn max_events(&self) -> usize {
        self.max_events.get()
    }

    /// Stream a complete transcript to disk independently of the recent-event window.
    ///
    /// Call before emitting any events, then call [`Self::finish_transcript`]
    /// after the run. Only a fixed-size file buffer is retained in memory.
    ///
    /// # Errors
    ///
    /// Returns an error if events have already been emitted, a transcript is
    /// already enabled, or the output file cannot be created.
    pub async fn enable_transcript(
        &self,
        path: impl AsRef<Path>,
        job_id: &str,
    ) -> anyhow::Result<()> {
        let mut history = self.history.write().await;
        anyhow::ensure!(
            history.next_seq == 0 && history.transcript.is_none(),
            "Enable the transcript before emitting events"
        );
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let mut writer = BufWriter::new(File::create(path)?);
        write!(
            writer,
            "{{\"job_id\":{},\"events\":[",
            serde_json::to_string(job_id)?
        )?;
        history.transcript = Some(TranscriptWriter {
            writer,
            has_events: false,
        });
        Ok(())
    }

    /// Finish a complete disk transcript without reading event history into RAM.
    ///
    /// # Errors
    ///
    /// Returns an error if a transcript was not enabled, an event could not be
    /// written, or the file cannot be finished and flushed.
    pub async fn finish_transcript(&self) -> anyhow::Result<()> {
        let mut history = self.history.write().await;
        if let Some(error) = history.transcript_error.take() {
            anyhow::bail!("Transcript write failed: {error}");
        }
        history
            .transcript
            .take()
            .ok_or_else(|| anyhow::anyhow!("Transcript is not enabled"))?
            .finish()
    }

    /// Direct child statuses for a checkpoint, including children whose events
    /// have been evicted. Detailed messages and tool outputs are not retained here.
    pub async fn child_ledger(&self, parent: &str) -> Value {
        let history = self.history.read().await;
        let children: BTreeMap<_, _> = history
            .children
            .iter()
            .filter(|(_, (owner, _))| owner == parent)
            .map(|(id, (_, state))| (id, state))
            .collect();
        json!(children)
    }

    /// Record an event. Assigns a monotonic sequence number, timestamps
    /// it, and wakes every waiting subscriber.
    pub async fn emit(&self, task_id: &str, event: TaskEvent) {
        let mut history = self.history.write().await;
        let seq = history.next_seq;
        history.next_seq = history.next_seq.saturating_add(1);
        history.record_child_status(task_id, &event);
        let mut se = SequencedEvent {
            seq,
            ts: Utc::now(),
            task_id: task_id.to_string(),
            event,
        };
        if let Some(transcript) = &mut history.transcript
            && let Err(error) = transcript.append(&se)
        {
            tracing::warn!(%error, "Failed to append transcript event");
            history.transcript_error = Some(error.to_string());
            history.transcript = None;
        }
        se.event = dashboard_event(se.event);
        if history.events.len() == self.max_events() {
            history.events.pop_front();
        }
        history.events.push_back(se);
        drop(history);
        self.notify.notify_waiters();
    }

    /// Snapshot retained events with `seq > since`, in sequence order.
    ///
    /// `None` or an evicted cursor starts at the oldest retained event.
    pub async fn snapshot_since(&self, since: Option<u64>) -> Vec<SequencedEvent> {
        self.history.read().await.snapshot_since(since)
    }

    /// Replay retained events with `seq > since`, then stream new events.
    ///
    /// Slow consumers skip evicted history and resume within the retained
    /// window. Each subscriber buffers at most one window of event clones.
    ///
    /// `terminate_on` is the task_id whose terminal event
    /// (`TaskCompleted`/`TaskFailed`) should close the stream — pass
    /// the root task's id so subagent terminations don't truncate the
    /// view of an in-progress run. `None` means never auto-close (used
    /// in tests and in-process consumers).
    pub fn subscribe(
        self: Arc<Self>,
        since: Option<u64>,
        terminate_on: Option<String>,
    ) -> impl Stream<Item = SequencedEvent> {
        // `last_seen` is `Option<u64>`; `None` means we have not yielded
        // anything yet, so seq = 0 still counts as "new".
        let mut last_seen: Option<u64> = since;

        async_stream::stream! {
            loop {
                // Subscribe to the notify *before* reading history, so
                // any emit racing between the read and the wait-arm is
                // already armed and won't be lost.
                //
                // CRITICAL: `Notify::notified()` only constructs the
                // Notified future; it does NOT register it as a waiter.
                // Registration happens on first poll, which `.await`
                // does. `Notify::notify_waiters()` only wakes
                // *registered* waiters and does not store permits, so
                // any emit between this construction and the eventual
                // `.await` would be silently dropped.
                //
                // `Notified::enable()` performs the registration
                // up-front so racing emits during the history-read and
                // yield phases below stash a wakeup we'll consume at
                // `.await`. Without this, subscribers that drained the
                // initial replay would park forever and never see new
                // events.
                let notified = self.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                let (new_events, finished) = {
                    let history = self.history.read().await;
                    (history.snapshot_since(last_seen), terminate_on.as_deref()
                        .is_some_and(|target| history.finished.contains(target)))
                };

                let mut yielded_terminal = false;
                for ev in new_events {
                    last_seen = Some(ev.seq);
                    // Only the *target* task's terminal event closes
                    // this stream. Subagent completions arrive on the
                    // same shared log but must not truncate a view of
                    // the parent task — they're just another step in
                    // the parent's narrative.
                    if let Some(target) = terminate_on.as_deref()
                        && ev.event.is_terminal() && ev.task_id == target {
                            yielded_terminal = true;
                        }
                    yield ev;
                    if yielded_terminal {
                        return;
                    }
                }

                if finished {
                    return;
                }

                // Wait for the next emit (or wake spuriously and re-check).
                notified.await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn limited_log(limit: usize) -> Arc<EventLog> {
        EventLog::new_with_limit(NonZeroUsize::new(limit).unwrap())
    }

    #[test]
    fn event_limit_is_positive_and_invalid_values_use_the_default() {
        assert_eq!(configured_max_events(Some(" 2 ")).get(), 2);
        for raw in [
            None,
            Some("0"),
            Some("-1"),
            Some("bad"),
            Some("9999999999999999999999999999"),
        ] {
            assert_eq!(configured_max_events(raw), DEFAULT_MAX_EVENTS);
        }
    }

    #[tokio::test]
    async fn oversized_dashboard_payload_is_shortened_but_disk_transcript_stays_complete() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("run.json");
        let log = limited_log(1);
        log.enable_transcript(&path, "run").await.unwrap();
        let output = format!(
            "{}END_OF_FULL_OUTPUT",
            "🔒".repeat(MAX_DASHBOARD_EVENT_BYTES)
        );
        log.emit(
            "root",
            TaskEvent::ToolCallCompleted {
                call_id: "large".into(),
                tool: "artifact".into(),
                ok: true,
                output: json!(output),
                duration_ms: 1,
            },
        )
        .await;
        let snapshot = log.snapshot_since(None).await;
        let TaskEvent::Warning { message } = &snapshot[0].event else {
            panic!("oversized event must be replaced with a preview");
        };
        assert!(message.contains("shortened"));
        assert!(!message.contains("END_OF_FULL_OUTPUT"));
        assert!(message.len() < EVENT_PREVIEW_BYTES + 512);
        assert!(serde_json::to_vec(&snapshot[0]).unwrap().len() < MAX_DASHBOARD_EVENT_BYTES);
        log.finish_transcript().await.unwrap();
        let transcript: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            transcript["events"][0]["event"]["type"],
            "ToolCallCompleted"
        );
        assert_eq!(transcript["events"][0]["event"]["output"], output);
    }

    #[tokio::test]
    async fn oversized_terminal_events_still_close_the_stream() {
        for event in [
            TaskEvent::TaskFailed {
                error: "x".repeat(MAX_DASHBOARD_EVENT_BYTES + 1),
            },
            TaskEvent::TaskCompleted {
                output: json!("x".repeat(MAX_DASHBOARD_EVENT_BYTES + 1)),
                elapsed_secs: 1.0,
                usage: json!({}),
            },
        ] {
            let log = limited_log(1);
            log.emit("root", event).await;
            let stream = log.subscribe(None, Some("root".into()));
            tokio::pin!(stream);
            assert!(stream.next().await.unwrap().event.is_terminal());
            assert!(stream.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn disk_transcript_keeps_evicted_events_without_reloading_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("run-1.json");
        let log = limited_log(1);
        log.enable_transcript(&path, "run-1").await.unwrap();
        for iteration in 0..5 {
            log.emit("root", TaskEvent::IterationStarted { iteration })
                .await;
        }
        assert_eq!(log.snapshot_since(None).await.len(), 1);
        log.finish_transcript().await.unwrap();
        let transcript: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(transcript["job_id"], "run-1");
        assert_eq!(transcript["final"], true);
        let sequence: Vec<_> = transcript["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(sequence, [0, 1, 2, 3, 4]);
        assert_eq!(log.snapshot_since(None).await[0].seq, 4);
        assert!(log.enable_transcript(&path, "late").await.is_err());
        // A rejected late enable must not truncate an existing archive.
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap(),
            transcript
        );
    }

    #[tokio::test]
    async fn eviction_retains_only_the_newest_events_and_cursors_stay_valid() {
        for limit in [1, 3] {
            let log = limited_log(limit);
            for iteration in 0..10 {
                log.emit("root", TaskEvent::IterationStarted { iteration })
                    .await;
            }
            let snapshot = log.snapshot_since(None).await;
            assert_eq!(snapshot.len(), limit);
            assert_eq!(snapshot[0].seq, 10 - limit as u64);
            assert_eq!(snapshot.last().unwrap().seq, 9);
            assert_eq!(log.snapshot_since(Some(0)).await.len(), limit);
            assert!(log.snapshot_since(Some(u64::MAX)).await.is_empty());
            assert_eq!(log.history.read().await.events.len(), limit);
        }
    }

    #[tokio::test]
    async fn concurrent_emitters_keep_sequence_order_after_eviction() {
        let log = limited_log(8);
        let mut tasks = tokio::task::JoinSet::new();
        for iteration in 0..64 {
            let log = log.clone();
            tasks.spawn(async move {
                log.emit("root", TaskEvent::IterationStarted { iteration })
                    .await;
            });
        }
        while let Some(task) = tasks.join_next().await {
            task.unwrap();
        }
        let sequence: Vec<_> = log
            .snapshot_since(None)
            .await
            .iter()
            .map(|event| event.seq)
            .collect();
        assert_eq!(sequence, (56..64).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn slow_subscriber_skips_evicted_events_and_keeps_streaming() {
        let log = limited_log(2);
        log.emit("root", TaskEvent::IterationStarted { iteration: 0 })
            .await;
        let stream = log.clone().subscribe(None, None);
        tokio::pin!(stream);
        assert_eq!(stream.next().await.unwrap().seq, 0);
        for iteration in 1..6 {
            log.emit("root", TaskEvent::IterationStarted { iteration })
                .await;
        }
        let next = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.seq, 4);
        assert_eq!(stream.next().await.unwrap().seq, 5);
    }

    #[tokio::test]
    async fn checkpoint_child_status_survives_evicted_payloads() {
        let log = limited_log(1);
        for child in ["completed", "failed", "cancelled"] {
            log.emit(
                "root",
                TaskEvent::SubagentSpawned {
                    child_task_id: child.into(),
                    agent: "agent".into(),
                    title: "title".into(),
                    query: "assignment".into(),
                },
            )
            .await;
        }
        log.emit(
            "completed",
            TaskEvent::TaskCompleted {
                output: json!("large output"),
                elapsed_secs: 1.0,
                usage: json!({}),
            },
        )
        .await;
        log.emit(
            "failed",
            TaskEvent::TaskFailed {
                error: "failure".into(),
            },
        )
        .await;
        log.emit(
            "root",
            TaskEvent::SubagentCompleted {
                child_task_id: "cancelled".into(),
                output: "cancelled output".into(),
                usage: json!({}),
            },
        )
        .await;
        log.emit("root", TaskEvent::IterationStarted { iteration: 1 })
            .await;
        let ledger = log.child_ledger("root").await;
        assert_eq!(ledger["completed"]["status"], "completed");
        assert_eq!(ledger["failed"]["status"], "failed");
        assert_eq!(ledger["failed"]["error"], "failure");
        assert_eq!(ledger["cancelled"]["status"], "returned_or_cancelled");
        assert_eq!(ledger["completed"]["query"], "assignment");
        assert!(!ledger.to_string().contains("large output"));
        assert_eq!(log.child_ledger("other").await, json!({}));
        assert_eq!(log.snapshot_since(None).await.len(), 1);
    }

    #[tokio::test]
    async fn stream_closes_when_its_terminal_event_has_been_evicted() {
        let log = limited_log(1);
        log.emit(
            "root",
            TaskEvent::TaskFailed {
                error: "done".into(),
            },
        )
        .await;
        log.emit("other", TaskEvent::IterationStarted { iteration: 1 })
            .await;
        let stream = log.subscribe(None, Some("root".into()));
        tokio::pin!(stream);
        assert_eq!(stream.next().await.unwrap().seq, 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn emits_in_order_with_monotonic_seq() {
        let log = limited_log(DEFAULT_MAX_EVENTS.get());
        log.emit("a", TaskEvent::IterationStarted { iteration: 1 })
            .await;
        log.emit("a", TaskEvent::IterationStarted { iteration: 2 })
            .await;
        log.emit("a", TaskEvent::IterationStarted { iteration: 3 })
            .await;

        let snap = log.snapshot_since(None).await;
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].seq, 0);
        assert_eq!(snap[1].seq, 1);
        assert_eq!(snap[2].seq, 2);
    }

    #[tokio::test]
    async fn snapshot_since_filters() {
        let log = limited_log(DEFAULT_MAX_EVENTS.get());
        for i in 0..5 {
            log.emit("a", TaskEvent::IterationStarted { iteration: i })
                .await;
        }
        let snap = log.snapshot_since(Some(2)).await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].seq, 3);
        assert_eq!(snap[1].seq, 4);
    }

    #[tokio::test]
    async fn subscribe_replays_history_then_streams_live() {
        let log = limited_log(DEFAULT_MAX_EVENTS.get());
        log.emit("a", TaskEvent::IterationStarted { iteration: 1 })
            .await;
        log.emit("a", TaskEvent::IterationStarted { iteration: 2 })
            .await;

        let sub = log.clone().subscribe(None, None);
        tokio::pin!(sub);

        // Replay
        let e1 = sub.next().await.expect("e1");
        assert_eq!(e1.seq, 0);
        let e2 = sub.next().await.expect("e2");
        assert_eq!(e2.seq, 1);

        // Live: emit while subscribed
        let log2 = log.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            log2.emit("a", TaskEvent::IterationStarted { iteration: 3 })
                .await;
        });
        let e3 = sub.next().await.expect("e3");
        assert_eq!(e3.seq, 2);
    }

    #[tokio::test]
    async fn subscribe_terminates_on_target_terminal_event() {
        let log = limited_log(DEFAULT_MAX_EVENTS.get());
        log.emit(
            "root",
            TaskEvent::TaskFailed {
                error: "boom".into(),
            },
        )
        .await;

        let sub = log.clone().subscribe(None, Some("root".to_string()));
        tokio::pin!(sub);
        let _e = sub.next().await.expect("terminal");
        // Stream should now be closed.
        assert!(sub.next().await.is_none());
    }

    #[tokio::test]
    async fn subscribe_does_not_terminate_on_subagent_completion() {
        let log = limited_log(DEFAULT_MAX_EVENTS.get());
        log.emit(
            "root",
            TaskEvent::TaskStarted {
                parent_task_id: None,
                agent: "parent".into(),
                query: "q".into(),
                title: "t".into(),
                iter_cap: 10,
            },
        )
        .await;
        // A subagent finishes — same shared log, different task_id.
        log.emit(
            "child",
            TaskEvent::TaskCompleted {
                output: serde_json::Value::Null,
                elapsed_secs: 0.1,
                usage: serde_json::Value::Object(Default::default()),
            },
        )
        .await;
        // Parent emits a follow-up event afterwards.
        log.emit("root", TaskEvent::IterationStarted { iteration: 2 })
            .await;

        let sub = log.clone().subscribe(None, Some("root".to_string()));
        tokio::pin!(sub);

        let e0 = sub.next().await.expect("TaskStarted");
        assert_eq!(e0.task_id, "root");
        let e1 = sub.next().await.expect("subagent TaskCompleted");
        assert_eq!(e1.task_id, "child");
        // CRITICAL: we must still receive the parent's next event.
        let e2 = sub.next().await.expect("parent IterationStarted");
        assert_eq!(e2.task_id, "root");

        // Now if root itself fails, the stream closes.
        let log2 = log.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            log2.emit(
                "root",
                TaskEvent::TaskFailed {
                    error: "done".into(),
                },
            )
            .await;
        });
        let e3 = sub.next().await.expect("root TaskFailed");
        assert_eq!(e3.task_id, "root");
        assert!(sub.next().await.is_none(), "stream closes on root terminal");
    }
}

use std::cell::Cell;
use std::fmt;
use std::io::IsTerminal;
use tokio::task_local;
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, format::Writer};
use tracing_subscriber::registry::LookupSpan;

use flawrence_chat::SharedUsage;

task_local! {
    pub static TASK_CONTEXT: TaskContext;
}

#[derive(Clone)]
pub struct TaskContext {
    pub hierarchy: String,
    pub context_size: Cell<u32>,
    pub iteration_count: Cell<u32>,
    /// Per-task cumulative usage (same Arc the task loop writes to)
    pub task_usage: SharedUsage,
    /// Global usage across all concurrent sessions
    pub global_usage: SharedUsage,
}

impl TaskContext {
    pub fn new(hierarchy: String, task_usage: SharedUsage, global_usage: SharedUsage) -> Self {
        Self {
            hierarchy,
            context_size: Cell::new(0),
            iteration_count: Cell::new(0),
            task_usage,
            global_usage,
        }
    }

    pub fn update_context_size(size: u32) {
        let _ = TASK_CONTEXT.try_with(|ctx| {
            ctx.context_size.set(size);
        });
    }

    pub fn update_iteration(n: u32) {
        let _ = TASK_CONTEXT.try_with(|ctx| {
            ctx.iteration_count.set(n);
        });
    }

    fn get() -> Option<Self> {
        TASK_CONTEXT.try_with(|ctx| ctx.clone()).ok()
    }
}

/// ANSI color codes for hierarchy levels
const HIERARCHY_COLORS: &[&str] = &[
    "\x1b[36m", // Cyan
    "\x1b[33m", // Yellow
    "\x1b[35m", // Magenta
    "\x1b[32m", // Green
    "\x1b[34m", // Blue
    "\x1b[31m", // Red
];

pub struct CustomFormat {
    ansi: bool,
}

impl CustomFormat {
    /// Construct a formatter with explicit ANSI control.
    pub fn new(ansi: bool) -> Self {
        Self { ansi }
    }

    /// Construct a formatter that autodetects whether to emit ANSI escapes:
    ///   - disabled if `NO_COLOR` is set to a non-empty value
    ///     (per <https://no-color.org>);
    ///   - disabled if stderr is not a terminal (e.g. running under a
    ///     container runtime, redirected to a file, captured by CI);
    ///   - enabled otherwise.
    pub fn auto() -> Self {
        Self::new(should_use_ansi())
    }
}

fn should_use_ansi() -> bool {
    if let Ok(v) = std::env::var("NO_COLOR")
        && !v.is_empty()
    {
        return false;
    }
    std::io::stderr().is_terminal()
}

impl<S, N> FormatEvent<S, N> for CustomFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        // Color codes — empty when ANSI is disabled so the existing write!s
        // collapse to plain text without further branching.
        let (dim, cyan, reset) = if self.ansi {
            ("\x1b[2m", "\x1b[36m", "\x1b[0m")
        } else {
            ("", "", "")
        };

        // Write timestamp (dim gray)
        let now = chrono::Utc::now();
        write!(
            writer,
            "{}{}{} ",
            dim,
            now.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            reset
        )?;

        // Get task context from task-local storage
        if let Some(task_ctx) = TaskContext::get() {
            let context_size = task_ctx.context_size.get();

            let iteration_count = task_ctx.iteration_count.get();

            let task_cost = task_ctx
                .task_usage
                .lock()
                .map(|u| u.total_cost())
                .unwrap_or(0.0)
                .max(0.0);
            let global_cost = task_ctx
                .global_usage
                .lock()
                .map(|u| u.total_cost())
                .unwrap_or(0.0)
                .max(0.0);

            write!(
                writer,
                "{}[C:{}K I:{} ${:.2} | ${:.2}]{} ",
                cyan,
                context_size / 1000,
                iteration_count,
                task_cost,
                global_cost,
                reset
            )?;

            if !task_ctx.hierarchy.is_empty() {
                let levels: Vec<&str> = task_ctx.hierarchy.split(" > ").collect();
                for (i, level) in levels.iter().enumerate() {
                    if i > 0 {
                        write!(writer, " > ")?;
                    }
                    let color = if self.ansi {
                        HIERARCHY_COLORS[i % HIERARCHY_COLORS.len()]
                    } else {
                        ""
                    };
                    write!(writer, "{}{}{}", color, level, reset)?;
                }
                write!(writer, " - ")?;
            }
        }

        // Write the message and remaining fields
        ctx.format_fields(writer.by_ref(), event)?;

        writeln!(writer)
    }
}

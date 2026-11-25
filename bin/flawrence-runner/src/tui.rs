//! Full-screen AFL-style dashboard for `--tui` mode.
//!
//! The runner and task pool write progress to stdout and stderr. In TUI mode
//! the `live` submodule redirects those file descriptors into a pipe, a reader
//! thread pushes each line into [`DashboardState`], and a render thread draws a
//! fixed header plus a scrolling log pane with ratatui.
//!
//! This module holds the parts that need no terminal: the shared state, the
//! parser that pulls live metrics out of the captured log prefix, formatting
//! helpers, and the pure `render` function. They are unit tested directly.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use flawrence_chat::UsageByModel;
use ratatui::layout::Margin;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

/// Maximum number of captured log lines kept for the scrolling pane.
const LOG_CAPACITY: usize = 4000;

/// Orange used for cost figures (per-MTok rates and running spend) in the
/// header, so money stands apart from yellow warnings and green status.
const COST_COLOR: Color = Color::Rgb(0xff, 0xa5, 0x00);

/// Static facts about the run, shown in the header. Set once at startup.
#[derive(Clone, Debug)]
pub struct RunInfo {
    pub agent: String,
    pub model: String,
    pub directory: String,
    pub max_iterations: u32,
    pub budget_cost: Option<f64>,
    /// Per-million-token rates (input, output, cache read, cache write) when a
    /// price is known for the model. `None` when the model is unpriced.
    pub rates: Option<Rates>,
    /// True when the cost is a models.dev estimate rather than a manual override.
    pub cost_estimated: bool,
    /// Set when pricing was resolved through a best-effort provider alias.
    pub alias: Option<String>,
}

/// Per-million-token pricing shown in the header overview.
#[derive(Clone, Copy, Debug)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// Live values parsed out of the captured log stream.
#[derive(Default, Debug)]
pub struct LiveState {
    pub iteration: u32,
    pub context_k: u32,
    pub hierarchy: String,
    pub status: String,
    pub logs: VecDeque<String>,
}

/// The full dashboard state shared between the reader and render threads.
pub struct DashboardState {
    pub info: RunInfo,
    pub started: Instant,
    pub live: LiveState,
    /// Set to true once the run finishes, so the render loop can show a footer.
    pub finished: bool,
    /// When true, the log pane sticks to the newest lines. Any manual scroll
    /// turns this off until the user scrolls back to the bottom.
    pub follow: bool,
    /// Top wrapped-line offset of the log pane when not following.
    pub scroll_top: u16,
    /// Log pane viewport height (rows), written by the renderer each frame so
    /// that key handling can page and clamp correctly.
    pub viewport_height: u16,
    /// Total wrapped height of the log content, written by the renderer.
    pub content_height: u16,
}

impl DashboardState {
    pub fn new(info: RunInfo) -> Self {
        Self {
            info,
            started: Instant::now(),
            live: LiveState::default(),
            finished: false,
            follow: true,
            scroll_top: 0,
            viewport_height: 0,
            content_height: 0,
        }
    }

    /// Maximum top offset that still shows content in the viewport.
    fn max_scroll(&self) -> u16 {
        self.content_height.saturating_sub(self.viewport_height)
    }

    /// Scroll by `delta` wrapped lines (negative is up, toward older output).
    /// Reaching the bottom re-enables follow mode.
    pub fn scroll_by(&mut self, delta: i32) {
        let max = self.max_scroll();
        let current = if self.follow { max } else { self.scroll_top };
        let next = (i32::from(current) + delta).clamp(0, i32::from(max)) as u16;
        self.scroll_top = next;
        self.follow = next >= max;
    }

    /// Scroll by a full page in the given direction (`-1` up, `1` down).
    pub fn scroll_page(&mut self, direction: i32) {
        self.scroll_by(direction * i32::from(self.viewport_height.max(1)));
    }

    pub fn scroll_to_top(&mut self) {
        self.follow = false;
        self.scroll_top = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.follow = true;
    }

    /// Ingest one captured output line: append it to the log ring buffer and,
    /// if it carries the task-context prefix, refresh the live metrics.
    pub fn ingest_line(&mut self, line: &str) {
        if let Some(metrics) = parse_log_metrics(line) {
            self.live.iteration = metrics.iteration.unwrap_or(self.live.iteration);
            self.live.context_k = metrics.context_k.unwrap_or(self.live.context_k);
            if let Some(hierarchy) = metrics.hierarchy {
                self.live.hierarchy = hierarchy;
            }
            if let Some(message) = metrics.message {
                self.live.status = message;
            }
        }
        if self.live.logs.len() >= LOG_CAPACITY {
            self.live.logs.pop_front();
        }
        self.live.logs.push_back(line.to_string());
    }
}

/// Live metrics extracted from a single formatted log line.
#[derive(Default, Debug, PartialEq)]
pub struct ParsedMetrics {
    pub context_k: Option<u32>,
    pub iteration: Option<u32>,
    pub hierarchy: Option<String>,
    pub message: Option<String>,
}

/// Parse the `[C:<n>K I:<n> $<x> | $<y>] <hierarchy> - <message>` prefix that
/// `CustomFormat` emits. Returns `None` when the line has no such prefix (for
/// example plain startup lines), in which case only the log text is kept.
pub fn parse_log_metrics(line: &str) -> Option<ParsedMetrics> {
    let open = line.find("[C:")?;
    let close_rel = line[open..].find(']')?;
    let close = open + close_rel;
    let inner = &line[open + 1..close]; // e.g. "C:31K I:12 $0.00 | $1.23"

    let mut metrics = ParsedMetrics::default();
    for token in inner.split_whitespace() {
        if let Some(rest) = token.strip_prefix("C:") {
            metrics.context_k = rest.trim_end_matches('K').parse().ok();
        } else if let Some(rest) = token.strip_prefix("I:") {
            metrics.iteration = rest.parse().ok();
        }
    }

    // After the closing bracket comes "<hierarchy> - <message>". Split on the
    // first " - " so the hierarchy stays intact even if the message contains it.
    let tail = line[close + 1..].trim();
    if !tail.is_empty() {
        if let Some((hierarchy, message)) = tail.split_once(" - ") {
            metrics.hierarchy = Some(hierarchy.trim().to_string());
            metrics.message = Some(message.trim().to_string());
        } else {
            metrics.hierarchy = Some(tail.to_string());
        }
    }

    Some(metrics)
}

/// Summed token counts and cost across all models, taken at render time.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost: f64,
}

/// Sum a [`UsageByModel`] into a flat snapshot for display.
pub fn snapshot_usage(usage: &UsageByModel) -> UsageSnapshot {
    let mut snapshot = UsageSnapshot::default();
    for model_usage in usage.by_model.values() {
        snapshot.input += model_usage.input;
        snapshot.cache_read += model_usage.cache_read_input;
        snapshot.cache_write += model_usage.cache_write_input;
        snapshot.output += model_usage.output;
        snapshot.cost += model_usage.cost_usd;
    }
    snapshot.cost = snapshot.cost.max(0.0);
    snapshot
}

/// Format a token count compactly, for example `1.2M` or `340k` or `812`.
pub fn fmt_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.0}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

/// Format an elapsed duration as `MM:SS` or `HH:MM:SS`.
pub fn fmt_duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Draw the dashboard: a bordered header block over a scrolling log pane.
pub fn render(frame: &mut Frame, state: &mut DashboardState, usage: &UsageSnapshot) {
    let elapsed = state.started.elapsed();
    // 2 borders + 8 fixed rows, plus 1 more when the alias warning is shown.
    let header_height = if state.info.alias.is_some() { 11 } else { 10 };
    let layout = Layout::vertical([Constraint::Length(header_height), Constraint::Min(1)])
        .split(frame.area());

    frame.render_widget(header(state, usage, elapsed), layout[0]);
    render_logs(frame, state, layout[1]);
}

fn header(state: &DashboardState, usage: &UsageSnapshot, elapsed: Duration) -> Paragraph<'static> {
    let info = &state.info;

    let iter_line = format!("{} / {}", state.live.iteration, info.max_iterations);
    let tokens_line = format!(
        "{} in / {} out  (cache {} r / {} w)",
        fmt_tokens(usage.input),
        fmt_tokens(usage.output),
        fmt_tokens(usage.cache_read),
        fmt_tokens(usage.cache_write),
    );
    let cost_line = match info.budget_cost {
        Some(cap) => format!("${:.2} / ${:.2} cap", usage.cost, cap),
        None => format!("${:.2}", usage.cost),
    };
    let cost_kind = if info.cost_estimated {
        "estimated"
    } else {
        "configured"
    };
    let pricing_line = match info.rates {
        Some(r) => format!(
            "in ${:.2}  out ${:.2}  cache-r ${:.2}  cache-w ${:.2}  /MTok ({cost_kind})",
            r.input, r.output, r.cache_read, r.cache_write,
        ),
        None => "not priced (running cost shown as $0.00)".to_string(),
    };

    let mut lines = vec![
        kv("Agent", &info.agent),
        kv("Model", &info.model),
        kv("Dir", &info.directory),
        Line::from(vec![
            label("Iter"),
            Span::raw(format!("{iter_line:<16}")),
            label("Ctx"),
            Span::raw(format!("{}K", state.live.context_k)),
        ]),
        Line::from(vec![label("Tokens"), Span::raw(tokens_line)]),
        Line::from(vec![
            label("Pricing"),
            Span::styled(pricing_line, Style::default().fg(COST_COLOR)),
        ]),
        Line::from(vec![
            label("Cost"),
            Span::styled(cost_line, Style::default().fg(COST_COLOR)),
            Span::raw(format!("  ({cost_kind})   ")),
            label("Elapsed"),
            Span::raw(fmt_duration(elapsed)),
        ]),
    ];

    let status = if state.finished {
        "done".to_string()
    } else if state.live.status.is_empty() {
        "starting...".to_string()
    } else {
        truncate(&state.live.status, 200)
    };
    let where_line = if state.live.hierarchy.is_empty() {
        status
    } else {
        format!("{}  {}", state.live.hierarchy, status)
    };
    lines.push(Line::from(vec![
        label("Status"),
        Span::styled(
            truncate(&where_line, 200),
            Style::default().fg(Color::Green),
        ),
    ]));

    if let Some(alias) = &info.alias {
        lines.push(Line::from(Span::styled(
            format!("cost via best-effort alias '{alias}', may differ"),
            Style::default().fg(Color::Yellow),
        )));
    }

    let title = if state.finished {
        " Flawrence run  (finished, press q to exit) "
    } else {
        " Flawrence run  (press q to quit) "
    };
    Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title))
}

/// Render the log pane with wrapped lines and scroll support. Long messages
/// wrap instead of truncating. The renderer measures the wrapped content and
/// viewport so key handling can page and clamp against real dimensions.
fn render_logs(frame: &mut Frame, state: &mut DashboardState, area: Rect) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2);

    let mode = if state.follow {
        "following"
    } else {
        "scrolled"
    };
    let title = format!(" logs [{mode}]  up/down pgup/pgdn home/end scroll, q quit ");

    // Pre-wrap to the pane width so the wrapped line count is exact. That keeps
    // the scroll offset and the scrollbar thumb in sync with what is drawn.
    let wrapped: Vec<Line<'static>> = state
        .live
        .logs
        .iter()
        .flat_map(|line| wrap_to_width(line, inner_width))
        .map(Line::from)
        .collect();

    let content = wrapped.len() as u16;
    state.content_height = content;
    state.viewport_height = inner_height;
    let max_top = content.saturating_sub(inner_height);
    let top = if state.follow {
        max_top
    } else {
        state.scroll_top.min(max_top)
    };
    state.scroll_top = top;

    let paragraph =
        Paragraph::new(wrapped).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(paragraph.scroll((top, 0)), area);

    // Scrollbar on the right border, inset so it sits between the corners.
    let mut scrollbar_state = ScrollbarState::new(content as usize)
        .viewport_content_length(inner_height as usize)
        .position(top as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{2191}"))
            .end_symbol(Some("\u{2193}")),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut scrollbar_state,
    );
}

fn kv(key: &'static str, value: &str) -> Line<'static> {
    Line::from(vec![label(key), Span::raw(value.to_string())])
}

fn label(text: &'static str) -> Span<'static> {
    Span::styled(
        format!("{text:<9}"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
}

fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// Hard-wrap a string into rows of at most `width` characters. Empty input
/// still yields one (blank) row so the line is represented in the pane.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![String::new()];
    }
    text.chars()
        .collect::<Vec<char>>()
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Remove ANSI escape sequences (CSI `ESC [ ... letter`) from captured output.
/// The pool prints hardcoded color codes, and we render styling ourselves.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Consume an escape sequence. CSI is `ESC [ ... final`, where final
            // is in the range @-~. Other escapes: drop the next char.
            if chars.peek() == Some(&'[') {
                chars.next();
                for e in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&e) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Live full-screen dashboard: captures stdout/stderr at the fd level, renders
/// a header and log pane to `/dev/tty`, and restores everything on teardown.
///
/// Unix only. The redirect relies on `dup2`, and drawing to `/dev/tty` keeps
/// the render surface separate from the captured application output.
#[cfg(unix)]
pub mod live {
    use super::{DashboardState, RunInfo, render, snapshot_usage, strip_ansi};
    use flawrence_chat::SharedUsage;
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::crossterm::{cursor, execute, terminal};
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::FromRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    const FRAME_INTERVAL: Duration = Duration::from_millis(90);

    /// Escape sequence to leave the alternate screen and show the cursor.
    const RESTORE_SEQ: &[u8] = b"\x1b[?1049l\x1b[?25h";

    pub struct Dashboard {
        state: Arc<Mutex<DashboardState>>,
        stop: Arc<AtomicBool>,
        render_thread: Option<JoinHandle<()>>,
        reader_thread: Option<JoinHandle<()>>,
        saved_out: i32,
        saved_err: i32,
    }

    impl Dashboard {
        /// Begin capturing output and rendering. Returns `Ok(None)` when the
        /// environment cannot host a TUI (for example `/dev/tty` is unavailable),
        /// so the caller can fall back to plain JSON mode.
        pub fn start(info: RunInfo, usage: SharedUsage) -> std::io::Result<Option<Self>> {
            // The render surface must be a real terminal.
            let Ok(tty) = File::options().read(true).write(true).open("/dev/tty") else {
                return Ok(None);
            };

            // 1. Redirect stdout (1) and stderr (2) into a pipe, saving originals.
            let (read_fd, saved_out, saved_err) = unsafe { redirect_std_to_pipe()? };

            let state = Arc::new(Mutex::new(DashboardState::new(info)));
            let stop = Arc::new(AtomicBool::new(false));

            install_panic_hook(saved_out, saved_err);

            // 2. Reader thread: pipe -> state.
            let reader_thread = {
                let state = state.clone();
                let stop = stop.clone();
                std::thread::spawn(move || reader_loop(read_fd, &state, &stop))
            };

            // 3. Enter the alternate screen, enable raw mode for key input, and
            // spawn the render/event thread.
            let mut tty = tty;
            let _ = execute!(tty, terminal::EnterAlternateScreen, cursor::Hide);
            let _ = terminal::enable_raw_mode();
            let render_thread = {
                let state = state.clone();
                let usage = usage.clone();
                let stop = stop.clone();
                std::thread::spawn(move || render_loop(tty, &state, &usage, &stop))
            };

            Ok(Some(Self {
                state,
                stop,
                render_thread: Some(render_thread),
                reader_thread: Some(reader_thread),
                saved_out,
                saved_err,
            }))
        }

        /// Mark the run finished so the header shows a completion hint.
        pub fn set_finished(&self) {
            if let Ok(mut state) = self.state.lock() {
                state.finished = true;
            }
        }

        /// Tear down: stop threads, restore the terminal and file descriptors,
        /// then replay the captured output onto the restored screen so nothing
        /// (including the agent's final answer) is lost.
        pub fn stop(mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.render_thread.take() {
                let _ = handle.join();
            }
            // Restore the real stdout/stderr before the reader hits EOF.
            unsafe {
                libc::dup2(self.saved_out, 1);
                libc::dup2(self.saved_err, 2);
            }
            if let Some(handle) = self.reader_thread.take() {
                let _ = handle.join();
            }
            unsafe {
                libc::close(self.saved_out);
                libc::close(self.saved_err);
            }
            let _ = std::panic::take_hook();

            // Replay the captured log onto the now-restored screen.
            if let Ok(state) = self.state.lock() {
                let mut out = std::io::stdout().lock();
                for line in &state.live.logs {
                    let _ = writeln!(out, "{line}");
                }
                let _ = out.flush();
            }
        }
    }

    /// Redirect fds 1 and 2 into a fresh pipe. Returns `(read_fd, saved_stdout,
    /// saved_stderr)`. The write end lives only as fds 1 and 2 afterwards, so it
    /// closes automatically once those are restored.
    unsafe fn redirect_std_to_pipe() -> std::io::Result<(i32, i32, i32)> {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        let saved_out = unsafe { libc::dup(1) };
        let saved_err = unsafe { libc::dup(2) };
        if saved_out < 0 || saved_err < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::dup2(write_fd, 1) } < 0 || unsafe { libc::dup2(write_fd, 2) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe { libc::close(write_fd) };
        Ok((read_fd, saved_out, saved_err))
    }

    fn reader_loop(read_fd: i32, state: &Mutex<DashboardState>, stop: &AtomicBool) {
        use std::io::Read;
        let mut file = unsafe { File::from_raw_fd(read_fd) };
        let mut buf = [0u8; 8192];
        let mut line: Vec<u8> = Vec::new();
        loop {
            let mut pfd = libc::pollfd {
                fd: read_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
            if ready > 0 {
                match file.read(&mut buf) {
                    Ok(0) => break, // EOF: all write ends closed.
                    Ok(n) => {
                        for &byte in &buf[..n] {
                            if byte == b'\n' {
                                flush_line(state, &line);
                                line.clear();
                            } else {
                                line.push(byte);
                            }
                        }
                    }
                    Err(_) => break,
                }
            } else if ready == 0 && stop.load(Ordering::Relaxed) {
                break;
            }
        }
        if !line.is_empty() {
            flush_line(state, &line);
        }
    }

    fn flush_line(state: &Mutex<DashboardState>, raw: &[u8]) {
        let text = strip_ansi(&String::from_utf8_lossy(raw));
        let text = text.trim_end();
        if let Ok(mut state) = state.lock() {
            state.ingest_line(text);
        }
    }

    /// Canonical ratatui loop on a dedicated thread: draw the current state,
    /// then wait for a key with a timeout that doubles as the refresh interval
    /// so live metrics keep updating even when no key is pressed. The main
    /// thread meanwhile drives the async run.
    fn render_loop(
        tty: File,
        state: &Mutex<DashboardState>,
        usage: &SharedUsage,
        stop: &AtomicBool,
    ) {
        let backend = CrosstermBackend::new(tty);
        let Ok(mut terminal) = Terminal::new(backend) else {
            return;
        };

        let draw = |terminal: &mut Terminal<CrosstermBackend<File>>| {
            let snapshot = usage.lock().map(|u| snapshot_usage(&u)).unwrap_or_default();
            if let Ok(mut state) = state.lock() {
                let _ = terminal.draw(|frame| render(frame, &mut state, &snapshot));
            }
        };

        while !stop.load(Ordering::Relaxed) {
            draw(&mut terminal);
            if matches!(event::poll(FRAME_INTERVAL), Ok(true))
                && let Ok(Event::Key(key)) = event::read()
                && key.kind != KeyEventKind::Release
            {
                handle_key(
                    state,
                    key.code,
                    key.modifiers.contains(KeyModifiers::CONTROL),
                );
            }
        }

        // Final frame, then restore the terminal.
        draw(&mut terminal);
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            terminal.backend_mut(),
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal.backend_mut().flush();
    }

    /// Apply a key: scroll the log pane, or quit the run.
    fn handle_key(state: &Mutex<DashboardState>, code: KeyCode, ctrl: bool) {
        let scroll = |f: &dyn Fn(&mut DashboardState)| {
            if let Ok(mut s) = state.lock() {
                f(&mut s);
            }
        };
        match code {
            KeyCode::Char('c') if ctrl => request_quit(state),
            KeyCode::Char('q') | KeyCode::Esc => request_quit(state),
            KeyCode::Up | KeyCode::Char('k') => scroll(&|s| s.scroll_by(-1)),
            KeyCode::Down | KeyCode::Char('j') => scroll(&|s| s.scroll_by(1)),
            KeyCode::PageUp => scroll(&|s| s.scroll_page(-1)),
            KeyCode::PageDown | KeyCode::Char(' ') => scroll(&|s| s.scroll_page(1)),
            KeyCode::Home | KeyCode::Char('g') => scroll(&|s| s.scroll_to_top()),
            KeyCode::End | KeyCode::Char('G') => scroll(&|s| s.scroll_to_bottom()),
            _ => {}
        }
    }

    /// Translate a quit key into a real SIGINT so the pool's existing Ctrl-C
    /// handler shuts the run down gracefully. Raw mode swallows terminal SIGINT,
    /// so we raise it ourselves. Once the run has finished, teardown is imminent,
    /// so we do nothing rather than signal a process with no handler left.
    fn request_quit(state: &Mutex<DashboardState>) {
        let finished = state.lock().map(|s| s.finished).unwrap_or(true);
        if !finished {
            unsafe { libc::raise(libc::SIGINT) };
        }
    }

    fn install_panic_hook(saved_out: i32, saved_err: i32) {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Best-effort terminal restore so a panic does not wreck the shell.
            let _ = terminal::disable_raw_mode();
            if let Ok(mut tty) = File::options().write(true).open("/dev/tty") {
                let _ = tty.write_all(RESTORE_SEQ);
                let _ = tty.flush();
            }
            unsafe {
                libc::dup2(saved_out, 1);
                libc::dup2(saved_err, 2);
            }
            previous(info);
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prefix_metrics_and_message() {
        let line = "2026-08-03T14:17:05.9Z [C:31K I:12 $0.00 | $1.23] run#1 (d18c9851) > title - API call failed, retrying";
        let m = parse_log_metrics(line).unwrap();
        assert_eq!(m.context_k, Some(31));
        assert_eq!(m.iteration, Some(12));
        assert_eq!(m.hierarchy.as_deref(), Some("run#1 (d18c9851) > title"));
        assert_eq!(m.message.as_deref(), Some("API call failed, retrying"));
    }

    #[test]
    fn message_keeps_embedded_separators() {
        let line = "[C:0K I:1 $0.00 | $0.00] run#1 - reading a - b - c";
        let m = parse_log_metrics(line).unwrap();
        assert_eq!(m.hierarchy.as_deref(), Some("run#1"));
        assert_eq!(m.message.as_deref(), Some("reading a - b - c"));
    }

    #[test]
    fn lines_without_prefix_are_not_parsed() {
        assert!(parse_log_metrics("Loading agents...").is_none());
        assert!(parse_log_metrics("Indexed 42 files").is_none());
    }

    #[test]
    fn ingest_updates_live_and_keeps_last_known_on_bare_lines() {
        let info = RunInfo {
            agent: "a".into(),
            model: "m".into(),
            directory: "/tmp".into(),
            max_iterations: 500,
            budget_cost: None,
            rates: None,
            cost_estimated: true,
            alias: None,
        };
        let mut state = DashboardState::new(info);
        state.ingest_line("[C:5K I:3 $0.00 | $0.10] run#1 - doing work");
        assert_eq!(state.live.iteration, 3);
        assert_eq!(state.live.context_k, 5);
        state.ingest_line("a bare line with no prefix");
        assert_eq!(state.live.iteration, 3); // unchanged
        assert_eq!(state.live.logs.len(), 2);
    }

    #[test]
    fn token_formatting_scales() {
        assert_eq!(fmt_tokens(812), "812");
        assert_eq!(fmt_tokens(340_000), "340k");
        assert_eq!(fmt_tokens(1_200_000), "1.2M");
    }

    #[test]
    fn duration_formatting_switches_to_hours() {
        assert_eq!(fmt_duration(Duration::from_secs(74)), "01:14");
        assert_eq!(
            fmt_duration(Duration::from_secs(3 * 3600 + 5 * 60 + 9)),
            "03:05:09"
        );
    }

    #[test]
    fn usage_snapshot_sums_across_models() {
        let mut usage = UsageByModel::default();
        usage.add("m1", 100, 0, 10, 50);
        usage.add("m2", 200, 0, 20, 60);
        let snap = snapshot_usage(&usage);
        assert_eq!(snap.input, 300);
        assert_eq!(snap.output, 110);
        assert_eq!(snap.cache_read, 30);
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell\u{2026}");
        assert_eq!(truncate("x", 0), "");
    }

    #[test]
    fn renders_full_frame_to_test_backend() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let info = RunInfo {
            agent: "security-review".into(),
            model: "llmbase/z-ai/glm-5.2".into(),
            directory: "/tmp/target".into(),
            max_iterations: 500,
            budget_cost: Some(5.0),
            rates: Some(Rates {
                input: 0.40,
                output: 2.00,
                cache_read: 0.06,
                cache_write: 0.00,
            }),
            cost_estimated: false,
            alias: Some("minimax-cn/MiniMax-M3".into()),
        };
        let mut state = DashboardState::new(info);
        state.ingest_line(
            "[C:31K I:142 $0.40 | $0.87] run#1 (a3f) > threat-model - running tool: grep_files",
        );
        state.ingest_line(
            "[C:31K I:142 $0.40 | $0.87] run#1 (a3f) > threat-model - read_file README.md",
        );
        let usage = UsageSnapshot {
            input: 1_200_000,
            output: 340_000,
            cache_read: 90_000,
            cache_write: 0,
            cost: 0.87,
        };

        let mut terminal = Terminal::new(TestBackend::new(72, 18)).unwrap();
        terminal.draw(|f| render(f, &mut state, &usage)).unwrap();

        let text = buffer_to_string(terminal.backend().buffer());
        // Print so `cargo test -- --nocapture` shows the real layout.
        println!("\n{text}");
        assert!(text.contains("security-review"));
        assert!(text.contains("142 / 500"));
        assert!(text.contains("1.2M in / 340k out"));
        assert!(text.contains("$0.87 / $5.00 cap"));
        assert!(text.contains("grep_files") || text.contains("read_file"));
        assert!(text.contains("best-effort alias"));
    }

    fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }
}

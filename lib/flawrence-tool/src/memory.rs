//! Best-effort Linux RSS watchdogs shared by each direct subagent and its descendants.
//!
//! Only command subprocesses are measured and terminated. Agent chat loops stay
//! alive to receive threshold feedback; no cgroups or elevated privileges are used.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use tokio::sync::Notify;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
const MAX_PROCESSES: usize = 65_536;

tokio::task_local! {
    static AGENT_MEMORY: AgentMemory;
}

struct AgentMemory {
    tree: Arc<MemoryTree>,
    is_root: bool,
}

/// Creates RSS watchdog budgets without requiring writable cgroups.
#[derive(Clone)]
pub struct MemoryLimiter {
    limit_bytes: u64,
    page_size_bytes: u64,
}

impl MemoryLimiter {
    /// Validate a positive per-tree threshold in MiB and Linux process access.
    ///
    /// # Errors
    ///
    /// Fails for invalid thresholds, non-Linux hosts, or unavailable `/proc` and
    /// pidfd support. A threshold is sampled, not a kernel-enforced hard ceiling.
    pub fn new(limit_mb: u64) -> Result<Self> {
        ensure!(
            cfg!(target_os = "linux"),
            "FLAWRENCE_AGENT_MEMORY_MB watchdog requires Linux /proc"
        );
        let limit_bytes = memory_bytes(limit_mb)?;
        // SAFETY: sysconf takes a constant and does not retain pointers.
        let page_size_bytes = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })?;
        ensure!(page_size_bytes > 0, "cannot determine process page size");
        Process::read(std::process::id() as i32, page_size_bytes)
            .context("FLAWRENCE_AGENT_MEMORY_MB watchdog cannot read /proc/self/stat")?;
        #[cfg(target_os = "linux")]
        let _ = process_fd(std::process::id() as i32)
            .context("FLAWRENCE_AGENT_MEMORY_MB watchdog requires Linux pidfd access")?;
        Ok(Self {
            limit_bytes,
            page_size_bytes,
        })
    }

    /// Run an agent within a new or inherited subprocess-memory budget.
    ///
    /// Set `is_root` only for a root agent. Its direct children get independent
    /// budgets; deeper descendants share their parent's budget, including concurrent
    /// commands. Dropping a new scope terminates its remaining tracked subprocesses.
    /// Tokio spawned tasks need explicit scoping because task-local storage is not inherited.
    ///
    /// # Errors
    ///
    /// Returns the agent future's error, or an error if a child has no parent scope.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> anyhow::Result<()> {
    /// use flawrence_tool::memory::MemoryLimiter;
    /// let memory = MemoryLimiter::new(1024)?;
    /// memory.scope(true, async {
    ///     memory.scope(false, async { Ok(()) }).await
    /// }).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn scope<T>(
        &self,
        is_root: bool,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let inherited = if is_root {
            None
        } else {
            AGENT_MEMORY
                .try_with(|parent| (!parent.is_root).then(|| parent.tree.clone()))
                .context("missing parent memory watchdog scope")?
        };
        let (owner, tree) = if let Some(tree) = inherited {
            (None, tree)
        } else {
            let tree = Arc::new(MemoryTree {
                limit_bytes: self.limit_bytes,
                page_size_bytes: self.page_size_bytes,
                state: Mutex::new(TreeState::default()),
                changed: Notify::new(),
            });
            tree.start();
            (Some(TreeOwner(tree.clone())), tree)
        };
        let result = AGENT_MEMORY
            .scope(AgentMemory { tree, is_root }, future)
            .await;
        drop(owner);
        result
    }
}

/// Return the latest tree-wide watchdog notice newer than an agent's cursor.
///
/// Every agent keeps its own cursor, so a child's tool error does not consume
/// its parent's notification. Returns `None` outside a watchdog scope.
///
/// ```
/// let mut cursor = 0;
/// assert!(flawrence_tool::memory::notice_since(&mut cursor).is_none());
/// ```
#[must_use]
pub fn notice_since(cursor: &mut u64) -> Option<String> {
    let tree = MemoryTree::current()?;
    let state = tree.state.lock().unwrap_or_else(|p| p.into_inner());
    if state.generation <= *cursor {
        return None;
    }
    *cursor = state.generation;
    Some(state.message.clone())
}

pub(crate) fn memory_bytes(limit_mb: u64) -> Result<u64> {
    ensure!(
        limit_mb > 0,
        "FLAWRENCE_AGENT_MEMORY_MB must be greater than zero"
    );
    limit_mb
        .checked_mul(1024 * 1024)
        .context("FLAWRENCE_AGENT_MEMORY_MB is too large")
}

#[derive(Clone, Copy, Debug)]
struct Process {
    pid: i32,
    parent: i32,
    group: i32,
    started: u64,
    rss_bytes: u64,
    alive: bool,
}

impl Process {
    fn read(pid: i32, page_size_bytes: u64) -> Result<Self> {
        let raw = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        parse_process(&raw, page_size_bytes)
    }

    fn kill(&self, page_size_bytes: u64) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let fd = match process_fd(self.pid) {
                Ok(fd) => fd,
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            // Open the pidfd before checking identity so PID reuse cannot make
            // us signal an unrelated process after the check.
            if !Self::read(self.pid, page_size_bytes).is_ok_and(|p| p.started == self.started) {
                return Ok(());
            }
            // SAFETY: fd owns a pidfd and the optional siginfo pointer is null.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error.into());
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = page_size_bytes;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn process_fd(pid: i32) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: pidfd_open returns an owned descriptor or -1; no pointers are used.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this is the successful descriptor returned above, owned exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn parse_process(raw: &str, page_size_bytes: u64) -> Result<Process> {
    let (pid, _) = raw.split_once('(').context("invalid process stat")?;
    let (_, tail) = raw.rsplit_once(')').context("invalid process name")?;
    let fields: Vec<_> = tail.split_whitespace().take(22).collect();
    ensure!(fields.len() == 22, "incomplete process stat");
    Ok(Process {
        pid: pid.trim().parse()?,
        parent: fields[1].parse()?,
        group: fields[2].parse()?,
        started: fields[19].parse()?,
        rss_bytes: fields[21].parse::<u64>()?.saturating_mul(page_size_bytes),
        alive: !matches!(fields[0], "Z" | "X" | "x"),
    })
}

fn snapshot(page_size_bytes: u64) -> Result<Vec<Process>> {
    let mut processes = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        ensure!(
            processes.len() < MAX_PROCESSES,
            "watchdog process scan exceeds {MAX_PROCESSES} entries"
        );
        match Process::read(pid, page_size_bytes) {
            Ok(process) => processes.push(process),
            Err(error)
                if error.downcast_ref::<io::Error>().is_some_and(|e| {
                    matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
                }) => {}
            Err(error) => return Err(error.context(format!("cannot sample PID {pid}"))),
        }
    }
    Ok(processes)
}

#[derive(Default)]
struct TreeState {
    roots: HashMap<i32, u64>,
    known: HashMap<i32, u64>,
    generation: u64,
    reported: u64,
    message: String,
    closed: bool,
}

impl TreeState {
    fn owned(&mut self, processes: &[Process]) -> Vec<Process> {
        let by_pid: HashMap<_, _> = processes.iter().map(|p| (p.pid, p)).collect();
        self.roots
            .retain(|pid, start| by_pid.get(pid).is_none_or(|p| p.started == *start));
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for process in processes {
            children
                .entry(process.parent)
                .or_default()
                .push(process.pid);
        }
        let mut pending: Vec<_> = processes
            .iter()
            .filter(|p| {
                p.alive
                    && (self.roots.contains_key(&p.group)
                        || self.known.get(&p.pid) == Some(&p.started))
            })
            .map(|p| p.pid)
            .collect();
        let mut owned = HashSet::new();
        while let Some(pid) = pending.pop() {
            if owned.insert(pid)
                && let Some(descendants) = children.get(&pid)
            {
                pending.extend(
                    descendants
                        .iter()
                        .filter(|child| by_pid.get(child).is_some_and(|p| p.alive)),
                );
            }
        }
        let selected: Vec<_> = processes
            .iter()
            .filter(|p| owned.contains(&p.pid))
            .copied()
            .collect();
        self.known = selected.iter().map(|p| (p.pid, p.started)).collect();
        self.roots
            .retain(|root, _| selected.iter().any(|p| p.group == *root));
        selected
    }
}

pub(crate) struct MemoryTree {
    limit_bytes: u64,
    page_size_bytes: u64,
    state: Mutex<TreeState>,
    changed: Notify,
}

impl MemoryTree {
    fn start(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        // ponytail: scan /proc per active tree; share snapshots if polling at
        // high fan-out becomes expensive. Idle trees perform no process scan.
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(tree) = weak.upgrade() else { break };
                if tree.state.lock().unwrap_or_else(|p| p.into_inner()).closed {
                    break;
                }
                let idle = {
                    let state = tree.state.lock().unwrap_or_else(|p| p.into_inner());
                    state.roots.is_empty() && state.known.is_empty()
                };
                if idle {
                    continue;
                }
                if let Err(error) = tokio::task::spawn_blocking(move || tree.sample()).await {
                    tracing::error!("Memory watchdog worker failed: {error}");
                    if let Some(tree) = weak.upgrade() {
                        tree.fail(format!("memory_watchdog_failed: {error}"));
                    }
                }
            }
        });
    }

    pub(crate) fn current() -> Option<Arc<Self>> {
        AGENT_MEMORY.try_with(|scope| scope.tree.clone()).ok()
    }

    pub(crate) fn checkpoint(&self) -> Result<u64> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(!state.closed, "memory_watchdog_failed: {}", state.message);
        if state.reported != state.generation {
            state.reported = state.generation;
            bail!("{}", state.message);
        }
        Ok(state.generation)
    }

    pub(crate) fn track(&self, pid: u32) -> Result<()> {
        let process = Process::read(i32::try_from(pid)?, self.page_size_bytes)?;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(!state.closed, "memory watchdog tree is closed");
        state.roots.insert(process.pid, process.started);
        Ok(())
    }

    pub(crate) fn check(&self, generation: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.generation != generation {
            state.reported = state.generation;
            bail!("{}", state.message);
        }
        Ok(())
    }

    pub(crate) async fn exceeded(&self, generation: u64) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .generation
                != generation
            {
                return;
            }
            notified.await;
        }
    }

    fn sample(&self) {
        let processes = match snapshot(self.page_size_bytes) {
            Ok(processes) => processes,
            Err(error) => {
                self.fail(format!(
                    "memory_watchdog_failed: cannot sample subprocess memory: {error:#}"
                ));
                return;
            }
        };
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            return;
        }
        let owned = state.owned(&processes);
        let rss_bytes = owned
            .iter()
            .fold(0_u64, |sum, p| sum.saturating_add(p.rss_bytes));
        if rss_bytes > self.limit_bytes {
            state.generation = state.generation.saturating_add(1);
            state.message = format!(
                "memory_limit_exceeded: watchdog observed {} MiB RSS across this subagent tree's \
                 command subprocesses, above FLAWRENCE_AGENT_MEMORY_MB={} MiB. The watchdog killed \
                 the tree's tracked subprocesses; this is not a kernel OOM diagnosis. Agent chat \
                 loops can continue. Reduce input/allocation/batch size before retrying. \
                 This sampled threshold is not a hard RAM ceiling or proof of a target vulnerability.",
                rss_bytes.div_ceil(1024 * 1024),
                self.limit_bytes / (1024 * 1024)
            );
            tracing::warn!(
                rss_bytes,
                limit_bytes = self.limit_bytes,
                "Agent tree memory watchdog triggered"
            );
            self.kill(&owned);
            self.changed.notify_waiters();
        }
    }

    fn fail(&self, message: String) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        state.generation = state.generation.saturating_add(1);
        state.message = message;
        // Monitoring failed: do not knowingly leave registered commands running.
        for (pid, started) in &state.roots {
            self.kill(&[Process {
                pid: *pid,
                started: *started,
                parent: 0,
                group: *pid,
                rss_bytes: 0,
                alive: true,
            }]);
        }
        for (pid, started) in &state.known {
            self.kill(&[Process {
                pid: *pid,
                started: *started,
                parent: 0,
                group: 0,
                rss_bytes: 0,
                alive: true,
            }]);
        }
        self.changed.notify_waiters();
    }

    fn kill(&self, processes: &[Process]) {
        for process in processes {
            if let Err(error) = process.kill(self.page_size_bytes) {
                tracing::warn!(
                    pid = process.pid,
                    "Cannot terminate watchdog subprocess: {error:#}"
                );
            }
        }
    }

    fn close(&self) {
        match snapshot(self.page_size_bytes) {
            Ok(processes) => {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                state.closed = true;
                self.kill(&state.owned(&processes));
            }
            Err(error) => {
                self.fail(format!(
                    "memory_watchdog_failed: cannot clean up subprocess tree: {error:#}"
                ));
                self.state.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
            }
        }
    }
}

struct TreeOwner(Arc<MemoryTree>);

impl Drop for TreeOwner {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_are_positive_and_checked_for_overflow() {
        assert_eq!(memory_bytes(1024).unwrap(), 1024 * 1024 * 1024);
        assert!(memory_bytes(0).is_err());
        assert!(memory_bytes(u64::MAX).is_err());
    }

    #[test]
    fn process_stat_accepts_spaces_and_closing_parentheses_in_names() {
        let stat = "123 (name with ) parens) S 12 123 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 456 4096 7";
        let process = parse_process(stat, 4096).unwrap();
        assert_eq!(process.pid, 123);
        assert_eq!(process.parent, 12);
        assert_eq!(process.group, 123);
        assert_eq!(process.started, 456);
        assert_eq!(process.rss_bytes, 7 * 4096);
        assert!(parse_process("123 (broken) S 1", 4096).is_err());
        assert!(parse_process("123 broken", 4096).is_err());
    }

    fn process(pid: i32, parent: i32, group: i32, started: u64) -> Process {
        Process {
            pid,
            parent,
            group,
            started,
            rss_bytes: 10,
            alive: true,
        }
    }

    #[test]
    fn ownership_includes_descendants_and_remembers_detached_processes_without_pid_reuse() {
        let mut state = TreeState::default();
        state.roots.insert(100, 1);
        let first = [
            process(100, 1, 100, 1),
            process(101, 100, 100, 2),
            process(102, 101, 102, 3),
            process(200, 1, 200, 4),
        ];
        assert_eq!(state.owned(&first).len(), 3);
        let detached = [process(102, 1, 102, 3), process(103, 102, 103, 5)];
        assert_eq!(state.owned(&detached).len(), 2);
        assert_eq!(state.owned(&[process(102, 1, 102, 30)]).len(), 0);
        state.roots.insert(100, 1);
        assert_eq!(
            state
                .owned(&[process(100, 1, 100, 9), process(104, 100, 100, 10)])
                .len(),
            0
        );
    }

    #[cfg(target_os = "linux")]
    async fn shell(command: &str) -> Result<crate::command::CommandResult> {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", command]);
        crate::command::run_command(&mut cmd, Duration::from_secs(10)).await
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn descendants_share_one_threshold_and_survive_to_retry_while_siblings_are_independent() {
        let limiter = MemoryLimiter::new(96).unwrap();
        limiter
            .scope(true, async {
                let parent = MemoryTree::current().unwrap();
                limiter
                    .scope(false, async {
                        let child = MemoryTree::current().unwrap();
                        assert!(!Arc::ptr_eq(&parent, &child));
                        limiter
                            .scope(false, async {
                                assert!(Arc::ptr_eq(&child, &MemoryTree::current().unwrap()));
                                Ok(())
                            })
                            .await
                    })
                    .await?;
                let workload =
                    "python3 -c 'import time; x=bytearray(56*1024*1024); time.sleep(30)'";
                let stressed = limiter.scope(false, async {
                    assert!(!Arc::ptr_eq(&parent, &MemoryTree::current().unwrap()));
                    let (first, second) = tokio::join!(
                        limiter.scope(false, shell(workload)),
                        limiter.scope(false, shell(workload))
                    );
                    for output in [first, second] {
                        let error = output.err().unwrap().to_string();
                        assert!(error.starts_with("memory_limit_exceeded"), "{error}");
                        assert!(error.contains("96 MiB"));
                        assert!(error.contains("not a kernel OOM diagnosis"));
                    }
                    assert_eq!(shell("printf recovered").await?.stdout, "recovered");
                    // Each agent has its own cursor: a child consuming a tool error
                    // cannot consume the tree owner's next-turn notification.
                    let mut owner_cursor = 0;
                    let mut child_cursor = 0;
                    assert!(notice_since(&mut owner_cursor).is_some());
                    assert!(notice_since(&mut child_cursor).is_some());
                    assert!(notice_since(&mut owner_cursor).is_none());
                    assert_eq!(shell("kill -9 $$").await?.exit_code, 137);
                    Ok(())
                });
                let sibling = limiter.scope(false, async {
                    assert_eq!(
                        shell("sleep 1; printf sibling-survived").await?.stdout,
                        "sibling-survived"
                    );
                    assert!(notice_since(&mut 0).is_none());
                    Ok(())
                });
                let (stressed, sibling) = tokio::join!(stressed, sibling);
                stressed?;
                sibling?;
                assert!(notice_since(&mut 0).is_none());
                Ok(())
            })
            .await
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn background_processes_are_monitored_after_the_launcher_exits() {
        let limiter = MemoryLimiter::new(48).unwrap();
        limiter.scope(true, async {
            shell("python3 -c 'import time; time.sleep(0.2); x=bytearray(64*1024*1024); time.sleep(30)' >/dev/null 2>&1 &").await?;
            let tree = MemoryTree::current().unwrap();
            tokio::time::timeout(Duration::from_secs(5), tree.exceeded(0)).await?;
            assert!(shell("printf should-not-run").await.err().unwrap().to_string().starts_with("memory_limit_exceeded"));
            assert_eq!(shell("printf recovered").await?.stdout, "recovered");
            Ok(())
        }).await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn finishing_a_tree_cleans_up_background_commands() {
        let limiter = MemoryLimiter::new(96).unwrap();
        let pid = limiter
            .scope(true, async {
                let output = shell("sleep 30 >/dev/null 2>&1 & echo $!").await?;
                Ok(output.stdout.trim().parse::<i32>()?)
            })
            .await
            .unwrap();
        // A killed orphan may briefly remain a zombie until its new parent reaps.
        for _ in 0..50 {
            let running = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')').map(|(_, fields)| {
                        fields
                            .split_whitespace()
                            .next()
                            .is_some_and(|state| !matches!(state, "Z" | "X"))
                    })
                })
                .unwrap_or(false);
            if !running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("background process {pid} survived tree cleanup");
    }
}

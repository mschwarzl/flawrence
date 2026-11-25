use anyhow::Result;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

pub struct CommandResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

// Converts signal to follow shell convention (e.g. SIGKILL=9 -> 137, SIGTERM=15 -> 143)
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

async fn read_pipe(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    max_bytes: usize,
) -> std::io::Result<(String, bool)> {
    let mut stored = Vec::with_capacity(max_bytes.min(8192));
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = pipe.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(stored.len());
        let keep = read.min(remaining);
        stored.extend_from_slice(&chunk[..keep]);
        truncated |= keep < read;
    }
    Ok((String::from_utf8_lossy(&stored).into_owned(), truncated))
}

async fn join_pipe(
    mut task: tokio::task::JoinHandle<std::io::Result<(String, bool)>>,
) -> Result<(String, bool)> {
    match tokio::time::timeout(std::time::Duration::from_secs(5), &mut task).await {
        Ok(result) => Ok(result??),
        Err(_) => {
            task.abort();
            anyhow::bail!("subprocess pipe remained open after process exit")
        }
    }
}

pub async fn run_command(cmd: &mut Command, timeout: std::time::Duration) -> Result<CommandResult> {
    run_command_bounded(
        cmd,
        timeout,
        crate::MAX_TOOL_RESPONSE_CHARS,
        crate::MAX_TOOL_RESPONSE_CHARS,
    )
    .await
}

pub async fn run_command_bounded(
    cmd: &mut Command,
    timeout: std::time::Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
) -> Result<CommandResult> {
    let memory = crate::memory::MemoryTree::current();
    let generation = memory
        .as_ref()
        .map(|tree| tree.checkpoint())
        .transpose()?
        .unwrap_or_default();
    let result = run_command_inner(
        cmd,
        timeout,
        max_stdout_bytes,
        max_stderr_bytes,
        memory.as_deref(),
        generation,
    )
    .await;
    if let Some(tree) = memory {
        tree.check(generation)?;
    }
    result
}

async fn run_command_inner(
    cmd: &mut Command,
    timeout: std::time::Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
    memory: Option<&crate::memory::MemoryTree>,
    generation: u64,
) -> Result<CommandResult> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let process_group_id = child.id().map(|id| id as i32);
    if let Some(tree) = memory
        && let Some(pid) = child.id()
        && let Err(error) = tree.track(pid)
    {
        terminate(&mut child, process_group_id).await;
        return Err(error);
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to capture stderr"))?;
    let stdout_task = tokio::spawn(read_pipe(stdout, max_stdout_bytes));
    let stderr_task = tokio::spawn(read_pipe(stderr, max_stderr_bytes));

    let (timed_out, exit_code) =
        wait_for_command(&mut child, process_group_id, timeout, memory, generation).await?;

    let (stdout, stdout_truncated) = join_pipe(stdout_task).await?;
    let (stderr, stderr_truncated) = join_pipe(stderr_task).await?;

    Ok(CommandResult {
        exit_code,
        stdout,
        stderr,
        timed_out,
        stdout_truncated,
        stderr_truncated,
    })
}

async fn wait_for_command(
    child: &mut Child,
    process_group_id: Option<i32>,
    timeout: std::time::Duration,
    memory: Option<&crate::memory::MemoryTree>,
    generation: u64,
) -> Result<(bool, i32)> {
    let wait = async {
        tokio::select! {
            status = child.wait() => status.map(Some),
            () = async {
                if let Some(tree) = memory { tree.exceeded(generation).await }
                else { std::future::pending::<()>().await }
            } => Ok(None),
        }
    };
    let outcome = match tokio::time::timeout(timeout, wait).await {
        Ok(Ok(Some(status))) => (false, exit_code(status)),
        Ok(Err(e)) => return Err(e.into()),
        stopped => {
            terminate(child, process_group_id).await;
            (stopped.is_err(), -1)
        }
    };

    Ok(outcome)
}

async fn terminate(child: &mut Child, process_group_id: Option<i32>) {
    if let Some(group) = process_group_id {
        // SAFETY: commands are launched in their own process group.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_command_drains_but_does_not_store_excess_output() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("i=0; while [ $i -lt 1000 ]; do printf x; printf y >&2; i=$((i + 1)); done");

        let result = run_command_bounded(&mut command, std::time::Duration::from_secs(5), 100, 50)
            .await
            .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.len(), 100);
        assert_eq!(result.stderr.len(), 50);
        assert!(result.stdout_truncated);
        assert!(result.stderr_truncated);
    }

    #[tokio::test]
    async fn timeout_kills_descendants_holding_output_pipes() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("(sleep 30) & wait");
        let started = std::time::Instant::now();

        let result = run_command_bounded(
            &mut command,
            std::time::Duration::from_millis(100),
            100,
            100,
        )
        .await
        .unwrap();

        assert!(result.timed_out);
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }
}

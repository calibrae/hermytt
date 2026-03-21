use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Semaphore;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 1024 * 1024; // 1MB cap per stream
const MAX_CONCURRENT: usize = 8;

static EXEC_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(MAX_CONCURRENT));

/// Execute a shell command directly (no PTY), capture stdout + stderr.
/// Timeout after 30s. Output capped at 1MB per stream. Max 8 concurrent.
pub async fn exec(cmd: &str, shell: &str) -> Result<ExecResult> {
    let _permit = EXEC_SEMAPHORE
        .acquire()
        .await
        .map_err(|_| anyhow::anyhow!("exec semaphore closed"))?;
    let mut child = Command::new(shell)
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let mut stdout_buf = vec![0u8; MAX_OUTPUT];
    let mut stderr_buf = vec![0u8; MAX_OUTPUT];
    let mut stdout_len = 0;
    let mut stderr_len = 0;

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();

    let result = tokio::time::timeout(DEFAULT_TIMEOUT, async {
        let (so, se) = tokio::join!(
            async {
                loop {
                    if stdout_len >= MAX_OUTPUT {
                        break;
                    }
                    match stdout.read(&mut stdout_buf[stdout_len..]).await {
                        Ok(0) => break,
                        Ok(n) => stdout_len += n,
                        Err(_) => break,
                    }
                }
            },
            async {
                loop {
                    if stderr_len >= MAX_OUTPUT {
                        break;
                    }
                    match stderr.read(&mut stderr_buf[stderr_len..]).await {
                        Ok(0) => break,
                        Ok(n) => stderr_len += n,
                        Err(_) => break,
                    }
                }
            }
        );
        drop((so, se, stdout, stderr));
        child.wait().await
    })
    .await;

    let status = match result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("command timed out after {}s", DEFAULT_TIMEOUT.as_secs());
        }
    };

    let stdout_str = String::from_utf8_lossy(&stdout_buf[..stdout_len]).to_string();
    let stderr_str = String::from_utf8_lossy(&stderr_buf[..stderr_len]).to_string();

    Ok(ExecResult {
        stdout: stdout_str,
        stderr: stderr_str,
        exit_code: status.code().unwrap_or(-1),
        truncated: stdout_len >= MAX_OUTPUT || stderr_len >= MAX_OUTPUT,
    })
}

pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub truncated: bool,
}

impl ExecResult {
    pub fn combined(&self) -> String {
        let mut out = if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout.trim_end(), self.stderr)
        };
        if self.truncated {
            out.push_str("\n(output truncated)");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exec_simple_command() {
        let result = exec("echo hello", "/bin/sh").await.unwrap();
        assert_eq!(result.stdout.trim(), "hello");
        assert_eq!(result.exit_code, 0);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn exec_captures_stderr() {
        let result = exec("echo err >&2", "/bin/sh").await.unwrap();
        assert_eq!(result.stderr.trim(), "err");
    }

    #[tokio::test]
    async fn exec_exit_code() {
        let result = exec("exit 42", "/bin/sh").await.unwrap();
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn exec_combined_output() {
        let result = exec("echo out; echo err >&2", "/bin/sh").await.unwrap();
        let combined = result.combined();
        assert!(combined.contains("out"));
        assert!(combined.contains("err"));
    }

    #[tokio::test]
    async fn exec_no_ansi_junk() {
        let result = exec("whoami", "/bin/sh").await.unwrap();
        assert!(!result.stdout.contains('\x1b'));
    }

    #[tokio::test]
    async fn exec_timeout() {
        let result = exec("sleep 60", "/bin/sh").await;
        assert!(result.is_err());
        let err = format!("{}", result.err().unwrap());
        assert!(err.contains("timed out"));
    }
}

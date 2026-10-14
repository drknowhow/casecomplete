//! The `llama-server` child process.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::net::{Ipv4Addr, TcpListener};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};

use crate::assets::Assets;

const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);
const HEALTH_INTERVAL: Duration = Duration::from_millis(100);
const TAIL_LINES: usize = 40;

/// A running `llama-server` bound to 127.0.0.1. The process is killed when this is dropped
/// and, on Windows, when this process dies for any reason (job object with kill-on-close).
pub struct Sidecar {
    child: Child,
    base_url: String,
    #[cfg(windows)]
    _job: job::Job,
}

#[cfg(windows)]
mod job {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(HANDLE);

    impl Job {
        pub fn kill_on_close(child: HANDLE) -> anyhow::Result<Job> {
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: plain Win32 calls with valid handles and a correctly sized struct.
            unsafe {
                let job = CreateJobObjectW(None, None)?;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )?;
                AssignProcessToJobObject(job, child)?;
                Ok(Job(job))
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: handle was created by CreateJobObjectW and is closed exactly once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// The server exited at startup because it did not recognise one of our flags.
#[derive(Debug)]
struct ArgsRejected(String);

impl std::fmt::Display for ArgsRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "llama-server rejected its arguments:\n{}", self.0)
    }
}

impl std::error::Error for ArgsRejected {}

type LogTail = Arc<Mutex<VecDeque<String>>>;

fn server_args(assets: &Assets, port: u16, full: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-m".into(),
        assets.model.clone().into_os_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
        "-ngl".into(),
        "99".into(),
        "-c".into(),
        "2048".into(),
    ];
    if full {
        // Nice-to-have flags that a future llama.cpp may rename or drop.
        args.extend(["-np", "1", "--cache-prompt"].map(OsString::from));
    }
    args
}

fn looks_like_bad_args(tail: &str) -> bool {
    let t = tail.to_ascii_lowercase();
    ["invalid argument", "unknown argument", "unrecognized argument", "unknown option"]
        .iter()
        .any(|needle| t.contains(needle))
}

fn pump_lines<R>(reader: R, stream: &'static str, tail: LogTail)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log::debug!(target: "llama-server", "[{stream}] {line}");
            let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
            if t.len() >= TAIL_LINES {
                t.pop_front();
            }
            t.push_back(line);
        }
    });
}

fn tail_text(tail: &LogTail) -> String {
    let t = tail.lock().unwrap_or_else(|e| e.into_inner());
    t.iter().map(String::as_str).collect::<Vec<_>>().join("\n")
}

impl Sidecar {
    /// Spawn `llama-server -m <model> --host 127.0.0.1 --port <port> -ngl 99 -c 2048
    /// -np 1 --cache-prompt` and wait (up to 60 s) for `GET /health` to return 200.
    ///
    /// If the build rejects `-np` / `--cache-prompt` the server is restarted once without
    /// them. stdout/stderr are forwarded to `log::debug!` (target `llama-server`).
    pub async fn start(assets: &Assets, port: u16) -> anyhow::Result<Sidecar> {
        // A stale server on this port would answer /health and mask our own startup failure.
        TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .with_context(|| format!("port {port} on 127.0.0.1 is not available"))?;

        match Self::launch(assets, port, true).await {
            Err(e) if e.downcast_ref::<ArgsRejected>().is_some() => {
                log::warn!("{e}\nretrying with core flags only");
                Self::launch(assets, port, false).await
            }
            other => other,
        }
    }

    async fn launch(assets: &Assets, port: u16, full: bool) -> Result<Sidecar> {
        let args = server_args(assets, port, full);
        let mut cmd = Command::new(&assets.server_exe);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

        log::debug!("spawning {} {:?}", assets.server_exe.display(), args);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", assets.server_exe.display()))?;
        #[cfg(windows)]
        let job = {
            let raw = child.raw_handle().context("child handle already closed")?;
            job::Job::kill_on_close(windows::Win32::Foundation::HANDLE(raw))
                .context("assigning llama-server to a job object")?
        };

        let tail: LogTail = Arc::default();
        if let Some(out) = child.stdout.take() {
            pump_lines(out, "stdout", tail.clone());
        }
        if let Some(err) = child.stderr.take() {
            pump_lines(err, "stderr", tail.clone());
        }

        let base_url = format!("http://127.0.0.1:{port}");
        let health_url = format!("{base_url}/health");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .context("building health-check client")?;

        let deadline = Instant::now() + HEALTH_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait().context("polling llama-server")? {
                // Let the log pumps drain the last lines before we quote them.
                tokio::time::sleep(Duration::from_millis(150)).await;
                let tail = tail_text(&tail);
                if looks_like_bad_args(&tail) {
                    return Err(ArgsRejected(tail).into());
                }
                bail!("llama-server exited during startup ({status}):\n{tail}");
            }
            if let Ok(resp) = http.get(&health_url).send().await {
                if resp.status().as_u16() == 200 {
                    break;
                }
            }
            if Instant::now() >= deadline {
                let _ = child.start_kill();
                bail!(
                    "llama-server did not become healthy within {}s:\n{}",
                    HEALTH_TIMEOUT.as_secs(),
                    tail_text(&tail)
                );
            }
            tokio::time::sleep(HEALTH_INTERVAL).await;
        }
        log::debug!("llama-server healthy at {base_url}");
        Ok(Sidecar {
            child,
            base_url,
            #[cfg(windows)]
            _job: job,
        })
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        // `kill_on_drop` would do this too; being explicit keeps it obvious.
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rejected_flags() {
        assert!(looks_like_bad_args("error: invalid argument: --cache-prompt"));
        assert!(looks_like_bad_args("Unknown argument: -np"));
        assert!(!looks_like_bad_args("failed to load model"));
    }

    #[test]
    fn core_args_are_a_prefix_of_full_args() {
        let assets = Assets { server_exe: "llama-server.exe".into(), model: "m.gguf".into() };
        let core = server_args(&assets, 8089, false);
        let full = server_args(&assets, 8089, true);
        assert_eq!(&full[..core.len()], &core[..]);
        assert!(full.iter().any(|a| a == "--cache-prompt"));
        assert!(!core.iter().any(|a| a == "--cache-prompt"));
        assert!(core.iter().any(|a| a == "8089"));
    }
}

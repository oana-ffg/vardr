use crate::model::Outcome;
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::fd::AsRawFd,
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

pub static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}
pub fn handle_signals() {
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(signal, stop as *const () as libc::sighandler_t);
        }
    }
}
struct Log {
    file: Option<File>,
    captured: Vec<u8>,
    left: u64,
    truncated: bool,
    exit_code: Option<i32>,
}
impl Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let n = bytes.len().min(self.left as usize);
        if let Some(file) = &mut self.file {
            file.write_all(&bytes[..n])?;
        } else {
            self.captured.extend_from_slice(&bytes[..n]);
        }
        self.left -= n as u64;
        if n < bytes.len() && !self.truncated {
            if let Some(file) = &mut self.file {
                file.write_all(b"\n[Vardr: output limit reached; remaining output discarded]\n")?;
            }
            self.truncated = true;
        }
        Ok(())
    }
}
struct ChildGroup {
    child: Child,
    reaped: bool,
}
impl ChildGroup {
    fn finish(&mut self) -> Result<std::process::ExitStatus> {
        // Keep the leader unreaped until its group is killed, so its PID cannot
        // be reused for an unrelated process group during cleanup.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
    fn exited(&self) -> Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e.into());
        }
        Ok(info.si_signo != 0)
    }
}
impl Drop for ChildGroup {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.finish();
        }
    }
}
fn nonblocking(pipe: &impl AsRawFd) -> Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    anyhow::ensure!(flags >= 0, "cannot read pipe flags");
    anyhow::ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        "cannot set nonblocking pipe"
    );
    Ok(())
}
fn drain(reader: &mut Option<impl Read>, sink: &mut Log) -> Result<()> {
    let Some(pipe) = reader.as_mut() else {
        return Ok(());
    };
    let mut bytes = [0u8; 8192];
    // Bound each iteration, including output from a continuously noisy child.
    for _ in 0..16 {
        match pipe.read(&mut bytes) {
            Ok(0) => {
                *reader = None;
                return Ok(());
            }
            Ok(n) => sink.write(&bytes[..n])?,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Nonblocking pipe I/O avoids reader/writer threads that could outlive a task.
/// Escaped descendants cannot keep the runner waiting beyond the cleanup grace.
pub fn execute(
    argv: &[String],
    cwd: &Path,
    input: Option<&str>,
    log: &Path,
    limit: u64,
    timeout: Duration,
) -> Result<Outcome> {
    execute_with_env(argv, cwd, input, &BTreeMap::new(), log, limit, timeout)
}

pub(crate) fn execute_with_env(
    argv: &[String],
    cwd: &Path,
    input: Option<&str>,
    env: &BTreeMap<String, String>,
    log: &Path,
    limit: u64,
    timeout: Duration,
) -> Result<Outcome> {
    let mut sink = Log {
        file: Some(OpenOptions::new().create_new(true).write(true).open(log)?),
        captured: Vec::new(),
        left: limit,
        truncated: false,
        exit_code: None,
    };
    execute_to(argv, cwd, input, env, &mut sink, timeout)
}

/// Bounded, in-memory stdout capture for credentials and executable version checks.
/// Stderr is discarded; failures never include captured values in error messages.
pub(crate) fn capture(
    argv: &[String],
    cwd: &Path,
    limit: u64,
    timeout: Duration,
) -> Result<Vec<u8>> {
    capture_input(argv, cwd, None, limit, timeout)
}

pub(crate) fn capture_input(
    argv: &[String],
    cwd: &Path,
    input: Option<&str>,
    limit: u64,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let result = capture_inner(argv, cwd, input, limit, timeout)?;
    anyhow::ensure!(
        result.outcome.status == "succeeded",
        "capture command {}",
        result.outcome.status
    );
    Ok(result.stdout)
}

pub(crate) struct CaptureResult {
    pub outcome: Outcome,
    pub stdout: Vec<u8>,
    pub exit_code: Option<i32>,
}

pub(crate) fn capture_result(
    argv: &[String],
    cwd: &Path,
    limit: u64,
    timeout: Duration,
) -> Result<CaptureResult> {
    capture_inner(argv, cwd, None, limit, timeout)
}

fn capture_inner(
    argv: &[String],
    cwd: &Path,
    input: Option<&str>,
    limit: u64,
    timeout: Duration,
) -> Result<CaptureResult> {
    let mut sink = Log {
        file: None,
        captured: Vec::new(),
        left: limit,
        truncated: false,
        exit_code: None,
    };
    let result = execute_to(argv, cwd, input, &BTreeMap::new(), &mut sink, timeout)?;
    anyhow::ensure!(!sink.truncated, "capture command output exceeded its limit");
    Ok(CaptureResult {
        outcome: result,
        stdout: sink.captured,
        exit_code: sink.exit_code,
    })
}
fn execute_to(
    argv: &[String],
    cwd: &Path,
    input: Option<&str>,
    env: &BTreeMap<String, String>,
    sink: &mut Log,
    timeout: Duration,
) -> Result<Outcome> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .envs(env)
        .current_dir(cwd)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(if sink.file.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let child = cmd
        .spawn()
        .with_context(|| format!("could not start {}", argv[0]))?;
    let mut group = ChildGroup {
        child,
        reaped: false,
    };
    let mut stdout = group.child.stdout.take();
    let mut stderr = group.child.stderr.take();
    let mut stdin = group.child.stdin.take();
    nonblocking(stdout.as_ref().unwrap())?;
    if let Some(pipe) = &stderr {
        nonblocking(pipe)?;
    }
    if let Some(pipe) = &stdin {
        nonblocking(pipe)?;
    }
    let input = input.unwrap_or("").as_bytes();
    let mut written = 0;
    let deadline = Instant::now() + timeout;
    let mut completed: Option<(Instant, Outcome)> = None;
    loop {
        drain(&mut stdout, sink)?;
        drain(&mut stderr, sink)?;
        if let Some(pipe) = stdin.as_mut() {
            if written == input.len() {
                stdin = None;
            } else {
                match pipe.write(&input[written..input.len().min(written + 131072)]) {
                    Ok(n) => written += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => stdin = None,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if completed.is_none() {
            let stopped = STOP.load(Ordering::Relaxed);
            if stopped || Instant::now() >= deadline {
                group.finish()?;
                stdin = None;
                completed = Some((
                    Instant::now(),
                    Outcome {
                        status: if stopped { "interrupted" } else { "timed_out" }.into(),
                        error: Some(
                            if stopped {
                                "runner stopped"
                            } else {
                                "timeout exceeded"
                            }
                            .into(),
                        ),
                        remote: None,
                    },
                ));
            } else if group.exited()? {
                let status = group.finish()?;
                sink.exit_code = status.code();
                stdin = None;
                completed = Some((
                    Instant::now(),
                    if status.success() {
                        Outcome::success()
                    } else {
                        Outcome::failed(format!("process exited with {status}"))
                    },
                ));
            }
        }
        if let Some((finished, _)) = &completed
            && ((stdout.is_none() && stderr.is_none())
                || finished.elapsed() >= Duration::from_millis(250))
        {
            return Ok(completed.unwrap().1);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

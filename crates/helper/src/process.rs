//! Controlled Unix child-process primitives.

use std::fs::File;
use std::io::{self, Read};
use std::ops::ControlFlow;
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, Command, ExitStatus, Stdio};
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Default TERM-to-KILL grace for media helpers.
pub const DEFAULT_TERMINATION_GRACE: Duration = Duration::from_millis(200);

// Capture shutdown uses an atomic flag rather than another inherited fd. This
// timeout bounds a join behind an escaped pipe writer while keeping idle
// helpers asleep in the kernel instead of polling every millisecond.
const CAPTURE_READINESS_TIMEOUT_MS: libc::c_int = 50;

// Once supervision has finished, retain a bounded sample of bytes that were
// already readable without letting an escaped, continuously writing process
// keep the capture thread alive. These caps bound both shutdown latency and
// post-stop I/O independently of the caller's retention limit.
const CAPTURE_STOP_DRAIN_TIMEOUT: Duration = Duration::from_millis(25);
const CAPTURE_STOP_DRAIN_MAX_READS: usize = 32;
const CAPTURE_STOP_DRAIN_MAX_BYTES: usize = 256 * 1024;

/// Which end of a bounded diagnostic stream remains available to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureRetention {
    Head,
    Tail,
}

/// Whether exceeding the retention limit is merely truncated or reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureOverflow {
    Truncate,
    Error,
}

/// Whether a pipe read error is ignored after retaining prior bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureReadError {
    Ignore,
    Error,
}

/// Memory and failure policy for one captured child stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureConfig {
    pub limit: usize,
    pub retention: CaptureRetention,
    pub overflow: CaptureOverflow,
    pub read_error: CaptureReadError,
}

impl CaptureConfig {
    pub const fn new(limit: usize, retention: CaptureRetention) -> Self {
        Self {
            limit,
            retention,
            overflow: CaptureOverflow::Truncate,
            read_error: CaptureReadError::Ignore,
        }
    }

    pub const fn overflow(mut self, overflow: CaptureOverflow) -> Self {
        self.overflow = overflow;
        self
    }

    pub const fn read_error(mut self, read_error: CaptureReadError) -> Self {
        self.read_error = read_error;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedStream {
    Stdout,
    Stderr,
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisionError {
    #[error("spawn helper: {0}")]
    Spawn(#[source] io::Error),
    #[error("wait for helper: {0}")]
    Wait(#[source] io::Error),
    #[error("read helper {stream:?}: {source}")]
    CaptureIo {
        stream: CapturedStream,
        #[source]
        source: io::Error,
    },
    #[error("helper {stream:?} exceeded {limit} bytes")]
    CaptureOverflow {
        stream: CapturedStream,
        limit: usize,
    },
    #[error("helper {0:?} reader panicked")]
    CapturePanicked(CapturedStream),
}

#[derive(Debug)]
enum CaptureFailure {
    Io(io::Error),
    Overflow { limit: usize },
}

#[derive(Debug)]
struct CaptureTask {
    stream: CapturedStream,
    thread: JoinHandle<Result<Vec<u8>, CaptureFailure>>,
}

impl CaptureTask {
    fn join(self) -> Result<Vec<u8>, SupervisionError> {
        match self.thread.join() {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(CaptureFailure::Io(source))) => Err(SupervisionError::CaptureIo {
                stream: self.stream,
                source,
            }),
            Ok(Err(CaptureFailure::Overflow { limit })) => Err(SupervisionError::CaptureOverflow {
                stream: self.stream,
                limit,
            }),
            Err(_) => Err(SupervisionError::CapturePanicked(self.stream)),
        }
    }
}

fn spawn_capture<R>(
    reader: R,
    stream: CapturedStream,
    config: CaptureConfig,
    stop: Arc<AtomicBool>,
) -> io::Result<CaptureTask>
where
    R: Read + AsRawFd + Send + 'static,
{
    let descriptor = reader.as_raw_fd();
    set_nonblocking(descriptor)?;
    let label = match stream {
        CapturedStream::Stdout => "stdout",
        CapturedStream::Stderr => "stderr",
    };
    let thread = std::thread::Builder::new()
        .name(format!("helper-{label}-capture"))
        .spawn(move || {
            drain_bounded(
                reader,
                Some(descriptor),
                config,
                &stop,
                #[cfg(test)]
                None,
            )
        })?;
    Ok(CaptureTask { stream, thread })
}

fn set_nonblocking(descriptor: RawFd) -> io::Result<()> {
    // SAFETY: callers provide a valid descriptor and keep its owner alive
    // through both fcntl calls and subsequent reads.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the caller still owns `descriptor`; F_SETFL changes only its
    // open-file status flags and preserves every existing flag.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureReadiness {
    Ready,
    Terminal,
    TimedOut,
}

fn wait_for_capture_readiness(
    descriptor: RawFd,
    stop: &AtomicBool,
) -> io::Result<CaptureReadiness> {
    let mut descriptor = libc::pollfd {
        fd: descriptor,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `descriptor` is live writable storage for one pollfd whose
        // fd remains owned by the capture reader for the duration of the call.
        let result = unsafe { libc::poll(&mut descriptor, 1, CAPTURE_READINESS_TIMEOUT_MS) };
        if result == 0 {
            return Ok(CaptureReadiness::TimedOut);
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                if stop.load(Ordering::Acquire) {
                    return Ok(CaptureReadiness::TimedOut);
                }
                continue;
            }
            return Err(error);
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        if descriptor.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(CaptureReadiness::Terminal);
        }
        return Ok(CaptureReadiness::Ready);
    }
}

fn drain_bounded(
    mut reader: impl Read,
    descriptor: Option<RawFd>,
    config: CaptureConfig,
    stop: &AtomicBool,
    #[cfg(test)] wait_counter: Option<&AtomicU64>,
) -> Result<Vec<u8>, CaptureFailure> {
    let mut kept = Vec::with_capacity(config.limit);
    let mut overflowed = false;
    let mut terminal = false;
    let mut stop_started = None;
    let mut stop_reads = 0usize;
    let mut stop_bytes = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        if stop.load(Ordering::Acquire) {
            let started = stop_started.get_or_insert_with(Instant::now);
            if started.elapsed() >= CAPTURE_STOP_DRAIN_TIMEOUT
                || stop_reads >= CAPTURE_STOP_DRAIN_MAX_READS
                || stop_bytes >= CAPTURE_STOP_DRAIN_MAX_BYTES
            {
                break;
            }
        }
        let read_limit = stop_started.map_or(chunk.len(), |_| {
            chunk
                .len()
                .min(CAPTURE_STOP_DRAIN_MAX_BYTES.saturating_sub(stop_bytes))
        });
        let read = match reader.read(&mut chunk[..read_limit]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) || terminal {
                    break;
                }
                let Some(descriptor) = descriptor else {
                    if config.read_error == CaptureReadError::Ignore {
                        break;
                    }
                    return Err(CaptureFailure::Io(error));
                };
                #[cfg(test)]
                if let Some(counter) = wait_counter {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                match wait_for_capture_readiness(descriptor, stop) {
                    Ok(CaptureReadiness::Ready | CaptureReadiness::TimedOut) => {}
                    Ok(CaptureReadiness::Terminal) => terminal = true,
                    Err(_) if config.read_error == CaptureReadError::Ignore => break,
                    Err(error) => return Err(CaptureFailure::Io(error)),
                }
                continue;
            }
            Err(_) if config.read_error == CaptureReadError::Ignore => break,
            Err(error) => return Err(CaptureFailure::Io(error)),
        };
        if stop_started.is_some() {
            stop_reads = stop_reads.saturating_add(1);
            stop_bytes = stop_bytes.saturating_add(read);
        }
        match config.retention {
            CaptureRetention::Head => {
                let room = config.limit.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..read.min(room)]);
                overflowed |= read > room;
            }
            CaptureRetention::Tail => {
                kept.extend_from_slice(&chunk[..read]);
                if kept.len() > config.limit {
                    let discard = kept.len() - config.limit;
                    kept.drain(..discard);
                    overflowed = true;
                }
            }
        }
    }
    if overflowed && config.overflow == CaptureOverflow::Error {
        return Err(CaptureFailure::Overflow {
            limit: config.limit,
        });
    }
    Ok(kept)
}

#[derive(Debug)]
struct InheritedFile {
    source: File,
    target: RawFd,
}

/// A command configured for bounded, isolated helper execution.
///
/// An inherited file is cloned and owned by its pre-exec closure, so reusing
/// the `Command` cannot act on a descriptor whose original file was dropped.
/// Source descriptors are normalized above every target before `fork`, so
/// multiple mappings cannot overwrite one another during sequential `dup2`.
#[derive(Debug)]
pub struct SupervisedCommand<'a> {
    command: &'a mut Command,
    stdout: Option<CaptureConfig>,
    stderr: Option<CaptureConfig>,
    inherited: Vec<InheritedFile>,
    termination_grace: Duration,
}

impl<'a> SupervisedCommand<'a> {
    pub fn new(command: &'a mut Command) -> Self {
        Self {
            command,
            stdout: None,
            stderr: None,
            inherited: Vec::new(),
            termination_grace: DEFAULT_TERMINATION_GRACE,
        }
    }

    pub fn capture_stdout(mut self, config: CaptureConfig) -> Self {
        self.stdout = Some(config);
        self
    }

    pub fn capture_stderr(mut self, config: CaptureConfig) -> Self {
        self.stderr = Some(config);
        self
    }

    pub fn termination_grace(mut self, grace: Duration) -> Self {
        self.termination_grace = grace;
        self
    }

    /// Make one owned clone of `source` available as `target` in the child.
    pub fn inherit_file_at(mut self, source: &File, target: RawFd) -> io::Result<Self> {
        if target <= libc::STDERR_FILENO {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inherited child descriptor must be greater than standard error",
            ));
        }
        if self
            .inherited
            .iter()
            .any(|inherited| inherited.target == target)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inherited child descriptor target is already in use",
            ));
        }
        self.inherited.push(InheritedFile {
            source: source.try_clone()?,
            target,
        });
        Ok(self)
    }

    fn spawn(mut self) -> io::Result<SupervisedChild> {
        use std::os::unix::process::CommandExt;

        if self.inherited.len() > 1 {
            use std::os::fd::FromRawFd;

            let mut minimum = self
                .inherited
                .iter()
                .map(|inherited| inherited.target)
                .max()
                .unwrap_or(libc::STDERR_FILENO)
                .checked_add(1)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "descriptor overflow")
                })?;
            for inherited in &mut self.inherited {
                // SAFETY: `F_DUPFD_CLOEXEC` returns a new owned descriptor on
                // success. Wrapping it in `File` transfers that ownership.
                let duplicate = unsafe {
                    libc::fcntl(inherited.source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, minimum)
                };
                if duplicate < 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: `duplicate` is a fresh descriptor owned by this call.
                inherited.source = unsafe { File::from_raw_fd(duplicate) };
                minimum = duplicate.checked_add(1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "descriptor overflow")
                })?;
            }
        }

        self.command
            .stdin(Stdio::null())
            .stdout(if self.stdout.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(if self.stderr.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .process_group(0);
        for inherited in self.inherited {
            let source_fd = inherited.source.as_raw_fd();
            let target_fd = inherited.target;
            // SAFETY: the closure owns the cloned File and performs only
            // async-signal-safe dup2/fcntl calls between fork and exec.
            unsafe {
                self.command.pre_exec(move || {
                    if source_fd != target_fd && libc::dup2(source_fd, target_fd) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::fcntl(target_fd, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let _keep_source_alive = &inherited.source;
                    Ok(())
                });
            }
        }
        let mut child = self.command.spawn()?;
        let mut stdout = None;
        let mut stderr = None;
        let capture_stop = Arc::new(AtomicBool::new(false));
        let capture_result = (|| {
            if let (Some(reader), Some(config)) = (child.stdout.take(), self.stdout) {
                stdout = Some(spawn_capture(
                    reader,
                    CapturedStream::Stdout,
                    config,
                    Arc::clone(&capture_stop),
                )?);
            }
            if let (Some(reader), Some(config)) = (child.stderr.take(), self.stderr) {
                stderr = Some(spawn_capture(
                    reader,
                    CapturedStream::Stderr,
                    config,
                    Arc::clone(&capture_stop),
                )?);
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(error) = capture_result {
            terminate_raw_child(&mut child, self.termination_grace);
            capture_stop.store(true, Ordering::Release);
            if let Some(task) = stdout {
                let _ = task.join();
            }
            if let Some(task) = stderr {
                let _ = task.join();
            }
            return Err(error);
        }
        Ok(SupervisedChild {
            child,
            stdout,
            stderr,
            capture_stop,
            termination_grace: self.termination_grace,
            collected: false,
        })
    }

    /// Run the helper to exit, a deadline, or a caller-specific stop reason.
    ///
    /// The observer is checked before spawning, so pre-cancelled work never
    /// creates a process.
    pub fn run_until<R>(
        self,
        deadline: Instant,
        poll_interval: Duration,
        mut observe: impl FnMut() -> ControlFlow<R>,
    ) -> Result<SupervisedOutcome<R>, SupervisionError> {
        if let ControlFlow::Break(reason) = observe() {
            return Ok(SupervisedOutcome::NotStarted { reason });
        }
        if Instant::now() >= deadline {
            return Ok(SupervisedOutcome::Deadline {
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        self.spawn()
            .map_err(SupervisionError::Spawn)?
            .wait_until(deadline, poll_interval, observe)
    }
}

#[derive(Debug)]
pub struct SupervisedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub enum SupervisedOutcome<R> {
    NotStarted {
        reason: R,
    },
    Exited(SupervisedOutput),
    Deadline {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Stopped {
        reason: R,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
}

/// An isolated child bundled with its bounded capture tasks.
#[derive(Debug)]
struct SupervisedChild {
    child: Child,
    stdout: Option<CaptureTask>,
    stderr: Option<CaptureTask>,
    capture_stop: Arc<AtomicBool>,
    termination_grace: Duration,
    collected: bool,
}

impl SupervisedChild {
    fn wait_until<R>(
        mut self,
        deadline: Instant,
        poll_interval: Duration,
        mut observe: impl FnMut() -> ControlFlow<R>,
    ) -> Result<SupervisedOutcome<R>, SupervisionError> {
        let poll_interval = poll_interval.max(Duration::from_millis(1));
        loop {
            if let ControlFlow::Break(reason) = observe() {
                let (stdout, stderr) = self.stop_and_collect();
                return Ok(SupervisedOutcome::Stopped {
                    reason,
                    stdout,
                    stderr,
                });
            }
            if Instant::now() >= deadline {
                let (stdout, stderr) = self.stop_and_collect();
                return Ok(SupervisedOutcome::Deadline { stdout, stderr });
            }
            match leader_has_exited(&mut self.child) {
                Ok(true) => return self.collect_exited().map(SupervisedOutcome::Exited),
                Ok(false) => std::thread::sleep(
                    poll_interval.min(deadline.saturating_duration_since(Instant::now())),
                ),
                Err(error) => {
                    let _ = self.stop_and_collect();
                    return Err(SupervisionError::Wait(error));
                }
            }
        }
    }

    fn collect_exited(&mut self) -> Result<SupervisedOutput, SupervisionError> {
        signal_group(&self.child, libc::SIGKILL);
        let status = self.child.wait().map_err(SupervisionError::Wait)?;
        self.collected = true;
        self.capture_stop.store(true, Ordering::Release);
        let stdout_result = self
            .stdout
            .take()
            .map(CaptureTask::join)
            .unwrap_or_else(|| Ok(Vec::new()));
        let stderr_result = self
            .stderr
            .take()
            .map(CaptureTask::join)
            .unwrap_or_else(|| Ok(Vec::new()));
        let stdout = stdout_result?;
        let stderr = stderr_result?;
        Ok(SupervisedOutput {
            status,
            stdout,
            stderr,
        })
    }

    fn stop_and_collect(&mut self) -> (Vec<u8>, Vec<u8>) {
        terminate_raw_child(&mut self.child, self.termination_grace);
        self.collected = true;
        self.capture_stop.store(true, Ordering::Release);
        let stdout = self
            .stdout
            .take()
            .and_then(|task| task.join().ok())
            .unwrap_or_default();
        let stderr = self
            .stderr
            .take()
            .and_then(|task| task.join().ok())
            .unwrap_or_default();
        (stdout, stderr)
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if !self.collected {
            let _ = self.stop_and_collect();
        }
    }
}

fn signal_group(child: &Child, signal: libc::c_int) {
    // SAFETY: supervised helpers are leaders of fresh process groups. The
    // unreaped leader reserves this process-group identity until final wait.
    unsafe {
        libc::kill(-(child.id() as i32), signal);
    }
}

fn terminate_raw_child(child: &mut Child, grace: Duration) {
    signal_group(child, libc::SIGTERM);
    let started = Instant::now();
    // Keep the unreaped leader as the group-identity reservation for the full
    // grace, even if it exits first, so descendants can finish TERM handlers.
    while started.elapsed() < grace {
        match leader_has_exited(child) {
            Err(_) => break,
            Ok(_) => std::thread::sleep(
                grace
                    .saturating_sub(started.elapsed())
                    .min(Duration::from_millis(10)),
            ),
        }
    }
    signal_group(child, libc::SIGKILL);
    let _ = child.wait();
}

fn leader_has_exited(child: &mut Child) -> io::Result<bool> {
    // Darwin may leave this untouched when WNOHANG finds a running child, so
    // zero it before the call and distinguish SIGCHLD from the zero sentinel.
    // SAFETY: every all-zero byte pattern is valid initial storage for the C
    // `siginfo_t`; waitid fills it when an exited child is observed.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is writable siginfo storage. WNOWAIT observes without
    // reaping on Unix, preserving the process-group identity until final wait.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    match info.si_signo {
        libc::SIGCHLD => Ok(true),
        0 => Ok(false),
        signal => Err(io::Error::other(format!(
            "unexpected si_signo from waitid: {signal}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

    fn temp_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "rusty-dlna-helper-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn run_capture(
        script: &str,
        stdout: CaptureConfig,
        stderr: CaptureConfig,
    ) -> Result<SupervisedOutput, SupervisionError> {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        let child = SupervisedCommand::new(&mut command)
            .capture_stdout(stdout)
            .capture_stderr(stderr)
            .spawn()
            .unwrap();
        match child.wait_until(
            Instant::now() + Duration::from_secs(2),
            Duration::from_millis(5),
            || ControlFlow::<()>::Continue(()),
        )? {
            SupervisedOutcome::Exited(output) => Ok(output),
            _ => panic!("short command did not exit"),
        }
    }

    #[test]
    fn captures_keep_requested_end_and_report_overflow() {
        let output = run_capture(
            "printf 0123456789; printf abcdefghij >&2",
            CaptureConfig::new(4, CaptureRetention::Head),
            CaptureConfig::new(4, CaptureRetention::Tail),
        )
        .unwrap();
        assert_eq!(output.stdout, b"0123");
        assert_eq!(output.stderr, b"ghij");

        let error = run_capture(
            "printf 0123456789",
            CaptureConfig::new(4, CaptureRetention::Head).overflow(CaptureOverflow::Error),
            CaptureConfig::new(4, CaptureRetention::Tail),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            SupervisionError::CaptureOverflow {
                stream: CapturedStream::Stdout,
                limit: 4
            }
        ));
    }

    #[test]
    fn zero_limit_and_pipe_read_error_follow_capture_policy() {
        let stop = AtomicBool::new(false);
        let empty = drain_bounded(
            Cursor::new(b"bytes"),
            None,
            CaptureConfig::new(0, CaptureRetention::Tail),
            &stop,
            None,
        )
        .unwrap();
        assert!(empty.is_empty());

        struct BrokenReader;
        impl Read for BrokenReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broken pipe reader"))
            }
        }
        let ignored = drain_bounded(
            BrokenReader,
            None,
            CaptureConfig::new(8, CaptureRetention::Head),
            &stop,
            None,
        )
        .unwrap();
        assert!(ignored.is_empty());
        let error = drain_bounded(
            BrokenReader,
            None,
            CaptureConfig::new(8, CaptureRetention::Head).read_error(CaptureReadError::Error),
            &stop,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, CaptureFailure::Io(_)));

        struct InterruptedOnce {
            interrupted: bool,
            emitted: bool,
        }
        impl Read for InterruptedOnce {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                if self.emitted {
                    return Ok(0);
                }
                self.emitted = true;
                buffer[0] = b'x';
                Ok(1)
            }
        }
        let retried = drain_bounded(
            InterruptedOnce {
                interrupted: false,
                emitted: false,
            },
            None,
            CaptureConfig::new(8, CaptureRetention::Head).read_error(CaptureReadError::Error),
            &stop,
            None,
        )
        .unwrap();
        assert_eq!(retried, b"x");
    }

    #[test]
    fn idle_capture_uses_bounded_readiness_waits_instead_of_spinning() {
        let (reader, writer) = UnixStream::pair().unwrap();
        let descriptor = reader.as_raw_fd();
        set_nonblocking(descriptor).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_later = Arc::clone(&stop);
        let start = Arc::new(std::sync::Barrier::new(2));
        let stopper_start = Arc::clone(&start);
        let stopper = std::thread::spawn(move || {
            stopper_start.wait();
            std::thread::sleep(Duration::from_millis(220));
            stop_later.store(true, Ordering::Release);
        });
        let waits = AtomicU64::new(0);
        let started = Instant::now();
        start.wait();

        let captured = drain_bounded(
            reader,
            Some(descriptor),
            CaptureConfig::new(16, CaptureRetention::Head),
            &stop,
            Some(&waits),
        )
        .unwrap();

        stopper.join().unwrap();
        drop(writer);
        assert!(captured.is_empty());
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(200));
        assert!(elapsed < Duration::from_secs(2));
        let waits = waits.load(Ordering::Relaxed);
        let maximum_low_wakeup_waits = elapsed.as_millis() / 40 + 2;
        assert!(waits >= 3, "idle reader waited only {waits} times");
        assert!(
            u128::from(waits) <= maximum_low_wakeup_waits,
            "idle reader waited {waits} times in {elapsed:?}"
        );
    }

    #[test]
    fn idle_capture_deadline_is_prompt_and_preserves_buffered_output() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf buffered-output; sleep 30"]);
        let started = Instant::now();

        let outcome = SupervisedCommand::new(&mut command)
            .capture_stdout(CaptureConfig::new(64, CaptureRetention::Head))
            .capture_stderr(CaptureConfig::new(64, CaptureRetention::Tail))
            .termination_grace(Duration::ZERO)
            .run_until(
                Instant::now() + Duration::from_millis(50),
                Duration::from_millis(5),
                || ControlFlow::<Stop>::Continue(()),
            )
            .unwrap();

        let SupervisedOutcome::Deadline { stdout, stderr } = outcome else {
            panic!("idle capture did not reach its deadline");
        };
        assert_eq!(stdout, b"buffered-output");
        assert!(stderr.is_empty());
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "capture deadline and join took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn inherited_file_is_child_only_and_owned() {
        let path = temp_path("fd");
        std::fs::write(&path, b"descriptor bytes").unwrap();
        let file = File::open(&path).unwrap();
        // SAFETY: `file` owns this valid descriptor for the duration of the call.
        let original_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert!(original_flags & libc::FD_CLOEXEC != 0);
        let mut command = Command::new("sh");
        command.args(["-c", "cat <&3"]);
        let child = SupervisedCommand::new(&mut command)
            .capture_stdout(
                CaptureConfig::new(1024, CaptureRetention::Head).overflow(CaptureOverflow::Error),
            )
            .inherit_file_at(&file, 3)
            .unwrap()
            .spawn()
            .unwrap();
        // SAFETY: `file` still owns this valid descriptor for the duration of the call.
        let after_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert!(after_flags & libc::FD_CLOEXEC != 0);
        drop(file);
        let outcome = child
            .wait_until(
                Instant::now() + Duration::from_secs(2),
                Duration::from_millis(5),
                || ControlFlow::<()>::Continue(()),
            )
            .unwrap();
        let SupervisedOutcome::Exited(output) = outcome else {
            panic!("descriptor reader did not exit");
        };
        assert!(output.status.success());
        assert_eq!(output.stdout, b"descriptor bytes");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn inherited_files_support_distinct_targets_and_reject_duplicates() {
        let path = temp_path("fd-validation");
        let other = temp_path("fd-validation-other");
        std::fs::write(&path, b"descriptor bytes").unwrap();
        std::fs::write(&other, b" and executable bytes").unwrap();
        let file = File::open(&path).unwrap();
        let other_file = File::open(&other).unwrap();

        let mut stdio_command = Command::new("true");
        let stdio_error = SupervisedCommand::new(&mut stdio_command)
            .inherit_file_at(&file, libc::STDOUT_FILENO)
            .unwrap_err();
        assert_eq!(stdio_error.kind(), io::ErrorKind::InvalidInput);

        let mut multiple_command = Command::new("sh");
        multiple_command.args(["-c", "cat <&3; cat <&4"]);
        let outcome = SupervisedCommand::new(&mut multiple_command)
            .capture_stdout(CaptureConfig::new(1024, CaptureRetention::Head))
            .inherit_file_at(&file, 3)
            .unwrap()
            .inherit_file_at(&other_file, 4)
            .unwrap()
            .run_until(
                Instant::now() + Duration::from_secs(2),
                Duration::from_millis(5),
                || ControlFlow::<()>::Continue(()),
            )
            .unwrap();
        let SupervisedOutcome::Exited(output) = outcome else {
            panic!("multiple descriptor reader did not exit");
        };
        assert!(output.status.success());
        assert_eq!(output.stdout, b"descriptor bytes and executable bytes");

        let mut duplicate_command = Command::new("true");
        let runner = SupervisedCommand::new(&mut duplicate_command)
            .inherit_file_at(&file, 3)
            .unwrap();
        let duplicate_error = runner.inherit_file_at(&other_file, 3).unwrap_err();
        assert_eq!(duplicate_error.kind(), io::ErrorKind::InvalidInput);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(other).unwrap();
    }

    #[test]
    fn supervised_children_always_receive_null_stdin() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "if read -r ignored; then exit 9; else exit 0; fi"])
            .stdin(Stdio::piped());
        let child = SupervisedCommand::new(&mut command).spawn().unwrap();
        let outcome = child
            .wait_until(
                Instant::now() + Duration::from_secs(2),
                Duration::from_millis(5),
                || ControlFlow::<()>::Continue(()),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            SupervisedOutcome::Exited(SupervisedOutput { status, .. }) if status.success()
        ));
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Stop {
        Cancelled,
    }

    #[test]
    fn cancellation_and_deadline_are_distinct_and_reap_the_group() {
        let marker = temp_path("child");
        let script = format!(
            "trap '' TERM; sleep 30 & child=$!; echo $$ $child > '{}'; wait",
            marker.display()
        );
        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        let child = SupervisedCommand::new(&mut command)
            .termination_grace(Duration::from_millis(20))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let outcome = child
            .wait_until(
                Instant::now() + Duration::from_secs(2),
                Duration::from_millis(5),
                || ControlFlow::Break(Stop::Cancelled),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            SupervisedOutcome::Stopped {
                reason: Stop::Cancelled,
                ..
            }
        ));
        let pids = std::fs::read_to_string(&marker).unwrap();
        for pid in pids.split_whitespace() {
            let pid: libc::pid_t = pid.parse().unwrap();
            let gone_deadline = Instant::now() + Duration::from_secs(1);
            loop {
                // SAFETY: signal 0 only probes the descendant PID recorded by this test helper.
                let gone = unsafe { libc::kill(pid, 0) };
                if gone == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    break;
                }
                assert!(Instant::now() < gone_deadline, "process {pid} still exists");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        std::fs::remove_file(marker).unwrap();

        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let child = SupervisedCommand::new(&mut command)
            .termination_grace(Duration::ZERO)
            .spawn()
            .unwrap();
        let outcome = child
            .wait_until(Instant::now(), Duration::from_millis(5), || {
                ControlFlow::<Stop>::Continue(())
            })
            .unwrap();
        assert!(matches!(outcome, SupervisedOutcome::Deadline { .. }));
    }

    #[test]
    fn hard_deadline_caps_a_large_poll_interval() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let outcome = SupervisedCommand::new(&mut command)
            .termination_grace(Duration::ZERO)
            .run_until(
                Instant::now() + Duration::from_millis(20),
                Duration::from_secs(30),
                || ControlFlow::<Stop>::Continue(()),
            )
            .unwrap();
        assert!(matches!(outcome, SupervisedOutcome::Deadline { .. }));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "large polling interval delayed the hard deadline"
        );
    }

    #[test]
    fn term_grace_applies_to_descendants_after_the_leader_exits() {
        let ready = temp_path("term-ready");
        let cooperated = temp_path("term-cooperated");
        let script = format!(
            "trap 'exit 0' TERM; (trap 'sleep 0.03; printf done > {}; exit 0' TERM; printf ready > {}; while :; do sleep 1; done) & wait",
            cooperated.display(),
            ready.display()
        );
        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        let wait_ready_until = Instant::now() + Duration::from_secs(2);
        let outcome = SupervisedCommand::new(&mut command)
            .termination_grace(Duration::from_millis(150))
            .run_until(
                Instant::now() + Duration::from_secs(3),
                Duration::from_millis(5),
                || {
                    if ready.exists() {
                        ControlFlow::Break(Stop::Cancelled)
                    } else {
                        assert!(
                            Instant::now() < wait_ready_until,
                            "descendant readiness marker was not created"
                        );
                        ControlFlow::Continue(())
                    }
                },
            )
            .unwrap();
        assert!(matches!(outcome, SupervisedOutcome::Stopped { .. }));
        assert!(
            cooperated.exists(),
            "descendant was killed before its cooperative TERM handler completed"
        );
        let _ = std::fs::remove_file(ready);
        let _ = std::fs::remove_file(cooperated);
    }

    #[test]
    fn pre_cancelled_run_does_not_spawn() {
        let marker = temp_path("not-spawned");
        let mut command = Command::new("sh");
        command.args(["-c", &format!("touch '{}'", marker.display())]);
        let outcome = SupervisedCommand::new(&mut command)
            .run_until(
                Instant::now() + Duration::from_secs(1),
                Duration::from_millis(5),
                || ControlFlow::Break(Stop::Cancelled),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            SupervisedOutcome::NotStarted {
                reason: Stop::Cancelled
            }
        ));
        assert!(!marker.exists());
    }

    #[test]
    fn exited_leader_cannot_leave_a_pipe_holding_descendant() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let started = Instant::now();
        let outcome = SupervisedCommand::new(&mut command)
            .capture_stdout(CaptureConfig::new(16, CaptureRetention::Head))
            .run_until(
                Instant::now() + Duration::from_secs(2),
                Duration::from_millis(5),
                || ControlFlow::<Stop>::Continue(()),
            )
            .unwrap();
        assert!(matches!(outcome, SupervisedOutcome::Exited(_)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn continuously_writing_escaped_session_cannot_block_capture_shutdown() {
        if Command::new("setsid")
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skip escaped-session capture test (setsid unavailable)");
            return;
        }
        let marker = temp_path("escaped-session-pid");
        let script = format!(
            "setsid sh -c 'echo $$ > \"$1\"; trap \"\" PIPE TERM; printf buffered-before-stop; while printf continuous-output; do :; done; while :; do sleep 1; done' escaped '{}' & while [ ! -s '{}' ]; do sleep 0.01; done; sleep 30",
            marker.display(),
            marker.display()
        );
        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        let started = Instant::now();
        let outcome = SupervisedCommand::new(&mut command)
            .capture_stdout(CaptureConfig::new(64, CaptureRetention::Head))
            .capture_stderr(CaptureConfig::new(64, CaptureRetention::Tail))
            .termination_grace(Duration::from_millis(20))
            .run_until(
                Instant::now() + Duration::from_millis(500),
                Duration::from_millis(5),
                || ControlFlow::<Stop>::Continue(()),
            );
        let elapsed = started.elapsed();
        let pid: libc::pid_t = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only probes the exact escaped PID recorded by this test helper.
        let escaped_was_alive = unsafe { libc::kill(pid, 0) } == 0;
        // SAFETY: the test owns cleanup of the exact escaped PID it recorded above.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        let gone_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: signal 0 only probes the exact escaped PID already killed above.
            let result = unsafe { libc::kill(pid, 0) };
            if result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                Instant::now() < gone_deadline,
                "escaped process {pid} was not reaped after exact cleanup"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let outcome = outcome.unwrap();
        let SupervisedOutcome::Deadline { stdout, .. } = outcome else {
            panic!("continuous escaped writer did not reach its deadline");
        };
        assert!(stdout.starts_with(b"buffered-before-stop"), "{stdout:?}");
        assert!(
            elapsed < Duration::from_secs(2),
            "capture join took {elapsed:?}"
        );
        assert!(
            escaped_was_alive,
            "supervisor broad-killed the escaped process"
        );
        std::fs::remove_file(marker).unwrap();
    }

    #[test]
    fn observer_panic_still_reaps_the_child() {
        let marker = temp_path("observer-panic");
        let mut command = Command::new("sh");
        command.args(["-c", &format!("echo $$ > '{}'; sleep 30", marker.display())]);
        let runner =
            SupervisedCommand::new(&mut command).termination_grace(Duration::from_millis(20));
        let marker_deadline = Instant::now() + Duration::from_secs(2);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = runner.run_until(
                Instant::now() + Duration::from_secs(2),
                Duration::from_millis(5),
                || {
                    if marker.is_file() {
                        panic!("observer failed");
                    }
                    assert!(Instant::now() < marker_deadline, "child marker not created");
                    ControlFlow::<Stop>::Continue(())
                },
            );
        }));
        assert!(result.is_err());
        let pid: libc::pid_t = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only probes the child PID recorded by this test helper.
        let gone = unsafe { libc::kill(pid, 0) };
        assert_eq!(gone, -1, "observer child {pid} still exists");
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        std::fs::remove_file(marker).unwrap();
    }
}

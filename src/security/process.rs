//! Trusted command execution with a filtered environment and bounded process/output lifecycle.
//! This is process supervision, not a hostile-code sandbox. Linux descendants that
//! deliberately create another session can escape the process group.
use std::{
    io::{self, Read},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const MAX_OUTPUT_BYTES: usize = 1_048_576;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);

pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub timeout: Duration,
}

pub struct ProcessResult {
    pub status: Option<ExitStatus>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

/// Deliberately excludes HOME, credential variables, loader hooks, Git overrides,
/// language startup hooks and unlisted caller-provided tool configuration variables.
/// PATH remains a trusted host input so existing gate program names resolve.
pub fn configure_environment(command: &mut Command) {
    let mut allowed = vec!["PATH", "TEMP", "TMP", "TMPDIR"];
    if cfg!(windows) {
        // Non-secret locations required by MSVC/vswhere discovery and developer
        // command prompts. Credentials and compiler flag injection stay absent.
        allowed.extend([
            "SystemRoot",
            "WINDIR",
            "ComSpec",
            "PATHEXT",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "VSINSTALLDIR",
            "VCINSTALLDIR",
            "VCToolsInstallDir",
            "VCToolsVersion",
            "WindowsSdkDir",
            "WindowsSDKVersion",
            "LIB",
            "LIBPATH",
            "INCLUDE",
            "VSCMD_ARG_TGT_ARCH",
            "VSCMD_ARG_VCVARS_SPECTRE",
        ]);
    }
    let values: Vec<_> = allowed
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(|value| (key, value)))
        .collect();
    command.env_clear();
    command.envs(values);
    command.env("TZ", "UTC").env("LC_ALL", "C");
}

/// Resolve a bare infrastructure executable only through absolute host PATH
/// entries. Windows' implicit current-directory search must not execute a
/// project-controlled git.exe (or replace another named gate tool).
pub fn host_executable(name: &str) -> io::Result<std::path::PathBuf> {
    if name.is_empty() || name.contains(['/', '\\', ':']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bare host program required",
        ));
    }
    let name = if cfg!(windows) && Path::new(name).extension().is_none() {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let paths = std::env::var_os("PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "host PATH unavailable"))?;
    for directory in std::env::split_paths(&paths).filter(|p| p.is_absolute()) {
        let candidate = directory.join(&name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "host executable unavailable",
    ))
}

/// Shared Git helpers retain their Command-returning API. A missing host Git
/// produces an invalid executable name rejected before spawn, never a fallback
/// to current-directory or project-controlled program search.
pub fn git_executable() -> std::ffi::OsString {
    fn resolve() -> io::Result<std::path::PathBuf> {
        let candidate = std::fs::canonicalize(host_executable("git")?)?;
        // Even an absolute PATH entry can be injected by a project launcher.
        // Authoritative reads must never execute a worker-writable Git binary.
        for parent in candidate.ancestors().skip(1) {
            match std::fs::symlink_metadata(parent.join(".git")) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "project Git executable refused",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        Ok(candidate)
    }
    resolve()
        .map(|p| p.into_os_string())
        .unwrap_or_else(|_| std::ffi::OsString::from("\0host-git-unavailable"))
}
pub fn run(spec: &CommandSpec, cwd: &Path) -> io::Result<ProcessResult> {
    if spec.timeout.is_zero() || spec.timeout > Duration::from_secs(3600) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid command timeout",
        ));
    }
    // Batch files cause an implicit cmd.exe invocation in std::process on Windows.
    // Only explicit interpreter programs with structured arguments are accepted.
    if cfg!(windows)
        && Path::new(&spec.program)
            .extension()
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
            })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "implicit batch interpreter refused",
        ));
    }
    let program = if !spec.program.contains(['/', '\\', ':']) {
        host_executable(&spec.program)?
    } else {
        std::path::PathBuf::from(&spec.program)
    };
    let mut command = Command::new(program);
    command
        .args(&spec.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_environment(&mut command);
    let containment = platform::Containment::prepare(&mut command)?;
    let child = command.spawn()?;
    let mut guard = ChildGuard {
        child,
        containment,
        cleaned: false,
        terminated: false,
    };
    guard.containment.attach(&guard.child)?;
    let mut stdout = Capture::new(
        guard
            .child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout pipe"))?,
    )?;
    let mut stderr = Capture::new(
        guard
            .child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing stderr pipe"))?,
    )?;
    let start = Instant::now();
    let mut timed_out = false;
    loop {
        stdout.drain()?;
        stderr.drain()?;
        let parent_exited = guard.containment.parent_exited(&mut guard.child)?;
        if start.elapsed() >= spec.timeout {
            // Deadline wins even if a process exits between the last poll and
            // observing the deadline; a timeout can never become success.
            timed_out = true;
            break;
        }
        if parent_exited {
            break;
        }
        thread::sleep(POLL);
    }
    // Clean descendants on ordinary parent exit too. A successful parent must
    // not leave a background worker holding pipes or touching the checkout.
    let status = guard.cleanup()?;
    let drain_start = Instant::now();
    while !(stdout.eof && stderr.eof) {
        stdout.drain()?;
        stderr.drain()?;
        if drain_start.elapsed() >= CLEANUP_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "output pipe cleanup deadline exceeded",
            ));
        }
        if !(stdout.eof && stderr.eof) {
            thread::sleep(POLL);
        }
    }
    Ok(ProcessResult {
        status,
        timed_out,
        truncated: stdout.truncated || stderr.truncated,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

struct ChildGuard {
    child: Child,
    containment: platform::Containment,
    cleaned: bool,
    terminated: bool,
}
impl ChildGuard {
    fn cleanup(&mut self) -> io::Result<Option<ExitStatus>> {
        if !self.terminated {
            self.containment.terminate(&mut self.child)?;
            self.terminated = true;
        }
        let start = Instant::now();
        loop {
            let status = self.child.try_wait()?;
            if status.is_some() && self.containment.empty()? {
                self.cleaned = true;
                return Ok(status);
            }
            if start.elapsed() >= CLEANUP_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process tree cleanup deadline exceeded",
                ));
            }
            thread::sleep(POLL);
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}

struct Capture<R> {
    reader: R,
    bytes: Vec<u8>,
    eof: bool,
    truncated: bool,
}
impl<R: platform::PipeReader> Capture<R> {
    fn new(reader: R) -> io::Result<Self> {
        reader.prepare_pipe()?;
        Ok(Self {
            reader,
            bytes: Vec::new(),
            eof: false,
            truncated: false,
        })
    }
    fn drain(&mut self) -> io::Result<()> {
        if self.eof {
            return Ok(());
        }
        let mut buffer = [0; 8192];
        // Fair bounded work per poll even when the child floods both streams.
        for _ in 0..8 {
            match self.reader.read_ready(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(count) => {
                    let retained = count.min(MAX_OUTPUT_BYTES.saturating_sub(self.bytes.len()));
                    self.bytes.extend_from_slice(&buffer[..retained]);
                    self.truncated |= retained < count;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::os::{fd::AsRawFd, unix::process::CommandExt};

    pub(super) struct Containment {
        group: Option<i32>,
    }
    impl Containment {
        pub(super) fn prepare(command: &mut Command) -> io::Result<Self> {
            command.process_group(0);
            Ok(Self { group: None })
        }
        pub(super) fn attach(&mut self, child: &Child) -> io::Result<()> {
            self.group = Some(
                i32::try_from(child.id()).map_err(|_| io::Error::other("invalid process id"))?,
            );
            Ok(())
        }
        pub(super) fn parent_exited(&self, child: &mut Child) -> io::Result<bool> {
            // WNOWAIT observes exit without reaping the leader. Its PID pins the
            // process group identity until SIGKILL has targeted the whole group.
            // SAFETY: waitid initializes this zeroed C record; P_PID names our
            // child, and WNOHANG makes the observation nonblocking.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { info.si_pid() } != 0)
        }
        pub(super) fn terminate(&self, child: &mut Child) -> io::Result<()> {
            if let Some(group) = self.group {
                // SAFETY: a negative PID addresses the child's dedicated process
                // group. SIGKILL cannot be ignored by cooperative descendants.
                if unsafe { libc::kill(-group, libc::SIGKILL) } != 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error);
                    }
                }
            } else if child.try_wait()?.is_none() {
                child.kill()?;
            }
            Ok(())
        }
        pub(super) fn empty(&self) -> io::Result<bool> {
            let Some(group) = self.group else {
                return Ok(true);
            };
            // Grandchildren cannot be waited by this process. Check every live
            // /proc group member; zombies have already released their resources
            // and cannot execute. Missing/restricted procfs fails closed.
            for entry in std::fs::read_dir("/proc")? {
                let entry = entry?;
                if !entry
                    .file_name()
                    .as_encoded_bytes()
                    .iter()
                    .all(u8::is_ascii_digit)
                {
                    continue;
                }
                let stat = match std::fs::read_to_string(entry.path().join("stat")) {
                    Ok(stat) => stat,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                let (_, fields) = stat
                    .rsplit_once(") ")
                    .ok_or_else(|| io::Error::other("invalid proc process record"))?;
                let mut fields = fields.split_whitespace();
                let state = fields
                    .next()
                    .ok_or_else(|| io::Error::other("missing proc process state"))?;
                let process_group: i32 = fields
                    .nth(1)
                    .ok_or_else(|| io::Error::other("missing proc process group"))?
                    .parse()
                    .map_err(|_| io::Error::other("invalid proc process group"))?;
                if process_group == group && !matches!(state, "Z" | "X") {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
    pub(super) trait PipeReader: Read {
        fn prepare_pipe(&self) -> io::Result<()>;
        fn read_ready(&mut self, buffer: &mut [u8]) -> io::Result<usize>;
    }
    impl<T: Read + AsRawFd> PipeReader for T {
        fn prepare_pipe(&self) -> io::Result<()> {
            // SAFETY: this is an owned, live pipe descriptor.
            let flags = unsafe { libc::fcntl(self.as_raw_fd(), libc::F_GETFL) };
            if flags < 0
                || unsafe { libc::fcntl(self.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                    < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        fn read_ready(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.read(buffer)
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        ffi::c_void,
        mem,
        os::windows::{io::AsRawHandle, process::CommandExt},
        ptr,
    };
    type Handle = *mut c_void;
    const INVALID_HANDLE: Handle = -1isize as Handle;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const SNAP_THREAD: u32 = 0x0000_0004;
    const THREAD_SUSPEND_RESUME: u32 = 0x0000_0002;
    const ERROR_BROKEN_PIPE: i32 = 109;
    const ERROR_NO_DATA: i32 = 232;
    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        process_time: i64,
        job_time: i64,
        flags: u32,
        min_working_set: usize,
        max_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_ops: u64,
        write_ops: u64,
        other_ops: u64,
        read_bytes: u64,
        write_bytes: u64,
        other_bytes: u64,
    }
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io: IoCounters,
        process_memory: usize,
        job_memory: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Accounting {
        user_time: i64,
        kernel_time: i64,
        period_user_time: i64,
        period_kernel_time: i64,
        page_faults: u32,
        total_processes: u32,
        active_processes: u32,
        terminated_processes: u32,
    }
    #[repr(C)]
    struct ThreadEntry {
        size: u32,
        usage: u32,
        id: u32,
        owner: u32,
        priority: i32,
        delta_priority: i32,
        flags: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(
            job: Handle,
            class: i32,
            data: *const c_void,
            length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
        fn QueryInformationJobObject(
            job: Handle,
            class: i32,
            data: *mut c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
        fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
        fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
        fn OpenThread(access: u32, inherit: i32, thread_id: u32) -> Handle;
        fn ResumeThread(thread: Handle) -> u32;
        fn PeekNamedPipe(
            pipe: Handle,
            buffer: *mut c_void,
            buffer_size: u32,
            read: *mut u32,
            available: *mut u32,
            remaining: *mut u32,
        ) -> i32;
    }
    struct OwnedHandle(Handle);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    pub(super) struct Containment {
        job: OwnedHandle,
        attached: bool,
    }
    impl Containment {
        pub(super) fn prepare(command: &mut Command) -> io::Result<Self> {
            // The job handle is unnamed and non-inheritable. Its last close
            // kills descendants even during Rust unwinding or host exit.
            let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = OwnedHandle(job);
            let limits = ExtendedLimits {
                basic: BasicLimits {
                    flags: KILL_ON_JOB_CLOSE,
                    ..Default::default()
                },
                ..Default::default()
            };
            if unsafe {
                SetInformationJobObject(
                    job.0,
                    9,
                    ptr::from_ref(&limits).cast(),
                    mem::size_of_val(&limits) as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            // No target code can execute or spawn children before assignment.
            command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
            Ok(Self {
                job,
                attached: false,
            })
        }
        pub(super) fn attach(&mut self, child: &Child) -> io::Result<()> {
            if unsafe { AssignProcessToJobObject(self.job.0, child.as_raw_handle()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            self.attached = true;
            // std::process does not expose its initial thread handle. The sole
            // initial thread remains suspended while documented Toolhelp APIs
            // locate it. If discovery/resume fails the guard terminates the job.
            let snapshot = unsafe { CreateToolhelp32Snapshot(SNAP_THREAD, 0) };
            if snapshot == INVALID_HANDLE {
                return Err(io::Error::last_os_error());
            }
            let snapshot = OwnedHandle(snapshot);
            let mut entry = ThreadEntry {
                size: mem::size_of::<ThreadEntry>() as u32,
                usage: 0,
                id: 0,
                owner: 0,
                priority: 0,
                delta_priority: 0,
                flags: 0,
            };
            let mut found = unsafe { Thread32First(snapshot.0, &mut entry) } != 0;
            while found {
                if entry.owner == child.id() {
                    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.id) };
                    if thread.is_null() {
                        return Err(io::Error::last_os_error());
                    }
                    let thread = OwnedHandle(thread);
                    let previous = unsafe { ResumeThread(thread.0) };
                    if previous == u32::MAX {
                        return Err(io::Error::last_os_error());
                    }
                    if previous != 1 {
                        return Err(io::Error::other("unexpected initial suspend count"));
                    }
                    return Ok(());
                }
                entry.size = mem::size_of::<ThreadEntry>() as u32;
                found = unsafe { Thread32Next(snapshot.0, &mut entry) } != 0;
            }
            Err(io::Error::other("suspended initial thread not found"))
        }
        pub(super) fn parent_exited(&self, child: &mut Child) -> io::Result<bool> {
            child.try_wait().map(|status| status.is_some())
        }
        pub(super) fn terminate(&self, child: &mut Child) -> io::Result<()> {
            if self.attached {
                if unsafe { TerminateJobObject(self.job.0, 1) } == 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if child.try_wait()?.is_none() {
                child.kill()?;
            }
            Ok(())
        }
        pub(super) fn empty(&self) -> io::Result<bool> {
            let mut accounting = Accounting::default();
            if unsafe {
                QueryInformationJobObject(
                    self.job.0,
                    1,
                    ptr::from_mut(&mut accounting).cast(),
                    mem::size_of::<Accounting>() as u32,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(accounting.active_processes == 0)
        }
    }
    pub(super) trait PipeReader: Read {
        fn prepare_pipe(&self) -> io::Result<()>;
        fn read_ready(&mut self, buffer: &mut [u8]) -> io::Result<usize>;
    }
    impl<T: Read + AsRawHandle> PipeReader for T {
        fn prepare_pipe(&self) -> io::Result<()> {
            Ok(())
        }
        fn read_ready(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let mut available = 0;
            if unsafe {
                PeekNamedPipe(
                    self.as_raw_handle(),
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut available,
                    ptr::null_mut(),
                )
            } == 0
            {
                let error = io::Error::last_os_error();
                if matches!(
                    error.raw_os_error(),
                    Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA)
                ) {
                    return Ok(0);
                }
                return Err(error);
            }
            if available == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let count = (available as usize).min(buffer.len());
            self.read(&mut buffer[..count])
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use super::*;
    pub(super) struct Containment;
    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "process-tree supervision unsupported on this platform",
        )
    }
    impl Containment {
        pub(super) fn prepare(_: &mut Command) -> io::Result<Self> {
            Err(unsupported())
        }
        pub(super) fn attach(&mut self, _: &Child) -> io::Result<()> {
            Err(unsupported())
        }
        pub(super) fn parent_exited(&self, _: &mut Child) -> io::Result<bool> {
            Err(unsupported())
        }
        pub(super) fn terminate(&self, _: &mut Child) -> io::Result<()> {
            Err(unsupported())
        }
        pub(super) fn empty(&self) -> io::Result<bool> {
            Err(unsupported())
        }
    }
    pub(super) trait PipeReader: Read {
        fn prepare_pipe(&self) -> io::Result<()>;
        fn read_ready(&mut self, buffer: &mut [u8]) -> io::Result<usize>;
    }
    impl<T: Read> PipeReader for T {
        fn prepare_pipe(&self) -> io::Result<()> {
            Err(unsupported())
        }
        fn read_ready(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(unsupported())
        }
    }
}

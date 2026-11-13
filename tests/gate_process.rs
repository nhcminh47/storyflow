#![cfg(any(target_os = "linux", windows))]
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
use storyflow::security::process::{CommandSpec, MAX_OUTPUT_BYTES, configure_environment, run};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new(mode: &str) -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/gate-process-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("mode"), mode).unwrap();
        Self(path)
    }
    fn spec(&self, timeout: Duration) -> CommandSpec {
        CommandSpec {
            program: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: fixture_args(),
            timeout,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture_args() -> Vec<String> {
    [
        "--exact",
        "subprocess_fixture",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]
    .map(str::to_owned)
    .into()
}

// An explicitly selected test-harness process is a portable native fixture. It
// requires no shell/interpreter and the role comes from its temporary cwd.
#[test]
#[ignore]
#[allow(clippy::zombie_processes)] // Intentional orphan fixture: the tested supervisor owns cleanup.
fn subprocess_fixture() {
    let cwd = std::env::current_dir().unwrap();
    let mode = fs::read_to_string(cwd.join("mode")).unwrap();
    match mode.as_str() {
        "timeout" | "normal" | "timeout-null" => {
            let middle = cwd.join("middle");
            let leaf = middle.join("leaf");
            fs::create_dir_all(&leaf).unwrap();
            fs::write(middle.join("mode"), "middle").unwrap();
            fs::write(leaf.join("mode"), "leaf").unwrap();
            let child = Command::new(std::env::current_exe().unwrap())
                .args(fixture_args())
                .current_dir(&middle)
                .stdin(Stdio::null())
                .stdout(if mode == "timeout-null" {
                    Stdio::null()
                } else {
                    Stdio::inherit()
                })
                .stderr(if mode == "timeout-null" {
                    Stdio::null()
                } else {
                    Stdio::inherit()
                })
                .spawn()
                .unwrap();
            fs::write(cwd.join("middle.pid"), child.id().to_string()).unwrap();
            let start = Instant::now();
            while !leaf.join("ready").exists() {
                assert!(start.elapsed() < Duration::from_secs(5));
                thread::sleep(Duration::from_millis(5));
            }
            fs::write(cwd.join("ready"), "ready").unwrap();
            if mode.starts_with("timeout") {
                thread::sleep(Duration::from_secs(10));
            }
            // No wait: the supervisor must clean the child and grandchild even
            // when the initial process succeeds.
        }
        "middle" => {
            let leaf = cwd.join("leaf");
            let child = Command::new(std::env::current_exe().unwrap())
                .args(fixture_args())
                .current_dir(&leaf)
                .stdin(Stdio::null())
                .stdout(if mode == "timeout-null" {
                    Stdio::null()
                } else {
                    Stdio::inherit()
                })
                .stderr(if mode == "timeout-null" {
                    Stdio::null()
                } else {
                    Stdio::inherit()
                })
                .spawn()
                .unwrap();
            fs::write(cwd.join("leaf.pid"), child.id().to_string()).unwrap();
            thread::sleep(Duration::from_secs(10));
        }
        "leaf" => {
            fs::write(cwd.join("ready"), "ready").unwrap();
            thread::sleep(Duration::from_secs(10));
            fs::write(cwd.join("orphan"), "unsafe").unwrap();
        }
        "output" => {
            let output = vec![b'x'; MAX_OUTPUT_BYTES * 2];
            io::stdout().lock().write_all(&output).unwrap();
            io::stderr().lock().write_all(&output).unwrap();
        }
        "environment" => {
            for (key, _) in std::env::vars_os() {
                let key = key.to_string_lossy().to_ascii_uppercase();
                assert!(
                    matches!(
                        key.as_str(),
                        "PATH"
                            | "TEMP"
                            | "TMP"
                            | "TMPDIR"
                            | "SYSTEMROOT"
                            | "WINDIR"
                            | "COMSPEC"
                            | "PATHEXT"
                            | "SYSTEMDRIVE"
                            | "PROGRAMDATA"
                            | "ALLUSERSPROFILE"
                            | "USERPROFILE"
                            | "LOCALAPPDATA"
                            | "APPDATA"
                            | "TZ"
                            | "LC_ALL"
                            | "PROGRAMFILES"
                            | "PROGRAMFILES(X86)"
                            | "PROGRAMW6432"
                            | "VSINSTALLDIR"
                            | "VCINSTALLDIR"
                            | "VCTOOLSINSTALLDIR"
                            | "VCTOOLSVERSION"
                            | "WINDOWSSDKDIR"
                            | "WINDOWSSDKVERSION"
                            | "LIB"
                            | "LIBPATH"
                            | "INCLUDE"
                            | "VSCMD_ARG_TGT_ARCH"
                            | "VSCMD_ARG_VCVARS_SPECTRE"
                    ),
                    "unexpected inherited key {key}"
                );
            }
            assert_eq!(std::env::var("TZ").unwrap(), "UTC");
            assert_eq!(std::env::var("LC_ALL").unwrap(), "C");
            assert!(std::env::var_os("STORYFLOW_TEST_SECRET").is_none());
        }
        _ => panic!("unknown subprocess fixture"),
    }
}

fn assert_descendants_stopped(fixture: &Fixture) {
    for path in [
        fixture.0.join("middle.pid"),
        fixture.0.join("middle/leaf.pid"),
    ] {
        let pid: u32 = fs::read_to_string(path).unwrap().parse().unwrap();
        let start = Instant::now();
        while process_alive(pid) {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "descendant {pid} remains alive"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(!fixture.0.join("middle/leaf/orphan").exists());
}
#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    // Orphaned zombies await init's reaper but cannot execute or hold pipes.
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .is_some_and(|(_, rest)| !rest.starts_with('Z'))
}
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    let handle = unsafe { OpenProcess(0x0010_0000, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let waiting = unsafe { WaitForSingleObject(handle, 0) } == 258;
    unsafe {
        CloseHandle(handle);
    }
    waiting
}

#[test]
fn timeout_kills_child_and_grandchild() {
    let fixture = Fixture::new("timeout");
    let start = Instant::now();
    let result = run(&fixture.spec(Duration::from_secs(2)), &fixture.0).unwrap();
    assert!(result.timed_out);
    assert!(!result.status.unwrap().success());
    assert!(start.elapsed() < Duration::from_secs(7));
    assert!(
        fixture.0.join("ready").exists(),
        "fixture did not launch its descendants"
    );
    assert_descendants_stopped(&fixture);
}
#[test]
fn successful_parent_cannot_leave_background_descendants() {
    let fixture = Fixture::new("normal");
    let result = run(&fixture.spec(Duration::from_secs(10)), &fixture.0).unwrap();
    assert!(!result.timed_out);
    assert!(result.status.unwrap().success());
    assert_descendants_stopped(&fixture);
}
#[test]
fn output_is_bounded_while_pipes_keep_draining() {
    let fixture = Fixture::new("output");
    let result = run(&fixture.spec(Duration::from_secs(15)), &fixture.0).unwrap();
    assert!(!result.timed_out);
    assert!(result.status.unwrap().success());
    assert!(result.truncated);
    assert_eq!(result.stdout.len(), MAX_OUTPUT_BYTES);
    assert_eq!(result.stderr.len(), MAX_OUTPUT_BYTES);
}
#[test]
fn environment_is_allowlisted() {
    let fixture = Fixture::new("environment");
    // No global environment mutation is needed; the same production filter is
    // exercised directly with an explicit secret on a command builder.
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env("STORYFLOW_TEST_SECRET", "must-not-reach-child");
    configure_environment(&mut command);
    let output = command
        .args(fixture_args())
        .current_dir(&fixture.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result = run(&fixture.spec(Duration::from_secs(10)), &fixture.0).unwrap();
    assert!(
        result.status.unwrap().success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    assert!(!String::from_utf8_lossy(&result.stdout).contains("must-not-reach-child"));
}
#[test]
fn invalid_timeouts_and_missing_programs_never_succeed() {
    let fixture = Fixture::new("environment");
    let mut spec = fixture.spec(Duration::ZERO);
    assert_eq!(
        run(&spec, &fixture.0).err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
    spec.timeout = Duration::from_secs(1);
    spec.program = fixture
        .0
        .join("absent-native-program")
        .to_string_lossy()
        .into_owned();
    assert!(run(&spec, &fixture.0).is_err());
}
#[cfg(windows)]
#[test]
fn batch_programs_are_refused_without_implicit_shell() {
    let fixture = Fixture::new("environment");
    let marker = fixture.0.join("marker");
    let program = fixture.0.join("probe.cmd");
    fs::write(&program, format!("@echo unsafe>\"{}\"", marker.display())).unwrap();
    let spec = CommandSpec {
        program: program.to_string_lossy().into_owned(),
        args: vec![],
        timeout: Duration::from_secs(1),
    };
    assert_eq!(
        run(&spec, &fixture.0).err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(!marker.exists());
}

#[test]
fn timeout_confirms_descendants_with_closed_output_pipes_stopped() {
    let fixture = Fixture::new("timeout-null");
    let result = run(&fixture.spec(Duration::from_secs(2)), &fixture.0).unwrap();
    assert!(result.timed_out);
    assert!(fixture.0.join("ready").exists());
    assert_descendants_stopped(&fixture);
}

#[cfg(windows)]
#[test]
fn msvc_installation_discovery_survives_environment_filtering() {
    let Some(base) = std::env::var_os("ProgramFiles(x86)") else {
        return;
    };
    let probe = PathBuf::from(base).join("Microsoft Visual Studio/Installer/vswhere.exe");
    if !probe.is_file() {
        return;
    } // GNU-only local hosts have no MSVC installer.
    let args: Vec<String> = [
        "-latest",
        "-products",
        "*",
        "-requires",
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        "-property",
        "installationPath",
    ]
    .map(str::to_owned)
    .into();
    let original = Command::new(&probe).args(&args).output().unwrap();
    assert!(original.status.success());
    if original.stdout.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    let fixture = Fixture::new("environment");
    let filtered = run(
        &CommandSpec {
            program: probe.to_string_lossy().into_owned(),
            args,
            timeout: Duration::from_secs(15),
        },
        &fixture.0,
    )
    .unwrap();
    assert!(
        filtered.status.unwrap().success(),
        "{}",
        String::from_utf8_lossy(&filtered.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&filtered.stdout).trim(),
        String::from_utf8_lossy(&original.stdout).trim(),
        "filtered MSVC discovery lost its installation"
    );
}

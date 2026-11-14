#![cfg(target_os = "linux")]
//! Deterministic regression for journal-lock ownership across fork, before exec.
use std::{fs, path::PathBuf};
use storyflow::tasks::{RunContext, TaskJournal, TaskStage};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn dropping_task_journal_releases_lock_while_child_inherits_descriptor() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/lock-inheritance-tests")
        .join(format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    fs::create_dir_all(root.join(".storyflow/lock-001/events")).unwrap();
    let fixture = Fixture(root);
    let context = RunContext {
        story: "lock-001".into(),
        base: "1".repeat(40),
        story_revision: 0,
        stage: TaskStage::Plan,
        blocked: false,
        scope: vec!["app".into()],
        protected: vec![],
    };
    let journal = TaskJournal::open(&fixture.0, context.clone()).unwrap();
    let control_path = fixture.0.join("native.lock");
    let control = fs::File::create(&control_path).unwrap();
    control.try_lock().unwrap();
    let mut pipe = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    // Only async-signal-safe libc calls execute in the child; in particular it
    // never runs Rust destructors that could release the inherited lock.
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe {
            libc::close(pipe[1]);
            libc::alarm(5);
            let mut byte = 0u8;
            while libc::read(pipe[0], std::ptr::from_mut(&mut byte).cast(), 1) < 0 {
                if *libc::__errno_location() != libc::EINTR {
                    libc::_exit(1);
                }
            }
            libc::_exit(0);
        }
    }
    if child < 0 {
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        panic!("fork failed: {}", std::io::Error::last_os_error());
    }
    unsafe {
        libc::close(pipe[0]);
    }
    drop(journal);
    drop(control);
    let control_contender = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&control_path)
        .unwrap();
    let inherited_control_still_locked = matches!(
        control_contender.try_lock(),
        Err(fs::TryLockError::WouldBlock)
    );
    // Capture the result before letting the child close its inherited file
    // description. Close-on-exec alone cannot satisfy this ownership contract.
    let reopened = TaskJournal::open(&fixture.0, context);
    let mut status = 0;
    let waited;
    unsafe {
        let byte = 1u8;
        libc::write(pipe[1], std::ptr::from_ref(&byte).cast(), 1);
        libc::close(pipe[1]);
        waited = loop {
            let result = libc::waitpid(child, &mut status, 0);
            if result >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break result;
            }
        };
    }
    assert_eq!(waited, child, "could not reap inheritance fixture");
    assert!(
        inherited_control_still_locked,
        "native close-only control did not retain the inherited lock"
    );
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "inheritance fixture did not finish normally"
    );
    assert!(
        reopened.is_ok(),
        "journal ownership survived drop: {:?}",
        reopened.err()
    );
}

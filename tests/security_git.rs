//! The invoking environment cannot erase trusted Git safety overrides.
#[allow(dead_code)]
#[path = "../src/model.rs"]
mod model;
#[allow(dead_code, unused_imports)]
#[path = "../src/repo.rs"]
mod repo;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[test]
fn git_environment_fixture() {
    if !std::env::args().any(|a| a == "security-git-env-fixture") {
        return;
    }
    let command = repo::git(std::path::Path::new("."));
    let overrides: std::collections::BTreeMap<_, _> = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    for key in [
        "GIT_CONFIG_NOSYSTEM",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_NO_LAZY_FETCH",
    ] {
        assert_eq!(overrides.get(key).and_then(Option::as_deref), Some("1"));
    }
    assert_eq!(
        overrides
            .get("GIT_CONFIG_GLOBAL")
            .and_then(Option::as_deref),
        Some(if cfg!(windows) { "NUL" } else { "/dev/null" })
    );
    assert_eq!(
        overrides
            .get("GIT_TERMINAL_PROMPT")
            .and_then(Option::as_deref),
        Some("0")
    );
    assert!(overrides.get("GIT_DIR").is_none_or(Option::is_none));
}
#[test]
fn ambient_git_keys_cannot_remove_trusted_overrides() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "git_environment_fixture",
            "--nocapture",
            "--skip",
            "security-git-env-fixture",
        ])
        .env("GIT_CONFIG_NOSYSTEM", "0")
        .env("GIT_CONFIG_GLOBAL", "PRIVATE_CONFIG_PATH_SENTINEL")
        .env("GIT_NO_REPLACE_OBJECTS", "0")
        .env("GIT_NO_LAZY_FETCH", "0")
        .env("GIT_TERMINAL_PROMPT", "1")
        .env("GIT_DIR", "PRIVATE_GIT_REDIRECT_SENTINEL")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn project_directory_cannot_shadow_authoritative_git() {
    let root = std::env::temp_dir().join(format!("sf-git-shadow-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let shadow = root.join(if cfg!(windows) { "git.exe" } else { "git" });
    std::fs::copy(std::env::current_exe().unwrap(), &shadow).unwrap();
    let mut command = repo::git(&root);
    assert!(std::path::Path::new(command.get_program()).is_absolute());
    assert_ne!(command.get_program(), shadow.as_os_str());
    let output = command.arg("--version").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("git version "));
    std::fs::remove_file(shadow).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn project_path_git_fixture() {
    if !std::env::args().any(|a| a == "security-git-path-fixture") {
        return;
    }
    let result = repo::git(&std::env::current_dir().unwrap())
        .arg("--version")
        .output();
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn absolute_project_path_entries_cannot_authorize_git_execution() {
    let root = std::env::temp_dir().join(format!("sf-git-path-{}", std::process::id()));
    std::fs::create_dir_all(root.join("tools")).unwrap();
    std::fs::write(root.join(".git"), "project marker").unwrap();
    let shadow = root
        .join("tools")
        .join(if cfg!(windows) { "git.exe" } else { "git" });
    std::fs::copy(std::env::current_exe().unwrap(), &shadow).unwrap();
    let mut entries = vec![root.join("tools")];
    entries.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "project_path_git_fixture",
            "--nocapture",
            "--skip",
            "security-git-path-fixture",
        ])
        .env("PATH", std::env::join_paths(entries).unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_file(shadow).unwrap();
    std::fs::remove_dir(root.join("tools")).unwrap();
    std::fs::remove_file(root.join(".git")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

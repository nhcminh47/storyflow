//! Configuration CLI: explicit bootstrap and read-only effective resolution.
use crate::{repo, run_config};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    env, fs,
    io::Write,
    path::{Component, Path, PathBuf},
};
use storyflow::config::{
    self, Diagnostic, EffectiveConfig, GitBaseline, HostTrust, Origin, ResolveInput, Scope,
    UserEnvironment, UserLocation, UserPlatform,
};

const HELP: &str = "storyflow init --scope user|workspace|repo [--path DIRECTORY]\nstoryflow config inspect --effective\nstoryflow config validate\n\nBootstrap creates only a configuration skeleton; existing files are preserved.\nRepository skeletons need approved agents and mandatory checks before validation.\nInspect/validate resolve the current repository HEAD, without running checks or changing a run.\nExternal scopes require STORYFLOW_HOST_CONTEXT and STORYFLOW_HOST_CONTEXT_ROOT.\nInspection redacts model hints, command programs/arguments, and contract content.\nExit codes: 0 success, 2 invalid input, unavailable input, or invalid configuration.";

#[derive(Debug, Serialize)]
struct Failure {
    code: &'static str,
    message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<Origin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    related: Option<Box<Origin>>,
}
type Result<T> = std::result::Result<T, Failure>;
impl Failure {
    fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            origin: None,
            related: None,
        }
    }
}
impl From<Diagnostic> for Failure {
    fn from(error: Diagnostic) -> Self {
        Self {
            code: error.code,
            message: error.message,
            origin: Some(error.origin),
            related: error.related,
        }
    }
}

/// Handle these forms before the legacy parser can open a story journal.
pub fn dispatch(args: &[String]) -> Option<u8> {
    let command = args.first()?.as_str();
    if command != "config" && !(command == "init" && args[1..].iter().any(|a| a.starts_with('-'))) {
        return None;
    }
    let tail: Vec<_> = args[1..].iter().map(String::as_str).collect();
    let help = matches!(tail.as_slice(), ["--help" | "-h"])
        || (command == "config"
            && matches!(tail.as_slice(), ["inspect" | "validate", "--help" | "-h"]))
        || (command == "init"
            && matches!(
                tail.as_slice(),
                ["--scope", "user" | "workspace" | "repo", "--help" | "-h"]
            ));
    if help {
        println!("{HELP}");
        return Some(0);
    }
    let result = match command {
        "init" => parse_bootstrap(&tail),
        "config" => match tail.as_slice() {
            ["inspect", "--effective"] => resolve().map(|effective| json!({"schema":1,"command":"config.inspect","configuration":inspection(&effective)})),
            ["validate"] => resolve().map(|effective| json!({"schema":1,"command":"config.validate","valid":true,"scopes":effective.scopes})),
            _ => Err(Failure::new("invalid_arguments", "expected config inspect --effective or config validate; use --help")),
        },
        _ => unreachable!(),
    };
    match result {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).expect("JSON output")
            );
            Some(0)
        }
        Err(error) => {
            println!("{}", json!({"schema":1,"error":error}));
            Some(2)
        }
    }
}

fn parse_bootstrap(args: &[&str]) -> Result<Value> {
    let mut scope = None;
    let mut path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--scope" if scope.is_none() => {
                scope = Some(match args.get(index + 1).copied() {
                    Some("user") => Scope::User,
                    Some("workspace") => Scope::Workspace,
                    Some("repo") => Scope::Repo,
                    _ => {
                        return Err(Failure::new(
                            "invalid_arguments",
                            "--scope requires user, workspace, or repo",
                        ));
                    }
                });
            }
            "--path" if path.is_none() => {
                let value = args
                    .get(index + 1)
                    .filter(|s| !s.is_empty() && !s.starts_with('-'))
                    .ok_or_else(|| {
                        Failure::new("invalid_arguments", "--path requires a directory")
                    })?;
                path = Some(PathBuf::from(value));
            }
            _ => {
                return Err(Failure::new(
                    "invalid_arguments",
                    "scope bootstrap accepts each flag once and no positional story arguments",
                ));
            }
        }
        index += 2;
    }
    let scope = scope
        .ok_or_else(|| Failure::new("invalid_arguments", "scope bootstrap requires --scope"))?;
    let directory = match path {
        Some(path) => absolute_directory(&path)?,
        None => match scope {
            Scope::User => {
                let location = UserLocation::from_environment(
                    if cfg!(windows) {
                        UserPlatform::Windows
                    } else {
                        UserPlatform::Unix
                    },
                    &UserEnvironment {
                        appdata: env::var_os("APPDATA").map(PathBuf::from),
                        xdg_config_home: env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
                        home: env::var_os("HOME").map(PathBuf::from),
                    },
                )?;
                absolute_directory(&location.base.join(location.directory))?
            }
            Scope::Workspace => absolute_directory(&env::current_dir().map_err(|_| {
                Failure::new("unavailable_directory", "current directory is unavailable")
            })?)?,
            Scope::Repo => repo::root().map_err(|_| {
                Failure::new(
                    "invalid_repository",
                    "repository bootstrap without --path requires a Git checkout",
                )
            })?,
        },
    };
    if scope == Scope::User {
        reject_checkout(&directory)?;
    }
    let (name, document) = match scope {
        Scope::User => (
            "config.json",
            json!({"version":2,"scope":"user","workspaces":[]}),
        ),
        Scope::Workspace => (
            "storyflow.workspace.json",
            json!({"version":2,"scope":"workspace","members":[]}),
        ),
        Scope::Repo => (
            "storyflow.json",
            json!({"version":2,"scope":"repo","agents":[],"policy":{"protected":[],"checks":[]}}),
        ),
    };
    let mut bytes = serde_json::to_vec_pretty(&document).expect("template JSON");
    bytes.push(b'\n');
    let created = create_configuration(&directory, name, &bytes)?;
    Ok(
        json!({"schema":1,"command":"init.scope","scope":scope,"path":directory.join(name),"created":created,"status":if created {"created"} else {"unchanged"},"requires_configuration":scope == Scope::Repo,"trust_granted":false}),
    )
}

fn absolute_directory(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Failure::new(
            "invalid_path",
            "bootstrap directory cannot be empty or contain parent traversal",
        ));
    }
    #[cfg(windows)]
    if (path.to_string_lossy().starts_with("\\\\") || path.to_string_lossy().starts_with("//")) || ((path.has_root() || path.components().any(|c| matches!(c, Component::Prefix(_)))) && !path.is_absolute()) || path.components().any(|c| matches!(c, Component::Prefix(p) if !matches!(p.kind(), std::path::Prefix::Disk(_)))) {
        return Err(Failure::new("invalid_path", "bootstrap requires a native local directory path"));
    }
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()
            .map_err(|_| Failure::new("unavailable_directory", "current directory is unavailable"))?
            .join(path)
    };
    for part in path.components() {
        if let Component::Normal(part) = part {
            config::validate_relative_path(
                part.to_str()
                    .ok_or_else(|| Failure::new("invalid_path", "directory is not UTF-8"))?,
                &Origin::new(Scope::Repo, "bootstrap", "path"),
            )?;
        }
    }
    Ok(path)
}

fn reject_checkout(path: &Path) -> Result<()> {
    for parent in path.ancestors() {
        match fs::symlink_metadata(parent.join(".git")) {
            Ok(_) => {
                return Err(Failure::new(
                    "untrusted_user_location",
                    "user bootstrap must be outside every Git checkout",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => {
                return Err(Failure::new(
                    "untrusted_user_location",
                    "cannot establish user location outside a checkout",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn create_configuration(directory: &Path, name: &str, bytes: &[u8]) -> Result<bool> {
    use std::{
        fs::OpenOptions,
        os::windows::fs::{MetadataExt, OpenOptionsExt},
    };
    let failure = || {
        Failure::new(
            "unsafe_bootstrap_path",
            "bootstrap requires physical directories and a regular target; links and reparse points are refused",
        )
    };
    let mut held = Vec::new();
    let mut current = PathBuf::new();
    for component in directory.components() {
        current.push(component);
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let open = || {
            OpenOptions::new()
                .read(true)
                .share_mode(1)
                .custom_flags(0x02000000 | 0x00200000)
                .open(&current)
        };
        let handle = match open() {
            Ok(handle) => handle,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => (),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
                    Err(_) => return Err(failure()),
                }
                open().map_err(|_| failure())?
            }
            Err(_) => return Err(failure()),
        };
        let metadata = handle.metadata().map_err(|_| failure())?;
        if !metadata.is_dir() || metadata.file_attributes() & 0x400 != 0 {
            return Err(failure());
        }
        held.push(handle);
    }
    let target = directory.join(name);
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.is_file() && metadata.file_attributes() & 0x400 == 0 => {
            return Ok(false);
        }
        Ok(_) => return Err(failure()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => return Err(failure()),
    }
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(1)
        .open(&target)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&target).map_err(|_| failure())?;
            if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
                return Err(failure());
            }
            return Ok(false);
        }
        Err(_) => {
            return Err(Failure::new(
                "bootstrap_write_failed",
                "configuration file could not be created",
            ));
        }
    };
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| {
            Failure::new(
                "bootstrap_write_failed",
                "configuration could not be fully persisted; inspect the target before retrying",
            )
        })?;
    Ok(true)
}
#[cfg(target_os = "linux")]
fn create_configuration(directory: &Path, name: &str, bytes: &[u8]) -> Result<bool> {
    use std::{
        ffi::CString,
        fs::File,
        os::fd::{AsRawFd, FromRawFd},
    };
    let failure = || {
        Failure::new(
            "unsafe_bootstrap_path",
            "bootstrap requires physical directories and a regular target; links are refused",
        )
    };
    let mut parent = unsafe {
        File::from_raw_fd({
            let fd = libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            );
            if fd < 0 {
                return Err(failure());
            }
            fd
        })
    };
    for component in directory.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let component = CString::new(component.as_encoded_bytes()).map_err(|_| failure())?;
        let mut fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            let made = unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) };
            if made < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(failure());
            }
            fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
        if fd < 0 {
            return Err(failure());
        }
        parent = unsafe { File::from_raw_fd(fd) };
    }
    let name = CString::new(name).expect("constant filename");
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
            return Err(Failure::new(
                "bootstrap_write_failed",
                "configuration file could not be created",
            ));
        }
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(failure());
        }
        if unsafe { metadata.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(failure());
        }
        return Ok(false);
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .and_then(|_| parent.sync_all())
        .map_err(|_| {
            Failure::new(
                "bootstrap_write_failed",
                "configuration could not be fully persisted; inspect the target before retrying",
            )
        })?;
    Ok(true)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn create_configuration(_: &Path, _: &str, _: &[u8]) -> Result<bool> {
    Err(Failure::new(
        "unsupported_secure_filesystem",
        "secure bootstrap is supported on Windows and Linux",
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostContext {
    version: u32,
    user_base: PathBuf,
    trusted_user_base: PathBuf,
    user_directory: String,
    trusted_workspace_roots: BTreeSet<PathBuf>,
    workspace_selection: Option<PathBuf>,
}
fn resolve() -> Result<EffectiveConfig> {
    let root = repo::root().map_err(|_| {
        Failure::new(
            "invalid_repository",
            "configuration resolution requires a Git checkout",
        )
    })?;
    let base = repo::resolve(&root, "HEAD").map_err(|_| {
        Failure::new(
            "invalid_baseline",
            "configuration resolution requires a committed HEAD",
        )
    })?;
    let repository = GitBaseline { root, commit: base };
    let capabilities = run_config::capabilities();
    match (
        env::var_os("STORYFLOW_HOST_CONTEXT"),
        env::var_os("STORYFLOW_HOST_CONTEXT_ROOT"),
    ) {
        (None, None) => Ok(config::resolve_repository(
            &repository,
            &capabilities,
            &capabilities,
        )?),
        (None, Some(_)) => Err(Failure::new(
            "missing_host_context",
            "host context root requires an explicit manifest path",
        )),
        (Some(_), None) => Err(Failure::new(
            "missing_host_context_root",
            "separately authorize the external host directory",
        )),
        (Some(path), Some(root)) => {
            let bytes = config::read_host_context(Path::new(&path), Path::new(&root))?;
            let context: HostContext = serde_json::from_slice(&bytes).map_err(|_| {
                Failure::new(
                    "invalid_host_context",
                    "expected versioned host trust inputs",
                )
            })?;
            if context.version != 1 {
                return Err(Failure::new(
                    "unsupported_host_context_version",
                    "unsupported host context version",
                ));
            }
            Ok(config::resolve(&ResolveInput {
                repository,
                user_location: UserLocation {
                    base: context.user_base,
                    directory: context.user_directory,
                },
                trust: HostTrust {
                    user_base: context.trusted_user_base,
                    workspace_roots: context.trusted_workspace_roots,
                },
                workspace_selection: context.workspace_selection,
                supported_capabilities: capabilities.clone(),
                granted_capabilities: capabilities,
            })?)
        }
    }
}
fn inspection(effective: &EffectiveConfig) -> Value {
    json!({
        "scopes": effective.scopes,
        "models": effective.models.iter().map(|(tier, model)| (tier, json!({"value":"[redacted]","origin":model.origin}))).collect::<std::collections::BTreeMap<_,_>>(),
        "agents": effective.agents.iter().map(|(id, agent)| (id, json!({"version":agent.version,"sha256":agent.sha256,"origin":agent.origin,"content":"[redacted]"}))).collect::<std::collections::BTreeMap<_,_>>(),
        "policy": {"protected":effective.policy.protected,"builtin_scope_gate":effective.policy.builtin_scope_gate,"checks":effective.policy.checks.iter().map(|check| json!({"id":check.value.id,"timeout_seconds":check.value.timeout_seconds,"program":"[redacted]","args":"[redacted]","origin":check.origin})).collect::<Vec<_>>()},
        "capabilities":effective.capabilities,
        "execution_admitted":false,
    })
}

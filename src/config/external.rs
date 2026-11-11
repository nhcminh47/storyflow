//! Windows component handles deny rename/delete and reparse mutation while in use.
//! Linux uses openat2 with RESOLVE_BENEATH. No canonicalize-then-open read is used.
use super::*;
use std::{
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
};

pub(crate) fn absolute(path: &Path, origin: &Origin) -> ConfigResult<()> {
    if !path.is_absolute() {
        return Err(Diagnostic::new(
            "invalid_root",
            origin,
            "root must be a native absolute path",
        ));
    }
    let text = path
        .to_str()
        .ok_or_else(|| Diagnostic::new("invalid_root", origin, "root must be UTF-8"))?;
    if text.split(['/', '\\']).any(|p| matches!(p, "." | "..")) {
        return Err(Diagnostic::new(
            "invalid_root",
            origin,
            "dot components are not accepted in roots",
        ));
    }
    if text.starts_with("\\\\?\\") || text.starts_with("\\\\.\\") || text.starts_with("//?/") {
        return Err(Diagnostic::new(
            "invalid_root",
            origin,
            "device namespace roots are not accepted",
        ));
    }
    for component in path.components() {
        match component {
            Component::Normal(c) => validate_relative_path(
                c.to_str().ok_or_else(|| {
                    Diagnostic::new("invalid_root", origin, "invalid root spelling")
                })?,
                origin,
            )?,
            Component::ParentDir | Component::CurDir => {
                return Err(Diagnostic::new(
                    "invalid_root",
                    origin,
                    "root aliases are not accepted",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) struct TrustedRoot {
    pub path: PathBuf,
    // Holding every ancestor prevents substitution of any component on Windows.
    _handles: Vec<File>,
}
impl TrustedRoot {
    pub fn establish(path: &Path, origin: &Origin) -> ConfigResult<Self> {
        absolute(path, origin)?;
        let (handles, canonical) = walk(path, true, origin)?;
        Ok(Self {
            path: canonical,
            _handles: handles,
        })
    }
    pub fn directory(&self, relative: &str, origin: &Origin) -> ConfigResult<Self> {
        validate_relative_path(relative, origin)?;
        let (handles, path) = beneath(self, relative, true, origin)?;
        if !path.starts_with(&self.path) {
            return Err(Diagnostic::new(
                "path_escape",
                origin,
                "directory escaped trusted root",
            ));
        }
        Ok(Self {
            path,
            _handles: handles,
        })
    }
    pub fn read(
        &self,
        relative: &str,
        optional: bool,
        origin: &Origin,
    ) -> ConfigResult<Option<Vec<u8>>> {
        self.read_checked(relative, optional, origin, |_| Ok(()))
    }
    /// Validate the actual opened target before consuming bytes. Keep its handle
    /// live across validation and reading; never reopen the validated pathname.
    pub fn read_checked(
        &self,
        relative: &str,
        optional: bool,
        origin: &Origin,
        validate: impl FnOnce(&Path) -> ConfigResult<()>,
    ) -> ConfigResult<Option<Vec<u8>>> {
        validate_relative_path(relative, origin)?;
        let (mut handles, path) = match beneath(self, relative, false, origin) {
            Ok(value) => value,
            Err(error) if optional && error.code == "missing_artifact" => return Ok(None),
            Err(error) => return Err(error),
        };
        if !path.starts_with(&self.path) {
            return Err(Diagnostic::new(
                "path_escape",
                origin,
                "opened artifact escaped trusted root",
            ));
        }
        validate(&path)?;
        let mut bytes = Vec::new();
        handles
            .last_mut()
            .expect("walk includes file handle")
            .read_to_end(&mut bytes)
            .map_err(|_| {
                Diagnostic::new(
                    "artifact_read_failed",
                    origin,
                    "cannot read opened artifact",
                )
            })?;
        std::str::from_utf8(&bytes)
            .map_err(|_| Diagnostic::new("invalid_utf8", origin, "artifact is not UTF-8"))?;
        Ok(Some(bytes))
    }
}

#[cfg(windows)]
fn walk(path: &Path, directory: bool, origin: &Origin) -> ConfigResult<(Vec<File>, PathBuf)> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    };
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut std::ffi::c_void,
            buffer: *mut u16,
            length: u32,
            flags: u32,
        ) -> u32;
        fn CompareStringOrdinal(
            first: *const u16,
            first_len: i32,
            second: *const u16,
            second_len: i32,
            ignore_case: i32,
        ) -> i32;
    }
    fn final_path(file: &File, origin: &Origin) -> ConfigResult<PathBuf> {
        let mut buffer = vec![0u16; 32768];
        // SAFETY: a live File owns the handle, buffer is writable for length elements.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        };
        if length == 0 || length as usize >= buffer.len() {
            return Err(Diagnostic::new(
                "unsafe_filesystem",
                origin,
                "cannot verify opened object identity",
            ));
        }
        Ok(PathBuf::from(OsString::from_wide(
            &buffer[..length as usize],
        )))
    }
    let mut handles = Vec::new();
    let mut current = PathBuf::new();
    let components: Vec<_> = path.components().collect();
    for (i, c) in components.iter().enumerate() {
        current.push(c.as_os_str());
        if matches!(c, Component::Prefix(_)) {
            continue;
        }
        let expect_dir = i < components.len() - 1 || directory;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ only: refuse writers and rename/delete handles.
            .custom_flags(0x00200000 | 0x02000000) // OPEN_REPARSE_POINT | BACKUP_SEMANTICS
            .open(&current)
            .map_err(|e| {
                Diagnostic::new(
                    if e.kind() == std::io::ErrorKind::NotFound {
                        "missing_artifact"
                    } else {
                        "unsafe_filesystem"
                    },
                    origin,
                    "cannot securely open path component",
                )
            })?;
        let meta = file.metadata().map_err(|_| {
            Diagnostic::new("unsafe_filesystem", origin, "cannot inspect opened object")
        })?;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(Diagnostic::new(
                "path_escape",
                origin,
                "external reparse points are unsupported; use physical in-root paths",
            ));
        }
        if (expect_dir && !meta.is_dir()) || (!expect_dir && !meta.is_file()) {
            return Err(Diagnostic::new(
                "invalid_artifact_type",
                origin,
                "expected directory or regular file",
            ));
        }
        let actual = final_path(&file, origin)?;
        // GetFinalPathNameByHandle expands short names. The opened name must be
        // the canonical spelling of this very object, not an 8.3 alias.
        if let Some(Component::Normal(requested)) = current.components().next_back() {
            let actual_name = actual.file_name().ok_or_else(|| {
                Diagnostic::new("unsafe_filesystem", origin, "opened name unavailable")
            })?;
            let requested: Vec<_> = requested.encode_wide().collect();
            let actual_name: Vec<_> = actual_name.encode_wide().collect();
            // SAFETY: both UTF-16 slices are live for the supplied lengths.
            let equal = unsafe {
                CompareStringOrdinal(
                    requested.as_ptr(),
                    requested.len() as i32,
                    actual_name.as_ptr(),
                    actual_name.len() as i32,
                    1,
                )
            } == 2;
            if !equal {
                return Err(Diagnostic::new(
                    "invalid_path",
                    origin,
                    "filesystem aliases are not accepted",
                ));
            }
        }
        current = actual;
        handles.push(file);
    }
    if handles.is_empty() {
        return Err(Diagnostic::new("invalid_root", origin, "empty root"));
    }
    Ok((handles, current))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn walk(_path: &Path, _directory: bool, origin: &Origin) -> ConfigResult<(Vec<File>, PathBuf)> {
    Err(Diagnostic::new(
        "unsupported_secure_filesystem",
        origin,
        "secure external handle traversal requires Windows or Linux openat2",
    ))
}

#[cfg(not(target_os = "linux"))]
fn beneath(
    root: &TrustedRoot,
    relative: &str,
    directory: bool,
    origin: &Origin,
) -> ConfigResult<(Vec<File>, PathBuf)> {
    walk(&root.path.join(relative), directory, origin)
}

#[cfg(target_os = "linux")]
fn walk(path: &Path, directory: bool, origin: &Origin) -> ConfigResult<(Vec<File>, PathBuf)> {
    linux_open(libc::AT_FDCWD, path, directory, 0x02, origin) // RESOLVE_NO_MAGICLINKS
}

#[cfg(target_os = "linux")]
fn beneath(
    root: &TrustedRoot,
    relative: &str,
    directory: bool,
    origin: &Origin,
) -> ConfigResult<(Vec<File>, PathBuf)> {
    use std::os::fd::AsRawFd;
    let dir = root._handles.last().expect("root handle").as_raw_fd();
    match linux_open(dir, Path::new(relative), directory, 0x08 | 0x02, origin) {
        Err(error) if error.code == "missing_artifact" => {
            // ENOENT after following a link does not establish that an optional
            // entry is absent. Probe entries without following their final link,
            // still anchored to the trusted descriptor and constrained beneath it.
            if linux_entry_absent(dir, relative, origin)? {
                Err(error)
            } else {
                Err(Diagnostic::new(
                    "unsafe_filesystem",
                    origin,
                    "external entry changed during resolution",
                ))
            }
        }
        result => result,
    }
}

#[cfg(target_os = "linux")]
fn linux_open(
    dir: libc::c_int,
    path: &Path,
    directory: bool,
    resolve: u64,
    origin: &Origin,
) -> ConfigResult<(Vec<File>, PathBuf)> {
    use std::os::fd::AsRawFd;
    let flags = if directory {
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC
    } else {
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC
    };
    let file = linux_open_handle(dir, path, flags, resolve, origin)?;
    let meta = file.metadata().map_err(|_| {
        Diagnostic::new("unsafe_filesystem", origin, "cannot inspect opened object")
    })?;
    if (directory && !meta.is_dir()) || (!directory && !meta.is_file()) {
        return Err(Diagnostic::new(
            "invalid_artifact_type",
            origin,
            "expected directory or regular file",
        ));
    }
    let canonical =
        std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(|_| {
            Diagnostic::new(
                "unsupported_secure_filesystem",
                origin,
                "opened-object identity requires procfs",
            )
        })?;
    Ok((vec![file], canonical))
}

#[cfg(target_os = "linux")]
fn linux_entry_absent(dir: libc::c_int, relative: &str, origin: &Origin) -> ConfigResult<bool> {
    let mut prefix = PathBuf::new();
    for component in Path::new(relative).components() {
        prefix.push(component);
        let entry = match linux_open_handle(
            dir,
            &prefix,
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0x08 | 0x02,
            origin,
        ) {
            Ok(file) => file,
            Err(error) if error.code == "missing_artifact" => return Ok(true),
            Err(error) => return Err(error),
        };
        let meta = entry.metadata().map_err(|_| {
            Diagnostic::new("unsafe_filesystem", origin, "cannot inspect external entry")
        })?;
        if meta.file_type().is_symlink() {
            match linux_open_handle(
                dir,
                &prefix,
                libc::O_PATH | libc::O_CLOEXEC,
                0x08 | 0x02,
                origin,
            ) {
                Err(error) if error.code == "missing_artifact" => {
                    return Err(Diagnostic::new(
                        "dangling_symlink",
                        origin,
                        "existing external symlink target is unavailable",
                    ));
                }
                Err(error) => return Err(error),
                Ok(_) => {}
            }
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn linux_open_handle(
    dir: libc::c_int,
    path: &Path,
    flags: libc::c_int,
    resolve: u64,
    origin: &Origin,
) -> ConfigResult<File> {
    use std::{
        ffi::CString,
        os::{fd::FromRawFd, unix::ffi::OsStrExt},
    };
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Diagnostic::new("invalid_path", origin, "NUL in external path"))?;
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve,
    };
    // SAFETY: openat2 receives a valid C path and correctly sized repr(C) open_how.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir,
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        let code = match error.raw_os_error() {
            Some(libc::ENOENT) => "missing_artifact",
            Some(libc::EXDEV | libc::ELOOP) => "path_escape",
            Some(libc::ENOSYS | libc::EINVAL) => "unsupported_secure_filesystem",
            _ => "unsafe_filesystem",
        };
        return Err(Diagnostic::new(
            code,
            origin,
            "secure openat2 traversal failed (requires Linux 5.6+)",
        ));
    }
    // SAFETY: successful openat2 returned a newly owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[test]
    fn opened_object_and_ancestors_cannot_be_replaced() {
        // The Windows TEMP environment may contain an 8.3 alias (RUNNER~1),
        // which is correctly refused by TrustedRoot. Keep the test fixture
        // under Cargo's checkout instead of weakening production validation.
        let path = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
            "sf4-handles-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(path.join("agents")).unwrap();
        std::fs::write(path.join("agents/p.md"), b"# exact\r\n").unwrap();
        let at = Origin::new(Scope::User, "handle-test", "artifact");
        let root = TrustedRoot::establish(&path, &at).unwrap();
        let (mut handles, actual) = beneath(&root, "agents/p.md", false, &at).unwrap();
        assert!(actual.starts_with(&root.path));
        assert!(std::fs::rename(path.join("agents"), path.join("replaced")).is_err());
        assert!(std::fs::rename(path.join("agents/p.md"), path.join("moved.md")).is_err());
        assert!(std::fs::write(path.join("agents/p.md"), b"changed").is_err());
        let mut bytes = Vec::new();
        handles.last_mut().unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"# exact\r\n");
        drop(handles);
        drop(root);
        std::fs::rename(path.join("agents"), path.join("replaced")).unwrap();
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn checked_read_keeps_the_validated_handle_after_path_replacement() {
        let path = std::env::temp_dir().join(format!(
            "sf4-checked-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(path.join("trusted")).unwrap();
        std::fs::write(path.join("trusted/p.md"), b"# original\r\n").unwrap();
        std::fs::write(path.join("outside.md"), b"# untrusted replacement").unwrap();
        let at = Origin::new(Scope::User, "checked-read-test", "artifact");
        let root = TrustedRoot::establish(&path.join("trusted"), &at).unwrap();
        let bytes = root
            .read_checked("p.md", false, &at, |opened| {
                assert_eq!(opened, root.path.join("p.md"));
                std::fs::rename(opened, root.path.join("original.md")).unwrap();
                std::os::unix::fs::symlink(path.join("outside.md"), opened).unwrap();
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"# original\r\n");
        drop(root);
        std::fs::remove_dir_all(path).unwrap();
    }
}

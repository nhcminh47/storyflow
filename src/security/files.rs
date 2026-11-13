//! Descriptor/handle based regular-file access for the managed runtime.
//! The host owns the root and excludes concurrent hostile same-identity writers.
use crate::config::{Origin, Scope, validate_relative_path};
use std::{
    io::{self, Read, Write},
    path::Path,
};
use unicode_normalization::UnicodeNormalization;

fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "managed_path_denied")
}
fn fold(name: &str) -> String {
    name.nfc()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .nfc()
        .collect()
}
fn parts(relative: &str) -> io::Result<Vec<&str>> {
    validate_relative_path(relative, &Origin::new(Scope::Repo, "managed", "path"))
        .map_err(|_| denied())?;
    if relative.nfc().collect::<String>() != relative
        || relative.split('/').any(|p| fold(p) == ".git")
    {
        return Err(denied());
    }
    Ok(relative.split('/').collect())
}

pub fn read(root: &Path, relative: &str, limit: usize) -> io::Result<Vec<u8>> {
    let parts = parts(relative)?;
    let mut file = platform::open(root, &parts, false)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(denied());
    }
    Ok(bytes)
}

pub fn write(root: &Path, relative: &str, bytes: &[u8]) -> io::Result<()> {
    let parts = parts(relative)?;
    let mut file = platform::open(root, &parts, true)?;
    // Validate the opened object before truncating; never damage a denied link.
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        fs::File,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
    };

    fn cstr(name: &str) -> io::Result<CString> {
        CString::new(name).map_err(|_| denied())
    }
    fn own(fd: i32) -> io::Result<OwnedFd> {
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: open/openat/dup returned a new owned descriptor.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }
    fn stat(fd: i32) -> io::Result<libc::stat> {
        // SAFETY: zeroed stat is a valid output buffer; fd is live.
        let mut value = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut value) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(value)
    }
    fn names(fd: i32) -> io::Result<Vec<String>> {
        // A fresh open file description avoids sharing readdir offsets across calls.
        let dot = c".";
        let duplicate = own(unsafe {
            libc::openat(
                fd,
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })?;
        use std::os::fd::IntoRawFd;
        let raw = duplicate.into_raw_fd();
        // SAFETY: fdopendir takes ownership on success.
        let dir = unsafe { libc::fdopendir(raw) };
        if dir.is_null() {
            unsafe {
                libc::close(raw);
            }
            return Err(io::Error::last_os_error());
        }
        struct Entries(*mut libc::DIR);
        impl Drop for Entries {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let _entries = Entries(dir);
        let mut result = Vec::new();
        loop {
            unsafe {
                *libc::__errno_location() = 0;
            }
            // SAFETY: live directory stream; name is copied before next readdir.
            let entry = unsafe { libc::readdir(dir) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(0) {
                    return Err(error);
                }
                break;
            }
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            let name = std::str::from_utf8(bytes).map_err(|_| denied())?;
            if !matches!(name, "." | "..") {
                result.push(name.to_owned());
            }
        }
        Ok(result)
    }
    fn validate_name(fd: i32, requested: &str) -> io::Result<()> {
        let identity = fold(requested);
        for name in names(fd)? {
            if fold(&name) == identity && name != requested {
                return Err(denied());
            }
        }
        Ok(())
    }
    pub(super) fn open(root: &Path, parts: &[&str], write: bool) -> io::Result<File> {
        if !root.is_absolute() {
            return Err(denied());
        }
        let root_name = CString::new(root.as_os_str().as_bytes()).map_err(|_| denied())?;
        let mut parent = own(unsafe {
            libc::open(
                root_name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        for name in &parts[..parts.len() - 1] {
            validate_name(parent.as_raw_fd(), name)?;
            let name = cstr(name)?;
            parent = own(unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })?;
            // Never cross a nested repository or submodule, including .GIT aliases.
            if names(parent.as_raw_fd())?
                .iter()
                .any(|name| fold(name) == ".git")
            {
                return Err(denied());
            }
        }
        let leaf = parts[parts.len() - 1];
        validate_name(parent.as_raw_fd(), leaf)?;
        let leaf = cstr(leaf)?;
        if write {
            // O_EXCL creates only a new regular file, never opens an existing
            // device, FIFO or link. Existing files use an inspection handle.
            let created = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    leaf.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            if created >= 0 {
                return Ok(File::from(own(created)?));
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(error);
            }
        }
        // O_PATH inspects without opening a device or blocking on a FIFO.
        let inspection = own(unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        let info = stat(inspection.as_raw_fd())?;
        if info.st_mode & libc::S_IFMT != libc::S_IFREG || info.st_nlink != 1 {
            return Err(denied());
        }
        // procfs reopens the exact inspected object, avoiding a second pathname
        // resolution of an attacker-replaceable leaf. Missing procfs fails closed.
        let descriptor_path = cstr(&format!("/proc/self/fd/{}", inspection.as_raw_fd()))?;
        let fd = own(unsafe {
            libc::open(
                descriptor_path.as_ptr(),
                libc::O_CLOEXEC
                    | libc::O_NONBLOCK
                    | if write {
                        libc::O_WRONLY
                    } else {
                        libc::O_RDONLY
                    },
            )
        })?;
        let actual = stat(fd.as_raw_fd())?;
        if actual.st_dev != info.st_dev
            || actual.st_ino != info.st_ino
            || actual.st_nlink != 1
            || actual.st_mode & libc::S_IFMT != libc::S_IFREG
        {
            return Err(denied());
        }
        Ok(File::from(fd))
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        ffi::c_void,
        fs::{self, File},
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        path::PathBuf,
    };
    type Handle = *mut c_void;
    const INVALID_HANDLE: Handle = -1isize as Handle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const SHARE_READ: u32 = 1;
    const SHARE_WRITE: u32 = 2;
    const OPEN_EXISTING: u32 = 3;
    const OPEN_ALWAYS: u32 = 4;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const DIRECTORY: u32 = 0x10;
    const REPARSE_POINT: u32 = 0x400;
    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct Information {
        attributes: u32,
        creation: FileTime,
        access: FileTime,
        write: FileTime,
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            creation: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn GetFileInformationByHandle(file: Handle, info: *mut Information) -> i32;
        fn GetFileType(file: Handle) -> u32;
    }
    fn open_one(path: &Path, directory: bool, write: bool) -> io::Result<File> {
        let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
        if name.contains(&0) {
            return Err(denied());
        }
        name.push(0);
        // Excluding FILE_SHARE_DELETE pins every opened component against rename
        // and replacement while descendant paths are resolved. No reparse follows.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                if !directory && write {
                    GENERIC_WRITE
                } else {
                    GENERIC_READ
                },
                if directory {
                    SHARE_READ | SHARE_WRITE
                } else {
                    SHARE_READ
                },
                std::ptr::null_mut(),
                if write { OPEN_ALWAYS } else { OPEN_EXISTING },
                BACKUP_SEMANTICS | OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_handle(handle) };
        let mut info: Information = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.attributes & REPARSE_POINT != 0
            || (info.attributes & DIRECTORY != 0) != directory
            || (!directory
                && (info.links != 1 || unsafe { GetFileType(file.as_raw_handle()) } != 1))
        {
            return Err(denied());
        }
        Ok(file)
    }
    fn names(path: &Path) -> io::Result<Vec<String>> {
        fs::read_dir(path)?
            .map(|entry| entry?.file_name().into_string().map_err(|_| denied()))
            .collect()
    }
    fn validate_name(parent: &Path, requested: &str) -> io::Result<()> {
        for name in names(parent)? {
            if fold(&name) == fold(requested) && name != requested {
                return Err(denied());
            }
        }
        Ok(())
    }
    pub(super) fn open(root: &Path, parts: &[&str], write: bool) -> io::Result<File> {
        if !root.is_absolute() {
            return Err(denied());
        }
        let mut ancestors = vec![open_one(root, true, false)?];
        let mut path = PathBuf::from(root);
        for name in &parts[..parts.len() - 1] {
            validate_name(&path, name)?;
            path.push(name);
            ancestors.push(open_one(&path, true, false)?);
            if names(&path)?.iter().any(|name| fold(name) == ".git") {
                return Err(denied());
            }
        }
        let leaf = parts[parts.len() - 1];
        validate_name(&path, leaf)?;
        path.push(leaf);
        let file = open_one(&path, false, write)?;
        // Parent handles remain pinned until the final file handle is acquired.
        drop(ancestors);
        Ok(file)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use super::*;
    pub(super) fn open(_: &Path, _: &[&str], _: bool) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed_files_unsupported",
        ))
    }
}

#[cfg(all(test, any(target_os = "linux", windows)))]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sf-files-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("app")).unwrap();
            fs::write(path.join("app/value"), b"original").unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn regular_access_and_bounds() {
        let f = Fixture::new();
        assert_eq!(read(&f.0, "app/value", 20).unwrap(), b"original");
        assert!(read(&f.0, "app/value", 2).is_err());
        write(&f.0, "app/value", b"new").unwrap();
        write(&f.0, "app/created", b"created").unwrap();
        assert_eq!(fs::read(f.0.join("app/value")).unwrap(), b"new");
    }
    #[test]
    fn traversal_aliases_and_boundaries() {
        let f = Fixture::new();
        fs::write(f.0.join("app/Caf\u{e9}"), b"protected").unwrap();
        fs::create_dir_all(f.0.join("app/nested/.git")).unwrap();
        fs::write(f.0.join("app/nested/value"), b"nested").unwrap();
        for path in [
            "../outside",
            "app/../outside",
            "app\\value",
            "/app/value",
            "app/.GIT/config",
            "app/VALUE",
            "APP/value",
            "app/Cafe\u{301}",
            "app/nested/value",
            "app/value/child",
            "app",
        ] {
            assert!(read(&f.0, path, 100).is_err(), "read allowed {path}");
            assert!(
                write(&f.0, path, b"changed").is_err(),
                "write allowed {path}"
            );
        }
        assert_eq!(fs::read(f.0.join("app/value")).unwrap(), b"original");
    }
    #[test]
    fn hardlinks_are_denied_before_truncation() {
        let f = Fixture::new();
        fs::hard_link(f.0.join("app/value"), f.0.join("app/link")).unwrap();
        assert!(read(&f.0, "app/link", 100).is_err());
        assert!(write(&f.0, "app/link", b"changed").is_err());
        assert_eq!(fs::read(f.0.join("app/value")).unwrap(), b"original");
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn symlink_and_fifo_denied() {
        use std::{ffi::CString, os::unix::fs::symlink};
        let f = Fixture::new();
        symlink(f.0.join("app/value"), f.0.join("app/link")).unwrap();
        symlink(f.0.join("app"), f.0.join("alias")).unwrap();
        let fifo = CString::new(f.0.join("app/fifo").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        for path in ["app/link", "alias/value", "app/fifo"] {
            assert!(read(&f.0, path, 100).is_err());
            assert!(write(&f.0, path, b"changed").is_err());
        }
        assert_eq!(fs::read(f.0.join("app/value")).unwrap(), b"original");
    }
    #[cfg(windows)]
    #[test]
    fn junction_denied() {
        let f = Fixture::new();
        let junction = f.0.join("alias");
        let status = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(f.0.join("app"))
            .status()
            .unwrap();
        assert!(status.success());
        assert!(read(&f.0, "alias/value", 100).is_err());
        assert!(write(&f.0, "alias/value", b"changed").is_err());
        fs::remove_dir(junction).unwrap();
        assert_eq!(fs::read(f.0.join("app/value")).unwrap(), b"original");
    }
}

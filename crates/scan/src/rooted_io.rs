//! Reversible path persistence and descriptor-backed configured-root I/O.

use std::path::{Path, PathBuf};

use crate::ScanConfig;

pub(super) const PATH_HEX_PREFIX: &str = "RDLNA_PATH_HEX_V1:";
const PATH_UTF8_ESCAPE_PREFIX: &str = "RDLNA_PATH_UTF8_V1:";

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn unhex_bytes(encoded: &str) -> Option<Vec<u8>> {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    if encoded.len() & 1 != 0 {
        return None;
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

/// Reversible SQLite TEXT representation. Ordinary UTF-8 stays readable;
/// invalid Unix bytes and reserved-prefix UTF-8 names are hex escaped.
pub fn path_to_db(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        match std::str::from_utf8(bytes) {
            Ok(text)
                if !text.starts_with(PATH_HEX_PREFIX)
                    && !text.starts_with(PATH_UTF8_ESCAPE_PREFIX) =>
            {
                text.to_string()
            }
            Ok(_) => format!("{PATH_UTF8_ESCAPE_PREFIX}{}", hex_bytes(bytes)),
            Err(_) => format!("{PATH_HEX_PREFIX}{}", hex_bytes(bytes)),
        }
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned()
    }
}

pub fn path_from_db(stored: &str) -> PathBuf {
    let encoded = stored
        .strip_prefix(PATH_HEX_PREFIX)
        .or_else(|| stored.strip_prefix(PATH_UTF8_ESCAPE_PREFIX));
    #[cfg(unix)]
    if let Some(bytes) = encoded.and_then(unhex_bytes) {
        use std::os::unix::ffi::OsStringExt;
        return PathBuf::from(std::ffi::OsString::from_vec(bytes));
    }
    PathBuf::from(stored)
}

pub fn path_is_live_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

/// A regular file opened through the configured root policy. The descriptor,
/// not the original pathname, is the security boundary for subsequent I/O.
#[derive(Debug)]
pub struct RootedFile {
    pub file: std::fs::File,
    pub resolved_path: PathBuf,
}

impl RootedFile {
    /// Stable same-process pathname for libraries that accept paths rather
    /// than descriptors. The owned descriptor must remain live while used.
    pub fn proc_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
    }
}

fn permission_denied(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("path is outside an allowed root: {}", path.display()),
    )
}

fn open_directory_without_symlinks(path: &Path) -> std::io::Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let mut directory = std::fs::File::open("/")?;
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            continue;
        };
        let component = CString::new(component.as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
        // SAFETY: directory is a live owned descriptor; component is NUL
        // terminated; a successful descriptor is uniquely owned below.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd is the unique successful openat return value.
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    Ok(directory)
}

fn open_beneath_directory(
    directory: &std::fs::File,
    relative: &Path,
) -> std::io::Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    if relative.as_os_str().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "root directory is not a regular file",
        ));
    }
    let relative_c = CString::new(relative.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: open_how is a plain kernel ABI struct for which zero is the
    // documented default for every field; assigned fields are valid flags.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    // Check the opened descriptor's type below. A pathname can be a FIFO
    // (including after replacement), so opening must not wait for a writer.
    how.flags = REGULAR_FILE_OPEN_FLAGS as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS | libc::RESOLVE_NO_SYMLINKS;
    // SAFETY: pointers refer to initialized values for the syscall duration;
    // a successful descriptor is transferred to File below.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory.as_raw_fd(),
            relative_c.as_ptr(),
            &how,
            std::mem::size_of::<libc::open_how>(),
        ) as i32
    };
    if fd >= 0 {
        // SAFETY: fd is the unique successful openat2 return value.
        return require_regular_file(unsafe { std::fs::File::from_raw_fd(fd) });
    }
    let openat2_error = std::io::Error::last_os_error();
    if !matches!(
        openat2_error.raw_os_error(),
        Some(libc::ENOSYS | libc::EINVAL | libc::EPERM)
    ) {
        return Err(openat2_error);
    }

    open_beneath_directory_fallback(directory, relative)
}

const REGULAR_FILE_OPEN_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

fn require_regular_file(file: std::fs::File) -> std::io::Result<std::fs::File> {
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "allowed path is not a regular file",
        ));
    }
    // O_NONBLOCK has no effect on regular-file reads. Retaining it avoids
    // another syscall and does not change the descriptor's media semantics.
    Ok(file)
}

// Old kernels and seccomp profiles use the same file-type policy, walking
// the canonical relative path one component at a time without symlinks.
fn open_beneath_directory_fallback(
    directory: &std::fs::File,
    relative: &Path,
) -> std::io::Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let mut current = directory.try_clone()?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(component) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "non-normal rooted path component",
            ));
        };
        let component = CString::new(component.as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
        let flags = if components.peek().is_some() {
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            REGULAR_FILE_OPEN_FLAGS
        };
        // SAFETY: current is live and component is a valid C string.
        let next = unsafe { libc::openat(current.as_raw_fd(), component.as_ptr(), flags) };
        if next < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: next is the unique successful openat return value.
        current = unsafe { std::fs::File::from_raw_fd(next) };
    }
    require_regular_file(current)
}

/// Open a regular file under `roots` without a check-then-open pathname race.
///
/// Strict mode resolves the requested alias once, verifies its target is under
/// a canonical root, then opens the canonical relative path beneath an opened
/// root descriptor. `wide_links` retains its explicit outside-root behavior,
/// but still opens the resolved absolute path without following a symlink
/// during the descriptor walk.
pub fn open_file_under_roots(
    path: &Path,
    roots: &[PathBuf],
    wide_links: bool,
) -> std::io::Result<RootedFile> {
    if roots.is_empty() {
        return Err(permission_denied(path));
    }
    let resolved_path = path.canonicalize()?;
    let mut matched = None;
    for root in roots {
        let Ok(canonical_root) = root.canonicalize() else {
            continue;
        };
        if resolved_path.starts_with(&canonical_root) {
            matched = Some(canonical_root);
            break;
        }
    }

    let (directory_path, relative) = if let Some(root) = matched {
        let relative = resolved_path
            .strip_prefix(&root)
            .map_err(|_| permission_denied(path))?
            .to_path_buf();
        (root, relative)
    } else if wide_links && lexical_path_is_under_roots(path, roots) {
        let relative = resolved_path
            .strip_prefix("/")
            .map_err(|_| permission_denied(path))?
            .to_path_buf();
        (PathBuf::from("/"), relative)
    } else {
        return Err(permission_denied(path));
    };

    let directory = open_directory_without_symlinks(&directory_path)?;
    let file = open_beneath_directory(&directory, &relative)?;
    Ok(RootedFile {
        file,
        resolved_path,
    })
}

pub fn open_allowed_file(path: &Path, cfg: &ScanConfig) -> std::io::Result<RootedFile> {
    open_file_under_roots(path, &cfg.media_dirs, cfg.wide_links)
}

fn canonical_path_is_under_roots(real: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| {
        root.canonicalize()
            .ok()
            .is_some_and(|canonical_root| real.starts_with(canonical_root))
    })
}

fn lexical_path_is_under_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| {
        path.starts_with(root)
            || root
                .canonicalize()
                .ok()
                .is_some_and(|canonical_root| path.starts_with(canonical_root))
    })
}

fn path_is_allowed_kind(
    path: &Path,
    roots: &[PathBuf],
    wide_links: bool,
    wanted: impl FnOnce(&std::fs::Metadata) -> bool,
) -> bool {
    if roots.is_empty() {
        return false;
    }
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(&real) else {
        return false;
    };
    if !wanted(&meta) {
        return false;
    }
    canonical_path_is_under_roots(&real, roots)
        || (wide_links && lexical_path_is_under_roots(path, roots))
}

/// Apply the scanner's root policy to a regular file. With `wide_links=false`
/// both the walked name and its final canonical target stay jailed. With the
/// explicit opt-in enabled, a link lexically below a configured root may point
/// outside it and the same rule is used by HTTP serving.
pub fn path_is_allowed_file(path: &Path, cfg: &ScanConfig) -> bool {
    open_allowed_file(path, cfg).is_ok()
}

/// Directory counterpart to [`path_is_allowed_file`], used by both walkers and
/// the inotify watch builder before opening or descending a directory.
pub fn path_is_allowed_dir(path: &Path, cfg: &ScanConfig) -> bool {
    path_is_allowed_kind(path, &cfg.media_dirs, cfg.wide_links, |meta| meta.is_dir())
}

/// True if `path` is a regular file whose canonical location is under one
/// of `roots`. Follows symlinks, so a link that escapes the tree is false.
pub fn path_is_under_roots(path: &Path, roots: &[PathBuf]) -> bool {
    open_file_under_roots(path, roots, false).is_ok()
}

/// Rebase a persisted media path using the root record that owns it. This does
/// not search for a coincidentally matching directory-name component.
pub fn rebase_media_path_for_config(stored: &Path, cfg: &ScanConfig) -> PathBuf {
    if stored.as_os_str().is_empty() {
        return stored.to_path_buf();
    }
    let Some(root) = cfg.selected_root(stored) else {
        return stored.to_path_buf();
    };
    let Ok(relative) = stored.strip_prefix(root.relative_to) else {
        return stored.to_path_buf();
    };
    let candidate = root.configured_path.join(relative);
    if path_is_live_file(&candidate) {
        candidate
    } else {
        stored.to_path_buf()
    }
}

/// Stable reconciliation key qualified by its selected media-root identity.
pub fn media_rel_key_for_config(path: &Path, cfg: &ScanConfig) -> String {
    let Some(root) = cfg.selected_root(path) else {
        return path_to_db(path);
    };
    let Ok(relative) = path.strip_prefix(root.relative_to) else {
        return path_to_db(path);
    };
    format!("{}:{}", root.key, path_to_db(relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, OpenOptionsExt};
    use std::time::Duration;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rusty-rooted-io-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a live NUL-terminated filename in our private test tree.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn rooted_and_fallback_opens_reject_fifos_without_waiting_for_a_writer() {
        for fallback in [false, true] {
            let dir = TestDirectory::new();
            let path = dir.0.join("movie.mp4");
            // Cover a regular catalog path replaced by a FIFO, as well as
            // discovery of a media-named symlink to that FIFO.
            std::fs::write(&path, b"previous regular file").unwrap();
            std::fs::remove_file(&path).unwrap();
            fifo(&path);
            symlink("movie.mp4", dir.0.join("alias.mp4")).unwrap();
            let root = dir.0.clone();
            let (sent, received) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let result = if fallback {
                    let directory = open_directory_without_symlinks(&root).unwrap();
                    open_beneath_directory_fallback(&directory, Path::new("movie.mp4"))
                } else {
                    open_file_under_roots(&root.join("alias.mp4"), &[root], false)
                        .map(|opened| opened.file)
                };
                let _ = sent.send(result.map_err(|error| error.kind()));
            });
            let result = received.recv_timeout(Duration::from_secs(1));
            // A regression must fail instead of hanging the suite. Linux
            // O_RDWR opens a FIFO without a peer and releases a blocked reader.
            let _release = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
                .unwrap();
            worker.join().unwrap();
            assert!(
                matches!(result, Ok(Err(std::io::ErrorKind::InvalidInput))),
                "fallback={fallback}: FIFO open must reject promptly: {result:?}"
            );
        }
    }

    #[test]
    fn rooted_and_fallback_opens_preserve_regular_reads_and_reject_special_files() {
        use std::io::{Read, Seek};
        let dir = TestDirectory::new();
        std::fs::create_dir(dir.0.join("nested")).unwrap();
        std::fs::write(dir.0.join("nested/movie.mp4"), b"0123456789").unwrap();
        symlink("nested/movie.mp4", dir.0.join("alias.mp4")).unwrap();
        let directory = open_directory_without_symlinks(&dir.0).unwrap();
        for mut file in [
            open_file_under_roots(
                &dir.0.join("alias.mp4"),
                std::slice::from_ref(&dir.0),
                false,
            )
            .unwrap()
            .file,
            open_beneath_directory_fallback(&directory, Path::new("nested/movie.mp4")).unwrap(),
        ] {
            file.seek(std::io::SeekFrom::Start(3)).unwrap();
            let mut bytes = [0; 4];
            file.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"3456");
        }
        let _socket = std::os::unix::net::UnixListener::bind(dir.0.join("socket")).unwrap();
        for path in ["nested", "socket"] {
            assert!(open_beneath_directory(&directory, Path::new(path)).is_err());
            assert!(open_beneath_directory_fallback(&directory, Path::new(path)).is_err());
        }
        // The fallback accepts canonical paths only; it cannot follow even an
        // in-root symlink, nor escape through a parent component.
        for path in ["alias.mp4", "../movie.mp4"] {
            assert!(open_beneath_directory_fallback(&directory, Path::new(path)).is_err());
        }
    }

    #[test]
    fn database_path_encoding_preserves_plain_and_reserved_prefix_names() {
        let plain = Path::new("media/Show/episode.mkv");
        assert_eq!(path_to_db(plain), "media/Show/episode.mkv");
        assert_eq!(path_from_db(&path_to_db(plain)), plain);

        for reserved in [PATH_HEX_PREFIX, PATH_UTF8_ESCAPE_PREFIX] {
            let path = PathBuf::from(format!("{reserved}ordinary-name"));
            let stored = path_to_db(&path);
            assert!(stored.starts_with(PATH_UTF8_ESCAPE_PREFIX));
            assert_ne!(stored, path.to_string_lossy());
            assert_eq!(path_from_db(&stored), path);
        }
    }

    #[test]
    fn malformed_encoded_database_path_remains_literal() {
        for stored in [
            "RDLNA_PATH_HEX_V1:not-hex",
            "RDLNA_PATH_HEX_V1:0",
            "RDLNA_PATH_UTF8_V1:xy",
        ] {
            assert_eq!(path_from_db(stored), PathBuf::from(stored));
        }
    }
}

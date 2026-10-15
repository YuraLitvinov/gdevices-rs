//! Reads anchored to an already-open sysfs directory, so a path swapped mid-collection cannot
//! redirect them (the safe-Rust counterpart of the C++ `openat`/`readlinkat` helpers).
//! Lookups go through `/proc/self/fd/<n>/…`; only the final component is checked for symlinks.
use crate::model::{Attribute, ReadState};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub const TEXT_LIMIT: usize = 4096;

/// Attribute for a failed read. `ENODATA` counts as unavailable, like `ENOENT`.
pub fn failure(error: i32) -> Attribute {
    let state = match error {
        libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
        libc::ENODEV => ReadState::Removed,
        libc::ENOENT | libc::ENODATA => ReadState::Unavailable,
        _ => ReadState::Error,
    };
    Attribute { state, value: String::new(), error }
}

pub fn available(value: impl Into<String>) -> Attribute {
    Attribute { state: ReadState::Available, value: value.into(), error: 0 }
}

pub fn io_errno(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// Reads at most `limit` bytes (+1 to detect overflow). `Err` carries an errno.
pub fn read_limited(file: &mut File, limit: usize, cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>, i32> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    while bytes.len() <= limit {
        if cancelled() {
            return Err(libc::ECANCELED);
        }
        let wanted = buffer.len().min(limit + 1 - bytes.len());
        match file.read(&mut buffer[..wanted]) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_errno(&e)),
        }
    }
    Ok(bytes)
}

/// An open directory; every operation is relative to it.
pub struct Dir {
    file: File,
}

impl Dir {
    /// Opens a directory without following a symlink at the final component.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path).map(|file| Self { file })
    }

    fn anchored(&self, name: &str) -> String {
        format!("/proc/self/fd/{}/{name}", self.file.as_raw_fd())
    }

    /// Opens a sub-directory, following a final symlink only when `follow` is set (for
    /// documented kernel links such as `driver`).
    pub fn open_dir(&self, name: &str, follow: bool) -> std::io::Result<Dir> {
        let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
        OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | nofollow).open(self.anchored(name)).map(|file| Dir { file })
    }

    pub fn open_file(&self, name: &str) -> std::io::Result<File> {
        OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW).open(self.anchored(name))
    }

    /// Small text attribute, trimmed; over [`TEXT_LIMIT`] bytes is an `EOVERFLOW` error.
    pub fn read_text(&self, name: &str) -> Attribute {
        let mut file = match self.open_file(name) {
            Ok(f) => f,
            Err(e) => return failure(io_errno(&e)),
        };
        match read_limited(&mut file, TEXT_LIMIT, &|| false) {
            Err(error) => failure(error),
            Ok(bytes) if bytes.len() > TEXT_LIMIT => failure(libc::EOVERFLOW),
            Ok(bytes) => available(String::from_utf8_lossy(&bytes).trim()),
        }
    }

    /// Target text of a symlink, exactly as stored.
    pub fn link_target(&self, name: &str) -> Attribute {
        match fs::read_link(self.anchored(name)) {
            Err(e) => failure(io_errno(&e)),
            Ok(target) => {
                let text = target.to_string_lossy();
                if text.len() > TEXT_LIMIT {
                    failure(libc::EOVERFLOW)
                } else {
                    available(text.into_owned())
                }
            }
        }
    }

    /// `lstat` of an entry in this directory.
    pub fn lstat(&self, name: &str) -> std::io::Result<Metadata> {
        fs::symlink_metadata(self.anchored(name))
    }

    pub fn metadata(&self) -> std::io::Result<Metadata> {
        self.file.metadata()
    }

    pub fn file(&self) -> &File {
        &self.file
    }
}

/// Last path component of a link target (`QFileInfo::fileName`).
pub fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn reads_are_anchored_and_classified() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("name"), " value \n").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        symlink("../elsewhere/driver-x", tmp.path().join("driver")).unwrap();
        let dir = Dir::open(tmp.path()).unwrap();
        assert_eq!(dir.read_text("name").value, "value");
        assert_eq!(dir.read_text("missing").state, ReadState::Unavailable);
        assert_eq!(dir.link_target("driver").value, "../elsewhere/driver-x");
        assert_eq!(file_name(&dir.link_target("driver").value), "driver-x");
        assert!(dir.open_dir("sub", false).is_ok());
        assert!(dir.open_dir("driver", false).is_err(), "symlink not followed");
        assert!(dir.lstat("driver").unwrap().file_type().is_symlink());
        assert_eq!(failure(libc::ENODATA).state, ReadState::Unavailable);
        fs::write(tmp.path().join("big"), vec![b'x'; TEXT_LIMIT + 1]).unwrap();
        assert_eq!(dir.read_text("big").error, libc::EOVERFLOW);
        assert!(Dir::open(tmp.path().join("name")).is_err(), "not a directory");
    }
}

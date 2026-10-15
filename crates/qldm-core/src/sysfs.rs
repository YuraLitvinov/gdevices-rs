//! Bounded, read-only sysfs helpers, ported from `readAttribute` in
//! `src/devices/enumerator.cpp` and `src/devices/directoryscan.h`.
use crate::model::{Attribute, ReadState};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Largest attribute value accepted; one more byte read means overflow.
const ATTRIBUTE_LIMIT: u64 = 4096;

/// Reads one small text attribute without following a final symlink.
pub fn read_attribute(path: impl AsRef<Path>) -> Attribute {
    let mut file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path) {
        Ok(file) => file,
        Err(e) => {
            let error = e.raw_os_error().unwrap_or(libc::EIO);
            let state = match error {
                libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
                libc::ENOENT => ReadState::Unavailable,
                _ => ReadState::Error,
            };
            return Attribute { state, value: String::new(), error };
        }
    };
    let mut buffer = Vec::new();
    if let Err(e) = (&mut file).take(ATTRIBUTE_LIMIT + 1).read_to_end(&mut buffer) {
        let error = e.raw_os_error().unwrap_or(libc::EIO);
        let state = match error {
            libc::ENODEV => ReadState::Removed,
            libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
            _ => ReadState::Error,
        };
        return Attribute { state, value: String::new(), error };
    }
    if buffer.len() as u64 > ATTRIBUTE_LIMIT {
        return Attribute { state: ReadState::Error, value: String::new(), error: libc::EOVERFLOW };
    }
    Attribute { state: ReadState::Available, value: String::from_utf8_lossy(&buffer).trim().to_string(), error: 0 }
}

/// Shell-style `*` / `?` match, as used by `QDirListing` name filters.
fn glob_matches(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.split_first(), text.split_first()) {
        (None, None) => true,
        (Some((b'*', rest)), _) => glob_matches(rest, text) || (!text.is_empty() && glob_matches(pattern, &text[1..])),
        (Some((b'?', rest)), Some((_, t))) => glob_matches(rest, t),
        (Some((p, rest)), Some((t, tail))) if p == t => glob_matches(rest, tail),
        _ => false,
    }
}

/// Sorted names of sub-directories (symlinks to directories included), or an errno.
///
/// Hidden entries, files and broken links are omitted. More than `limit` matches
/// is `EOVERFLOW` and never a partial list; a set `cancel` flag is `ECANCELED`.
pub fn bounded_directory_names(path: impl AsRef<Path>, limit: i64, filters: &[&str], cancel: Option<&AtomicBool>) -> Result<Vec<String>, i32> {
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    if limit < 0 {
        return Err(libc::EINVAL);
    }
    if cancelled() {
        return Err(libc::ECANCELED);
    }
    let path = path.as_ref();
    let mut names = Vec::new();
    for entry in fs::read_dir(path).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))? {
        if cancelled() {
            return Err(libc::ECANCELED);
        }
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        if !filters.is_empty() && !filters.iter().any(|f| glob_matches(f.as_bytes(), name.as_bytes())) {
            continue;
        }
        if names.len() as i64 >= limit {
            return Err(libc::EOVERFLOW);
        }
        names.push(name);
    }
    if cancelled() {
        return Err(libc::ECANCELED);
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn directory_names_follow_cpp_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in ["zeta", "alpha", ".hidden"] {
            fs::create_dir(root.join(d)).unwrap();
        }
        fs::write(root.join("regular"), b"").unwrap();
        symlink("alpha", root.join("linked")).unwrap();
        symlink("regular", root.join("file-link")).unwrap();
        symlink("missing", root.join("broken")).unwrap();

        assert_eq!(bounded_directory_names(root, 3, &[], None), Ok(vec!["alpha".into(), "linked".into(), "zeta".into()]));
        assert_eq!(bounded_directory_names(root, 2, &[], None), Err(libc::EOVERFLOW));
        assert_eq!(bounded_directory_names(root, 1, &["a*"], None), Ok(vec!["alpha".into()]));
        assert_eq!(bounded_directory_names(root, 0, &[], None), Err(libc::EOVERFLOW));
        assert_eq!(bounded_directory_names(root, -1, &[], None), Err(libc::EINVAL));
        fs::create_dir(root.join("empty")).unwrap();
        assert_eq!(bounded_directory_names(root.join("empty"), 0, &[], None), Ok(vec![]));
        let cancel = AtomicBool::new(true);
        assert_eq!(bounded_directory_names(root, 3, &[], Some(&cancel)), Err(libc::ECANCELED));
    }

    #[test]
    fn glob_supports_star_and_question_mark() {
        assert!(glob_matches(b"a*", b"alpha"));
        assert!(glob_matches(b"?lpha", b"alpha"));
        assert!(!glob_matches(b"a*z", b"alpha"));
    }

    #[test]
    fn attribute_states() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("name");
        fs::write(&ok, b"  Touchpad \n").unwrap();
        let a = read_attribute(&ok);
        assert_eq!((a.state, a.value.as_str(), a.error), (ReadState::Available, "Touchpad", 0));

        let missing = read_attribute(dir.path().join("none"));
        assert_eq!((missing.state, missing.error), (ReadState::Unavailable, libc::ENOENT));

        let big = dir.path().join("big");
        fs::write(&big, vec![b'x'; 4097]).unwrap();
        let o = read_attribute(&big);
        assert_eq!((o.state, o.error), (ReadState::Error, libc::EOVERFLOW));
        fs::write(&big, vec![b'x'; 4096]).unwrap();
        assert_eq!(read_attribute(&big).state, ReadState::Available);

        symlink(&ok, dir.path().join("link")).unwrap();
        assert_eq!(read_attribute(dir.path().join("link")).state, ReadState::Error, "final symlink is not followed");

        // A directory opens but cannot be read: EISDIR is a generic Error.
        assert_eq!(read_attribute(dir.path()).state, ReadState::Error);
    }
}

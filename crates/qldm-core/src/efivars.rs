//! UEFI variable directory inventory, ported from `src/devices/efivariables.cpp`.
//!
//! Metadata only: the fixed efivarfs directory is opened without following a symlink, at
//! most [`ENTRY_LIMIT`] entries are examined, and no variable value is ever opened.
use crate::model::Device;
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub const EFIVARS_DIRECTORY: &str = "/sys/firmware/efi/efivars";
pub const ENTRY_LIMIT: usize = 4096;

/// Splits `<Name>-<canonical GUID>`; the GUID is lower-cased. Names may contain hyphens.
pub fn split_efi_variable_filename(filename: &str) -> Option<(String, String)> {
    let guid = filename.get(filename.len().checked_sub(36)?..)?;
    let name = filename.get(..filename.len() - 37)?;
    let separator = filename.as_bytes().get(filename.len() - 37)?;
    let groups: Vec<&str> = guid.split('-').collect();
    let shape_ok = groups.len() == 5 && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()));
    (shape_ok && *separator == b'-' && !name.is_empty()).then(|| (name.to_string(), guid.to_lowercase()))
}

/// Readable label for a variable name. Standard mixed-case spellings are preserved and unknown
/// capital runs stay intact; no vendor meaning is decoded.
pub fn efi_variable_label(name: &str) -> String {
    const PRESERVED: [&str; 6] = ["WiMAX", "WiFi", "NVMe", "PCIe", "IPv4", "IPv6"];
    let chars: Vec<char> = name.chars().collect();
    let preserved_at = |offset: usize| -> Option<&'static str> {
        PRESERVED.iter().copied().find(|word| {
            let end = offset + word.chars().count();
            end <= chars.len() && chars[offset..end].iter().copied().eq(word.chars()) && (end == chars.len() || !chars[end].is_lowercase())
        })
    };
    let separator = |c: char| c.is_whitespace() || c == '_' || c == '-';
    let mut words: Vec<String> = Vec::new();
    let mut offset = 0;
    while offset < chars.len() {
        if separator(chars[offset]) {
            offset += 1;
            continue;
        }
        if let Some(word) = preserved_at(offset) {
            words.push(word.to_string());
            offset += word.chars().count();
            continue;
        }
        let start = offset;
        offset += 1;
        while offset < chars.len() {
            let (previous, current) = (chars[offset - 1], chars[offset]);
            if separator(current) || preserved_at(offset).is_some() {
                break;
            }
            let next_lower = chars.get(offset + 1).is_some_and(|c| c.is_lowercase());
            if (previous.is_lowercase() && current.is_uppercase())
                || (previous.is_uppercase() && current.is_uppercase() && next_lower)
                || (previous.is_ascii_digit() && current.is_uppercase() && next_lower)
            {
                break;
            }
            offset += 1;
        }
        let word: String = chars[start..offset].iter().collect();
        // A known compound joining an identifier and the endorsement-key acronym; other
        // runs such as EDID, UUID and ordinary words like IDLE stay whole.
        if word == "IDEK" {
            words.extend(["ID".to_string(), "EK".to_string()]);
        } else {
            words.push(word);
        }
    }
    let mut label = words.join(" ");
    for prefix in ["Boot", "Driver", "SysPrep"] {
        if let Some(hex) = name.strip_prefix(prefix) {
            if hex.len() == 4 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                label = format!("{} {hex}", if prefix == "SysPrep" { "Sys Prep" } else { prefix });
            }
        }
    }
    label.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Result of scanning the variable directory.
#[derive(Default)]
pub struct EfiScan {
    pub devices: Vec<Device>,
    /// Entries that could not be examined, plus one if the entry limit cut the scan short.
    pub skipped: usize,
}

/// Lists public (world-readable, regular-file) variables in `directory`, sorted by path.
/// A missing or inaccessible directory is normal and yields an empty result.
pub fn scan_efi_variables(directory: &Path, cancel: Option<&AtomicBool>) -> EfiScan {
    let mut scan = EfiScan::default();
    let Ok(handle) = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(directory) else {
        return scan; // EFI/efivarfs may be absent or unavailable to this account.
    };
    // Every lookup goes through the opened directory, so a swapped path cannot redirect it.
    let anchored = format!("/proc/self/fd/{}", handle.as_raw_fd());
    let Ok(entries) = fs::read_dir(&anchored) else { return scan };
    let dir_text = directory.to_string_lossy();
    let mut examined = 0;
    for entry in entries {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        let Ok(entry) = entry else {
            scan.skipped += 1;
            continue;
        };
        examined += 1;
        if examined > ENTRY_LIMIT {
            scan.skipped += 1;
            break;
        }
        let filename = entry.file_name().to_string_lossy().into_owned();
        let Ok(info) = fs::symlink_metadata(format!("{anchored}/{filename}")) else {
            scan.skipped += 1;
            continue;
        };
        // Root execution must not broaden the public inventory. Never follow links.
        if !info.file_type().is_file() || info.mode() & 0o004 == 0 {
            continue;
        }
        let Some((name, guid)) = split_efi_variable_filename(&filename) else { continue };
        let mut device = Device {
            path: format!("{dir_text}/{filename}"),
            parent_path: dir_text.to_string(),
            sysname: filename,
            subsystem: "efivarfs".into(),
            category: "efi",
            hidden: true, // Firmware records follow the virtual/internal visibility toggle.
            name,
            name_source: "UEFI variable filename; contents not read".into(),
            incarnation: format!("{}:{}", info.dev(), info.ino()),
            ..Default::default()
        };
        device.properties.insert("EFI_VENDOR_GUID".into(), guid);
        scan.devices.push(device);
    }
    scan.devices.sort_by(|a, b| a.path.cmp(&b.path));
    scan
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    const GUID: &str = "8BE4DF61-93CA-11D2-AA0D-00E098032B8C";

    #[test]
    fn filenames_split_name_and_lowercase_guid() {
        let (name, guid) = split_efi_variable_filename(&format!("BootCurrent-{GUID}")).unwrap();
        assert_eq!((name.as_str(), guid.as_str()), ("BootCurrent", "8be4df61-93ca-11d2-aa0d-00e098032b8c"));
        let (hyphenated, _) = split_efi_variable_filename(&format!("Vendor-Setting-{}", GUID.to_lowercase())).unwrap();
        assert_eq!(hyphenated, "Vendor-Setting");
        assert!(split_efi_variable_filename("BootCurrent-not-a-guid").is_none());
        assert!(split_efi_variable_filename(&format!("-{GUID}")).is_none(), "name must not be empty");
        assert!(split_efi_variable_filename("é").is_none());
    }

    #[test]
    fn labels_split_words_and_keep_acronyms() {
        for (name, want) in [
            ("BootCurrent", "Boot Current"),
            ("Boot000A", "Boot 000A"),
            ("SecureBoot", "Secure Boot"),
            ("PKDefault", "PK Default"),
            ("WiMAXModuleID", "WiMAX Module ID"),
            ("OfflineUniqueIDEKPubCRC", "Offline Unique ID EK Pub CRC"),
            ("LoaderDevicePartUUID", "Loader Device Part UUID"),
            ("AMITCGPPIVAR", "AMITCGPPIVAR"),
            ("EDID1Nv", "EDID1 Nv"),
            ("SmbiosV3EntryPointTable", "Smbios V3 Entry Point Table"),
            ("SysPrep0001", "Sys Prep 0001"),
            ("Driver00AB", "Driver 00AB"),
            ("Vendor-Setting", "Vendor Setting"),
        ] {
            assert_eq!(efi_variable_label(name), want, "{name}");
        }
    }

    #[test]
    fn directory_scan_is_metadata_only_public_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = |name: &str, mode: u32| {
            let path = root.join(name);
            fs::write(&path, b"secret").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        };
        file(&format!("Zeta-{GUID}"), 0o644);
        file(&format!("Alpha-{GUID}"), 0o604);
        file(&format!("Private-{GUID}"), 0o600);
        file("NoGuid", 0o644);
        symlink(root.join(format!("Alpha-{GUID}")), root.join(format!("Link-{GUID}"))).unwrap();
        let scan = scan_efi_variables(root, None);
        let names: Vec<_> = scan.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Zeta"], "only public regular files with a GUID, sorted by path");
        let alpha = &scan.devices[0];
        assert_eq!((alpha.category, alpha.subsystem.as_str(), alpha.hidden), ("efi", "efivarfs", true));
        assert_eq!(alpha.properties["EFI_VENDOR_GUID"], GUID.to_lowercase());
        assert_eq!(alpha.parent_path, root.to_string_lossy());
    }

    #[test]
    fn entry_limit_and_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..ENTRY_LIMIT + 5 {
            fs::write(dir.path().join(format!("V{i}-{GUID}")), b"").unwrap();
        }
        let scan = scan_efi_variables(dir.path(), None);
        assert!(scan.devices.len() <= ENTRY_LIMIT);
        assert_eq!(scan.skipped, 1);
        let gone = scan_efi_variables(&dir.path().join("missing"), None);
        assert!(gone.devices.is_empty() && gone.skipped == 0);
        let link = dir.path().join("dirlink");
        symlink(dir.path(), &link).unwrap();
        assert!(scan_efi_variables(&link, None).devices.is_empty(), "a directory symlink is not followed");
    }
}

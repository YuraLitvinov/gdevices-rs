//! Secure Boot state and Machine Owner Key certificates, ported from
//! `src/devices/secureboot.cpp`. Everything is read from firmware variables the kernel already
//! exposes; certificates are parsed with `x509-parser` instead of Qt's SSL layer. A truncated
//! or malformed list never yields an apparently complete certificate list.
use crate::dirfd::{available, read_limited};
use crate::model::{Attribute, ReadState};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use x509_parser::prelude::*;

const LIMIT: usize = 1024 * 1024;
/// EFI_CERT_X509_GUID in EFI's little-endian GUID representation.
const X509_GUID: [u8; 16] = [0xa1, 0x59, 0xc0, 0xa5, 0xe4, 0x94, 0xa7, 0x4a, 0x87, 0xb5, 0xab, 0x15, 0x5c, 0x2b, 0xf0, 0x72];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MokCertificate {
    pub subject: String,
    pub issuer: String,
    pub expires: String,
    pub subject_display: String,
    pub issuer_display: String,
}

#[derive(Clone, Debug, Default)]
pub struct MokCertificates {
    pub status: Attribute,
    pub certificates: Vec<MokCertificate>,
    pub other_signatures: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SecureBootInformation {
    pub state: Attribute,
    pub mok: MokCertificates,
    pub state_source: String,
    pub mok_source: String,
}

fn failure(error: i32) -> Attribute {
    let state = match error {
        libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
        libc::ENOENT => ReadState::Unavailable,
        _ => ReadState::Error,
    };
    Attribute { state, value: String::new(), error }
}

/// Reads a firmware file (efivarfs files carry a 4-byte attribute prefix that is dropped).
fn read_binary(path: &Path, efivar: bool) -> Result<Vec<u8>, Attribute> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| failure(e.raw_os_error().unwrap_or(libc::EIO)))?;
    let mut bytes = read_limited(&mut file, LIMIT, &|| false).map_err(failure)?;
    if bytes.len() > LIMIT {
        return Err(failure(libc::EOVERFLOW));
    }
    if efivar {
        if bytes.len() < 4 {
            return Err(failure(libc::EINVAL));
        }
        bytes.drain(..4);
    }
    Ok(bytes)
}

/// `SecureBoot` is exactly one byte, 0 or 1; anything else is an error, never "disabled".
pub fn parse_secure_boot(payload: &[u8]) -> Attribute {
    match payload {
        [0] => available("disabled"),
        [1] => available("enabled"),
        _ => failure(libc::EINVAL),
    }
}

/// Short attribute names in the order Qt reports them (alphabetical, case-sensitive).
fn attribute_name(oid: &str) -> String {
    match oid {
        "2.5.4.3" => "CN",
        "2.5.4.6" => "C",
        "2.5.4.7" => "L",
        "2.5.4.8" => "ST",
        "2.5.4.9" => "street",
        "2.5.4.10" => "O",
        "2.5.4.11" => "OU",
        "1.2.840.113549.1.9.1" => "emailAddress",
        "0.9.2342.19200300.100.1.25" => "DC",
        "2.5.4.5" => "serialNumber",
        other => other,
    }
    .to_string()
}

fn name_fields(name: &X509Name<'_>) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    for attribute in name.iter_attributes() {
        let value = attribute.as_str().map(str::to_string).unwrap_or_default();
        fields.push((attribute_name(&attribute.attr_type().to_id_string()), value));
    }
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    fields
}

/// `O=…, CN=…` style text: every attribute, grouped by sorted name, values simplified.
fn full_name(name: &X509Name<'_>) -> String {
    name_fields(name).iter().map(|(key, value)| format!("{key}={}", value.split_whitespace().collect::<Vec<_>>().join(" "))).collect::<Vec<_>>().join(", ")
}

/// Organizational unit, organization and common name, which identify distro certificates
/// better than a generic CN, joined with an em dash.
fn display_name(name: &X509Name<'_>) -> String {
    let fields = name_fields(name);
    let mut parts: Vec<String> = Vec::new();
    for wanted in ["OU", "O", "CN"] {
        for (_, value) in fields.iter().filter(|(key, _)| key == wanted) {
            let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
            if !text.is_empty() && !parts.contains(&text) {
                parts.push(text);
            }
        }
    }
    parts.join(" — ")
}

fn expiry_iso(time: &ASN1Time) -> String {
    let t = time.to_datetime();
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
}

fn invalid(error: i32) -> MokCertificates {
    MokCertificates { status: failure(error), ..Default::default() }
}

/// Parses concatenated `EFI_SIGNATURE_LIST`s: X.509 entries become certificates, other
/// signature types (hashes) are only counted.
pub fn parse_mok_certificates(payload: &[u8]) -> MokCertificates {
    if payload.len() > LIMIT {
        return invalid(libc::EOVERFLOW);
    }
    let mut result = MokCertificates::default();
    let (mut offset, mut lists, mut signatures) = (0usize, 0, 0);
    let le = |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().expect("4 bytes")) as usize;
    while offset < payload.len() {
        lists += 1;
        if lists > 256 {
            return invalid(libc::EOVERFLOW);
        }
        if payload.len() - offset < 28 {
            return invalid(libc::EINVAL);
        }
        let (size, header_size, signature_size) = (le(offset + 16), le(offset + 20), le(offset + 24));
        if size < 28 || size > payload.len() - offset || header_size > size - 28 || signature_size <= 16 {
            return invalid(libc::EINVAL);
        }
        if (size - 28 - header_size) % signature_size != 0 {
            return invalid(libc::EINVAL);
        }
        let is_x509 = payload[offset..offset + 16] == X509_GUID;
        let mut position = offset + 28 + header_size;
        while position < offset + size {
            signatures += 1;
            if signatures > 1024 {
                return invalid(libc::EOVERFLOW);
            }
            if !is_x509 {
                result.other_signatures += 1;
                position += signature_size;
                continue;
            }
            if result.certificates.len() >= 256 || signature_size > 65536 {
                return invalid(libc::EOVERFLOW);
            }
            // Skip the signature-owner GUID.
            let der = &payload[position + 16..position + signature_size];
            let Ok((rest, certificate)) = X509Certificate::from_der(der) else { return invalid(libc::EINVAL) };
            // Re-encoding must reproduce the input exactly: no trailing or ignored bytes.
            if !rest.is_empty() || certificate.as_ref() != der {
                return invalid(libc::EINVAL);
            }
            result.certificates.push(MokCertificate {
                subject: full_name(certificate.subject()),
                issuer: full_name(certificate.issuer()),
                expires: expiry_iso(&certificate.validity().not_after),
                subject_display: display_name(certificate.subject()),
                issuer_display: display_name(certificate.issuer()),
            });
            position += signature_size;
        }
        offset += size;
    }
    result.status = available("");
    result
}

/// Reads Secure Boot and the MOK list below `efi_root` (normally `/sys/firmware/efi`).
pub fn collect_secure_boot_information_at(efi_root: &Path) -> SecureBootInformation {
    let mut result = SecureBootInformation::default();
    let variables = efi_root.join("efivars");
    let source = |name: &str| -> PathBuf { variables.join(name) };
    let state_path = source("SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c");
    result.state_source = state_path.to_string_lossy().into_owned();
    result.state = match read_binary(&state_path, true) {
        Ok(bytes) => parse_secure_boot(&bytes),
        Err(state) => state,
    };
    // The kernel's MOK configuration-table copy contains the complete runtime list.
    let table_path = efi_root.join("mok-variables/MokListRT");
    result.mok_source = table_path.to_string_lossy().into_owned();
    let mut payload = read_binary(&table_path, false);
    let table_failure = payload.as_ref().err().cloned();
    if let Some(table) = table_failure.filter(|a| a.error == libc::ENOENT || a.state == ReadState::PermissionDenied) {
        let variable_path = source("MokListRT-605dab50-e046-4300-abb6-3dd810dd8b23");
        result.mok_source = variable_path.to_string_lossy().into_owned();
        payload = read_binary(&variable_path, true);
        if matches!(&payload, Err(a) if a.error == libc::ENOENT) && table.state == ReadState::PermissionDenied {
            payload = Err(table);
            result.mok_source = table_path.to_string_lossy().into_owned();
        }
        // A split runtime list cannot be presented as a complete certificate list.
        let split = read_binary(&source("MokListRT1-605dab50-e046-4300-abb6-3dd810dd8b23"), true);
        let split_exists = match &split {
            Ok(_) => true,
            Err(a) => a.error != libc::ENOENT,
        };
        if payload.is_ok() && split_exists {
            payload = Err(failure(libc::EOVERFLOW));
        }
    }
    result.mok = match payload {
        Ok(bytes) => parse_mok_certificates(&bytes),
        Err(status) => MokCertificates { status, ..Default::default() },
    };
    result
}

pub fn collect_secure_boot_information() -> SecureBootInformation {
    collect_secure_boot_information_at(Path::new("/sys/firmware/efi"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_DER: &str = "MIIBGzCBwaADAgECAgEBMAoGCCqGSM49BAMCMBcxFTATBgNVBAMMDFFMRE0gZml4dHVyZTAeFw0yMDAxMDEwMDAwMDBaFw0zMDAxMDEwMDAwMDBaMBcxFTATBgNVBAMMDFFMRE0gZml4dHVyZTBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABK9KZsQWrAixDhWcxerBMOstANWdiM9RdfPf+25bsZX6Hgz+pMLR2hpgxDWrLCpmGDVZNYAAya1/3hrgz6/Tm9gwCgYIKoZIzj0EAwIDSQAwRgIhAPjy8A6F++gXv0CNHw3xd5VwAHI0/Tpt5Bifyhf1KK5AAiEA839IXhcKhZWx329x4fNC1e8wt25T7EBf5Hjzq6wDviw=";

    fn base64(text: &str) -> Vec<u8> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let (mut buffer, mut bits) = (0u32, 0);
        for c in text.bytes().filter(|c| *c != b'=') {
            buffer = (buffer << 6) | ALPHABET.iter().position(|a| *a == c).unwrap() as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buffer >> bits) as u8);
                buffer &= (1 << bits) - 1;
            }
        }
        out
    }

    fn signature_list(guid: &[u8], signature: &[u8]) -> Vec<u8> {
        let mut list = guid.to_vec();
        for value in [28 + 16 + signature.len(), 0, 16 + signature.len()] {
            list.extend_from_slice(&(value as u32).to_le_bytes());
        }
        list.extend_from_slice(&[0; 16]); // Signature-owner GUID.
        list.extend_from_slice(signature);
        list
    }

    #[test]
    fn secure_boot_is_exactly_one_byte() {
        assert_eq!(parse_secure_boot(&[1]).value, "enabled");
        assert_eq!(parse_secure_boot(&[0]).value, "disabled");
        for bad in [&[][..], &[2][..], &[0, 0, 0, 0, 0][..]] {
            assert_eq!(parse_secure_boot(bad).state, ReadState::Error);
        }
    }

    #[test]
    fn mok_certificates_keep_owner_issuer_and_expiry_together() {
        let der = base64(FIXTURE_DER);
        let list = signature_list(&X509_GUID, &der);
        let mok = parse_mok_certificates(&list);
        assert_eq!(mok.status.state, ReadState::Available);
        assert_eq!(mok.certificates.len(), 1);
        let certificate = &mok.certificates[0];
        assert!(certificate.subject.contains("QLDM fixture") && certificate.issuer.contains("QLDM fixture"));
        assert_eq!(certificate.subject, "CN=QLDM fixture");
        assert_eq!(certificate.expires, "2030-01-01T00:00:00Z");
        assert_eq!(certificate.subject_display, "QLDM fixture");
        assert_eq!(parse_mok_certificates(&[list.clone(), list.clone()].concat()).certificates.len(), 2, "multiple lists");
    }

    #[test]
    fn malformed_lists_never_look_complete() {
        let der = base64(FIXTURE_DER);
        let list = signature_list(&X509_GUID, &der);
        assert_eq!(parse_mok_certificates(&list[..list.len() - 1]).status.state, ReadState::Error, "truncated");
        assert_eq!(parse_mok_certificates(&signature_list(&X509_GUID, b"invalid DER")).status.state, ReadState::Error);
        let mut trailing = der.clone();
        trailing.push(0);
        assert_eq!(parse_mok_certificates(&signature_list(&X509_GUID, &trailing)).status.state, ReadState::Error, "bytes after the certificate");
        assert_eq!(parse_mok_certificates(&vec![0; LIMIT + 1]).status.error, libc::EOVERFLOW);
        assert_eq!(parse_mok_certificates(&[0; 10]).status.error, libc::EINVAL);
    }

    #[test]
    fn non_certificate_signatures_are_counted_not_decoded() {
        let hashes = parse_mok_certificates(&signature_list(&[0; 16], &[1; 32]));
        assert_eq!(hashes.status.state, ReadState::Available);
        assert!(hashes.certificates.is_empty());
        assert_eq!(hashes.other_signatures, 1);
        assert_eq!(parse_mok_certificates(&[]).status.state, ReadState::Available, "an empty list is a valid empty list");
    }

    fn efi_tree(secure_boot: Option<&[u8]>, table: Option<&[u8]>, variable: Option<&[u8]>, split: bool) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("efivars")).unwrap();
        std::fs::create_dir_all(tmp.path().join("mok-variables")).unwrap();
        let with_attributes = |bytes: &[u8]| [&[7, 0, 0, 0][..], bytes].concat();
        if let Some(b) = secure_boot {
            std::fs::write(tmp.path().join("efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c"), with_attributes(b)).unwrap();
        }
        if let Some(b) = table {
            std::fs::write(tmp.path().join("mok-variables/MokListRT"), b).unwrap();
        }
        if let Some(b) = variable {
            std::fs::write(tmp.path().join("efivars/MokListRT-605dab50-e046-4300-abb6-3dd810dd8b23"), with_attributes(b)).unwrap();
        }
        if split {
            std::fs::write(tmp.path().join("efivars/MokListRT1-605dab50-e046-4300-abb6-3dd810dd8b23"), with_attributes(b"x")).unwrap();
        }
        tmp
    }

    #[test]
    fn collection_prefers_the_configuration_table_and_rejects_split_variables() {
        let list = signature_list(&X509_GUID, &base64(FIXTURE_DER));
        let table = efi_tree(Some(&[1]), Some(&list), None, false);
        let found = collect_secure_boot_information_at(table.path());
        assert_eq!(found.state.value, "enabled");
        assert_eq!(found.mok.certificates.len(), 1);
        assert!(found.mok_source.ends_with("mok-variables/MokListRT"));

        let variable = efi_tree(Some(&[0]), None, Some(&list), false);
        let found = collect_secure_boot_information_at(variable.path());
        assert_eq!((found.state.value.as_str(), found.mok.certificates.len()), ("disabled", 1));
        assert!(found.mok_source.contains("efivars/MokListRT-"), "falls back to the variable");

        let split = efi_tree(Some(&[0]), None, Some(&list), true);
        let found = collect_secure_boot_information_at(split.path());
        assert_eq!(found.mok.status.error, libc::EOVERFLOW);
        assert!(found.mok.certificates.is_empty(), "a split list is never shown as complete");

        let none = efi_tree(None, None, None, false);
        let found = collect_secure_boot_information_at(none.path());
        assert_eq!(found.state.state, ReadState::Unavailable);
        assert_eq!(found.mok.status.state, ReadState::Unavailable);
    }

    #[test]
    fn short_efivar_payloads_are_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("v"), [1, 2, 3]).unwrap();
        assert_eq!(read_binary(&tmp.path().join("v"), true).unwrap_err().error, libc::EINVAL);
        assert_eq!(read_binary(&tmp.path().join("v"), false).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn this_machines_secure_boot_state_is_explicit() {
        let found = collect_secure_boot_information();
        assert!(matches!(found.state.state, ReadState::Available | ReadState::Unavailable | ReadState::PermissionDenied | ReadState::Error));
        if found.state.state == ReadState::Available {
            assert!(matches!(found.state.value.as_str(), "enabled" | "disabled"));
        }
    }
}

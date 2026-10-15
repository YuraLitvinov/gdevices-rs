//! Reads name, version and repository from `project.json` (the single source of truth, as the
//! CMake build did) and exports them as compile-time environment variables.
use std::path::Path;

fn field<'a>(json: &'a str, key: &str) -> &'a str {
    let needle = format!("\"{key}\"");
    let at = json.find(&needle).unwrap_or_else(|| panic!("project.json has no {key}"));
    let rest = &json[at + needle.len()..];
    let start = rest.find('"').expect("string value") + 1;
    let end = start + rest[start..].find('"').expect("closing quote");
    &rest[start..end]
}

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let path = Path::new(&manifest).join("../../project.json");
    println!("cargo:rerun-if-changed={}", path.display());
    let json = std::fs::read_to_string(&path).expect("project.json");
    println!("cargo:rerun-if-env-changed=QLDM_LOCALE_DIR");
    println!("cargo:rustc-env=QLDM_NAME={}", field(&json, "name"));
    println!("cargo:rustc-env=QLDM_VERSION={}", field(&json, "version"));
    println!("cargo:rustc-env=QLDM_REPOSITORY={}", field(&json, "repository"));
}

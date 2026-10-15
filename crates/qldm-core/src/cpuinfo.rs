//! `/proc/cpuinfo` model matching, ported from `src/devices/cpumetadata.cpp`.
use crate::model::{Attribute, ReadState};
use std::collections::HashMap;
use std::io::Read;

const LIMIT: u64 = 4 * 1024 * 1024;

/// Per-processor models keyed by processor number; never a global board string.
pub fn parse_cpu_models(cpuinfo: &str) -> HashMap<u32, String> {
    let mut models = HashMap::new();
    let mut record: HashMap<&str, &str> = HashMap::new();
    let mut commit = |record: &HashMap<&str, &str>| {
        let Some(cpu) = record.get("processor").and_then(|v| v.parse::<u32>().ok()) else { return };
        for key in ["model name", "cpu model", "uarch"] {
            if let Some(value) = record.get(key).filter(|v| !v.is_empty()) {
                models.insert(cpu, value.to_string());
                break;
            }
        }
    };
    for line in cpuinfo.split('\n') {
        if line.trim().is_empty() {
            commit(&record);
            record.clear();
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        // Only per-processor keys. A board's global Hardware string never reaches a CPU.
        if matches!(key, "processor" | "model name" | "cpu model" | "uarch") {
            record.insert(key, value.trim());
        }
    }
    commit(&record);
    models
}

/// Reads `/proc/cpuinfo`; a file over 4 MiB is an `EOVERFLOW` error, never truncated metadata.
pub fn read_cpuinfo() -> Attribute {
    let file = match std::fs::File::open("/proc/cpuinfo") {
        Ok(f) => f,
        Err(e) => {
            let state = if e.kind() == std::io::ErrorKind::PermissionDenied { ReadState::PermissionDenied } else { ReadState::Unavailable };
            return Attribute { state, value: String::new(), error: e.raw_os_error().unwrap_or(0) };
        }
    };
    let mut bytes = Vec::new();
    match file.take(LIMIT + 1).read_to_end(&mut bytes) {
        Err(e) => Attribute { state: ReadState::Error, value: String::new(), error: e.raw_os_error().unwrap_or(libc::EIO) },
        Ok(_) if bytes.len() as u64 > LIMIT => Attribute { state: ReadState::Error, value: String::new(), error: libc::EOVERFLOW },
        Ok(_) => Attribute { state: ReadState::Available, value: String::from_utf8_lossy(&bytes).into_owned(), error: 0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_match_processor_numbers_not_parse_order() {
        let models = parse_cpu_models(
            "processor : 1\nmodel name : Different CPU\n\n\
             processor : 0\nmodel name : Intel(R) Core(TM) Ultra X7 358H\n\n\
             processor : 2\nHardware : Board name\n",
        );
        assert_eq!(models.get(&0).map(String::as_str), Some("Intel(R) Core(TM) Ultra X7 358H"));
        assert_eq!(models.get(&1).map(String::as_str), Some("Different CPU"));
        assert!(!models.contains_key(&2), "global board name must not become a CPU model");
    }

    #[test]
    fn fallback_keys_and_invalid_processor() {
        let models = parse_cpu_models("processor : 3\ncpu model : POWER9\n\nprocessor : x\nmodel name : bad\n");
        assert_eq!(models.get(&3).map(String::as_str), Some("POWER9"));
        assert_eq!(models.len(), 1);
    }

    #[test]
    fn reads_real_cpuinfo() {
        let info = read_cpuinfo();
        assert_eq!(info.state, ReadState::Available);
    }
}

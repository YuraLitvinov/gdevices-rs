//! Runs the real helper binary against this machine's video nodes.
use qldm_core::camera::collect_camera_properties;
use qldm_core::dirfd::Dir;
use qldm_core::model::ReadState;
use std::path::Path;

#[test]
fn helper_reports_each_video_node_or_a_precise_failure() {
    let helper = Path::new(env!("CARGO_BIN_EXE_qldm-camera-helper"));
    let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") else { return };
    let mut checked = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("video") {
            continue;
        }
        let sysfs = std::fs::canonicalize(entry.path()).unwrap().to_string_lossy().into_owned();
        let node = format!("/dev/{name}");
        let dir = Dir::open(&sysfs).unwrap();
        let found = collect_camera_properties(&dir, &sysfs, &node, helper, None);
        checked += 1;
        match found.state.state {
            ReadState::Available => {
                assert!(found.report.is_object(), "{node}");
                if found.applicable {
                    let modes = found.report["modes"].as_array().expect("modes array");
                    assert!(!modes.is_empty(), "{node}: a capture camera lists at least one mode");
                    assert!(modes.iter().all(|m| m["format"].as_u64().is_some() && m["queue"].is_string()), "{node}");
                } else {
                    assert_eq!(found.report["applicable"], false, "{node}");
                }
            }
            // No access to the node, or it is not a capture device: explicit, never a hang.
            other => assert!(
                matches!(other, ReadState::PermissionDenied | ReadState::Unsupported | ReadState::Removed | ReadState::Error),
                "{node}: {:?}",
                found.state
            ),
        }
    }
    eprintln!("checked {checked} video nodes");
}

#[test]
fn a_helper_that_never_answers_is_killed_within_the_budget() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("hang");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let dir = Dir::open(tmp.path()).unwrap();
    let started = std::time::Instant::now();
    let found = retry_text_busy(|| collect_camera_properties(&dir, &tmp.path().to_string_lossy(), "/dev/video0", &script, None));
    assert_eq!(found.state.error, libc::ETIMEDOUT);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    // A cancelled wait is also prompt and leaves no stuck helper behind.
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let again = collect_camera_properties(&dir, &tmp.path().to_string_lossy(), "/dev/video0", &script, Some(&cancel));
    assert!(matches!(again.state.error, libc::ECANCELED | libc::EAGAIN));
}

#[test]
fn a_helper_with_bad_output_is_a_protocol_error() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("noise");
    std::fs::write(&script, "#!/bin/sh\necho 'not json'\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let dir = Dir::open(tmp.path()).unwrap();
    let found = retry_text_busy(|| collect_camera_properties(&dir, &tmp.path().to_string_lossy(), "/dev/video0", &script, None));
    assert_eq!(found.state.error, libc::EPROTO);
}

/// Executing a script right after writing it can fail with ETXTBSY when a parallel test forks while
/// the write handle is still open; that is a test-harness race, not a product error, so retry.
fn retry_text_busy(mut run: impl FnMut() -> qldm_core::camera::CameraProperties) -> qldm_core::camera::CameraProperties {
    for _ in 0..20 {
        let found = run();
        if found.state.error != libc::ETXTBSY {
            return found;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    run()
}

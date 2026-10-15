//! Short-lived helper process for V4L2 queries; see `qldm_core::camera`.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(qldm_core::camera::run_camera_query_helper(&args));
}

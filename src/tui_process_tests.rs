#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::process_alive_via_ps;
#[test]
fn process_exists_recognizes_current_process() {
    assert!(super::process_exists(std::process::id()));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn process_alive_via_ps_recognizes_current_process() {
    assert!(process_alive_via_ps(std::process::id()));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn exited_process_is_not_alive_via_ps() {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("short-lived child starts");
    let exited_pid = child.id();
    child.wait().expect("short-lived child exits");

    assert!(!process_alive_via_ps(exited_pid));
}

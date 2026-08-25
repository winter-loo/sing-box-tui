use super::process_is_alive;

#[test]
fn current_process_is_alive() {
    assert!(process_is_alive(std::process::id()));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn exited_process_is_not_alive() {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("short-lived child starts");
    let exited_pid = child.id();
    child.wait().expect("short-lived child exits");

    assert!(!process_is_alive(exited_pid));
}

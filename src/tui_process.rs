#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Command, Stdio};

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn process_exists(pid: u32) -> bool {
    let exists = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    exists && !process_is_zombie(pid)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_is_zombie(pid: u32) -> bool {
    let Ok(output) = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim_start()
        .starts_with('Z')
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(super) fn process_alive_via_ps(pid: u32) -> bool {
    let Ok(output) = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    else {
        // Cannot inspect the process list; assume alive so the caller still attempts the kill.
        return true;
    };
    let stat = String::from_utf8_lossy(&output.stdout);
    if !stat.trim().is_empty() {
        return !stat.trim_start().starts_with('Z');
    }
    // `ps -p` exits non-zero with empty stdout when the PID no longer exists. A real
    // inspection failure includes a diagnostic; stay conservative only in that case.
    !output.status.success() && !output.stderr.is_empty()
}

#[cfg(windows)]
pub(super) fn process_exists(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return false;
    }
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    let mut exit_code = 0_u32;
    let alive = unsafe { GetExitCodeProcess(handle, &mut exit_code).is_ok() }
        && exit_code == STILL_ACTIVE.0 as u32;
    let _ = unsafe { CloseHandle(handle) };
    alive
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub(super) fn process_exists(_pid: u32) -> bool {
    false
}

#[cfg(test)]
#[path = "tui_process_tests.rs"]
mod tests;

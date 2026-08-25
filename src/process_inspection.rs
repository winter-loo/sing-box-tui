#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessStatus {
    Alive,
    Exited,
    Unknown,
}

/// Reports confirmed liveness; inspection failures are not considered alive.
pub(crate) fn process_is_alive(pid: u32) -> bool {
    process_status(pid) == ProcessStatus::Alive
}

/// Conservatively reports liveness for destructive lifecycle operations.
///
/// An inspection failure remains possibly alive so callers do not mistake it
/// for confirmed process exit and then reuse or signal the wrong PID.
pub(crate) fn process_may_be_alive(pid: u32) -> bool {
    process_status(pid) != ProcessStatus::Exited
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_status(pid: u32) -> ProcessStatus {
    let Ok(output) = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    else {
        return ProcessStatus::Unknown;
    };
    let stat = String::from_utf8_lossy(&output.stdout);
    if !stat.trim().is_empty() {
        return if stat.trim_start().starts_with('Z') {
            ProcessStatus::Exited
        } else {
            ProcessStatus::Alive
        };
    }
    if !output.status.success() && !output.stderr.is_empty() {
        ProcessStatus::Unknown
    } else {
        ProcessStatus::Exited
    }
}

#[cfg(windows)]
fn process_status(pid: u32) -> ProcessStatus {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return ProcessStatus::Exited;
    }
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return ProcessStatus::Exited;
    };
    let mut exit_code = 0_u32;
    let alive = unsafe { GetExitCodeProcess(handle, &mut exit_code).is_ok() }
        && exit_code == STILL_ACTIVE.0 as u32;
    let _ = unsafe { CloseHandle(handle) };
    if alive {
        ProcessStatus::Alive
    } else {
        ProcessStatus::Exited
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn process_status(_pid: u32) -> ProcessStatus {
    ProcessStatus::Exited
}

#[cfg(test)]
#[path = "process_inspection_tests.rs"]
mod tests;

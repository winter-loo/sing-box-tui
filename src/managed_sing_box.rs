#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::collections::BTreeSet;
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
#[cfg(windows)]
use serde_json::Value;

use crate::config::inspect_tun_config;
#[cfg(target_os = "macos")]
use crate::macos_privileged_helper;
use crate::process_command::{command_program_name_matches, command_tokens};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::process_inspection::process_may_be_alive as process_alive_via_ps;
#[cfg(windows)]
use crate::process_inspection::process_may_be_alive as process_exists;
struct SingBoxRestartResult {
    started_pid: u32,
    child: Option<Child>,
    elevated: bool,
    privileged_helper: bool,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const SING_BOX_START_MAX_ATTEMPTS: usize = 9;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const SING_BOX_START_RETRY_BACKOFF: Duration = Duration::from_millis(200);
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const SING_BOX_STARTUP_GRACE: Duration = Duration::from_millis(500);
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const SING_BOX_STOP_GRACE: Duration = Duration::from_secs(3);

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn resolve_sing_box_executable(executable: &Path) -> Result<PathBuf> {
    resolve_sing_box_executable_from_path(executable, env::var_os("PATH").as_deref())
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn resolve_sing_box_executable_from_path(
    executable: &Path,
    search_path: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    let executable_text = executable.to_string_lossy();
    let explicit_path = executable.is_absolute()
        || path_text_has_directory(&executable_text)
        || executable
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty());
    if explicit_path {
        return executable.canonicalize().with_context(|| {
            format!(
                "failed to resolve sing-box executable {}",
                executable.display()
            )
        });
    }

    let search_path = search_path.with_context(|| {
        format!(
            "cannot resolve {} because PATH is not set",
            executable.display()
        )
    })?;
    for directory in env::split_paths(search_path) {
        for candidate in executable_candidates_in_directory(&directory, executable) {
            if executable_candidate_is_runnable(&candidate) {
                return candidate.canonicalize().with_context(|| {
                    format!(
                        "failed to resolve sing-box executable candidate {}",
                        candidate.display()
                    )
                });
            }
        }
    }
    bail!(
        "failed to resolve sing-box executable {}: program was not found in PATH",
        executable.display()
    )
}

#[cfg(windows)]
fn executable_candidates_in_directory(directory: &Path, executable: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![directory.join(executable)];
    if executable.extension().is_none() {
        let extensions = env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        for extension in extensions.split(';').filter(|value| !value.is_empty()) {
            let mut name = executable.as_os_str().to_os_string();
            name.push(extension);
            candidates.push(directory.join(name));
        }
    }
    candidates
}

#[cfg(not(windows))]
fn executable_candidates_in_directory(directory: &Path, executable: &Path) -> Vec<PathBuf> {
    vec![directory.join(executable)]
}

#[cfg(unix)]
fn executable_candidate_is_runnable(candidate: &Path) -> bool {
    candidate
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn executable_candidate_is_runnable(candidate: &Path) -> bool {
    candidate.is_file()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn restart_sing_box_for_config(
    executable: &Path,
    config_path: &Path,
    elevate: bool,
) -> Result<SingBoxRestartResult> {
    let executable = resolve_sing_box_executable(executable)?;
    let config_path = config_path.canonicalize().with_context(|| {
        format!(
            "failed to resolve sing-box config {}",
            config_path.display()
        )
    })?;
    #[cfg(target_os = "macos")]
    if should_use_macos_privileged_helper(macos_privileged_helper::helper_available()) {
        let started_pid = macos_privileged_helper::restart(&config_path)?;
        return Ok(SingBoxRestartResult {
            started_pid,
            child: None,
            elevated: true,
            privileged_helper: true,
        });
    }
    let existing_pids = find_sing_box_run_pids_for_config(&executable, &config_path)?;
    if !existing_pids.is_empty() {
        bail!(
            "refusing to replace external sing-box process(es) {:?}; ManagedSingBox only manages processes it started",
            existing_pids
        );
    }

    let use_sudo = elevate && tun_helper_needs_sudo();
    let log_path = sing_box_process_log_path(&config_path);
    let mut child = spawn_sing_box_with_bind_retry(&executable, &config_path, &log_path, || {
        sing_box_run_command(&executable, &config_path, use_sudo)
    })?;
    // When elevated through sudo, the direct child is the sudo wrapper. Resolve the actual
    // sing-box pid so shutdown can signal it directly (a root-owned process cannot be killed
    // by a non-root TUI without going through sudo).
    let started_pid = if use_sudo {
        resolve_new_sing_box_pid_or_cleanup(&executable, &config_path, &existing_pids, &mut child)?
    } else {
        child.id()
    };
    Ok(SingBoxRestartResult {
        started_pid,
        child: Some(child),
        elevated: use_sudo,
        privileged_helper: false,
    })
}

#[cfg(target_os = "macos")]
fn should_use_macos_privileged_helper(helper_available: bool) -> bool {
    helper_available
}

#[cfg(windows)]
fn restart_sing_box_for_config(
    executable: &Path,
    config_path: &Path,
    _elevate: bool,
) -> Result<SingBoxRestartResult> {
    let executable = resolve_sing_box_executable(executable)?;
    let config_path = config_path.canonicalize().with_context(|| {
        format!(
            "failed to resolve sing-box config {}",
            config_path.display()
        )
    })?;
    let existing_pids = find_sing_box_run_pids_for_config(&executable, &config_path)?;
    if !existing_pids.is_empty() {
        bail!(
            "refusing to replace external sing-box process(es) {:?}; ManagedSingBox only manages processes it started",
            existing_pids
        );
    }

    let log_path = sing_box_process_log_path(&config_path);
    let child = spawn_sing_box_with_bind_retry(&executable, &config_path, &log_path, || {
        let mut command = Command::new(&executable);
        command.arg("run").arg("--config").arg(&config_path);
        command
    })?;
    let started_pid = child.id();
    Ok(SingBoxRestartResult {
        started_pid,
        child: Some(child),
        elevated: false,
        privileged_helper: false,
    })
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn restart_sing_box_for_config(
    _executable: &Path,
    _config_path: &Path,
    _elevate: bool,
) -> Result<SingBoxRestartResult> {
    bail!("automatic sing-box restart is only available on Windows, macOS, and Linux")
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn spawn_sing_box_with_bind_retry<F>(
    executable: &Path,
    config_path: &Path,
    log_path: &Path,
    mut make_command: F,
) -> Result<Child>
where
    F: FnMut() -> Command,
{
    for attempt in 1..=SING_BOX_START_MAX_ATTEMPTS {
        let log_offset = fs::metadata(log_path).map(|value| value.len()).unwrap_or(0);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| {
                format!("failed to open sing-box process log {}", log_path.display())
            })?;
        let stderr = log.try_clone().with_context(|| {
            format!(
                "failed to clone sing-box process log {}",
                log_path.display()
            )
        })?;
        let mut command = make_command();
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start {} run --config {}",
                    executable.display(),
                    config_path.display()
                )
            })?;
        std::thread::sleep(SING_BOX_STARTUP_GRACE);
        let status = match child.try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => return Ok(child),
            Err(error) => {
                let error = anyhow::Error::new(error)
                    .context("failed to inspect restarted sing-box process");
                let cleanup = cleanup_uncommitted_sing_box_child(&mut child);
                return Err(startup_error_with_cleanup(error, cleanup));
            }
        };

        let attempt_log = read_text_from_offset(log_path, log_offset).with_context(|| {
            format!(
                "failed to inspect attempt {attempt} output after {executable:?} exited with {status}"
            )
        })?;
        let bind_conflict = sing_box_log_reports_bind_conflict(&attempt_log);
        if bind_conflict && attempt < SING_BOX_START_MAX_ATTEMPTS {
            std::thread::sleep(SING_BOX_START_RETRY_BACKOFF);
            continue;
        }

        let log_excerpt = sing_box_start_log_excerpt(&attempt_log)
            .map(|value| format!("; last log line: {value}"))
            .unwrap_or_default();
        let retry_note = if bind_conflict {
            format!(" after {attempt} startup attempts")
        } else {
            String::new()
        };
        bail!(
            "{} exited immediately with {status}{retry_note}{log_excerpt}; see {}",
            executable.display(),
            log_path.display()
        );
    }
    unreachable!("sing-box startup loop returns or fails on every attempt")
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn read_text_from_offset(path: &Path, offset: u64) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to read process log {}", path.display()))?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(offset.min(length)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn sing_box_log_reports_bind_conflict(log: &str) -> bool {
    let log = log.to_ascii_lowercase();
    log.contains("bind:")
        && (log.contains("address already in use")
            || log.contains("only one usage of each socket address"))
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn sing_box_start_log_excerpt(log: &str) -> Option<String> {
    let line = log.lines().rev().find(|line| !line.trim().is_empty())?;
    let mut escaped = String::new();
    for character in line.trim().chars() {
        escaped.extend(character.escape_default());
        if escaped.len() >= 240 {
            escaped.truncate(240);
            escaped.push('…');
            break;
        }
    }
    (!escaped.is_empty()).then_some(escaped)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sing_box_run_command(executable: &Path, config_path: &Path, use_sudo: bool) -> Command {
    if use_sudo {
        let mut command = Command::new("sudo");
        command
            .arg("-n")
            .arg(executable)
            .arg("run")
            .arg("--config")
            .arg(config_path);
        command
    } else {
        let mut command = Command::new(executable);
        command.arg("run").arg("--config").arg(config_path);
        command
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_new_sing_box_pid(
    executable: &Path,
    config_path: &Path,
    exclude: &[u32],
    wrapper_pid: u32,
) -> Result<u32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pids = find_sing_box_run_pids_for_config(executable, config_path)?;
        let descendants = process_descendant_pids(wrapper_pid)?;
        if let Some(pid) = select_owned_sing_box_pid(&pids, exclude, wrapper_pid, &descendants) {
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            bail!(
                "sing-box did not appear in the process list after an elevated start; see {}",
                sing_box_process_log_path(config_path).display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn select_owned_sing_box_pid(
    matching_pids: &[u32],
    excluded_pids: &[u32],
    wrapper_pid: u32,
    descendant_pids: &[u32],
) -> Option<u32> {
    matching_pids.iter().copied().find(|pid| {
        !excluded_pids.contains(pid) && (*pid == wrapper_pid || descendant_pids.contains(pid))
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_new_sing_box_pid_or_cleanup(
    executable: &Path,
    config_path: &Path,
    previous_pids: &[u32],
    child: &mut Child,
) -> Result<u32> {
    match resolve_new_sing_box_pid(executable, config_path, previous_pids, child.id()) {
        Ok(pid) => Ok(pid),
        Err(error) => {
            let cleanup = cleanup_uncommitted_sing_box_child(child);
            Err(startup_error_with_cleanup(error, cleanup))
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_descendant_pids(root_pid: u32) -> Result<Vec<u32>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .context("failed to list process relationships with ps")?;
    if !output.status.success() {
        bail!(
            "ps exited with {} while listing process relationships",
            output.status
        );
    }

    let relationships = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let parent_pid = fields.next()?.parse::<u32>().ok()?;
            Some((pid, parent_pid))
        })
        .collect::<Vec<_>>();
    let mut descendants = Vec::new();
    let mut seen = BTreeSet::new();
    let mut frontier = vec![root_pid];
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for (pid, parent_pid) in &relationships {
            if frontier.contains(parent_pid) && seen.insert(*pid) {
                descendants.push(*pid);
                next.push(*pid);
            }
        }
        frontier = next;
    }
    Ok(descendants)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_sing_box_child_descendants(child_pid: u32) -> Result<()> {
    let descendants = process_descendant_pids(child_pid)?;
    let mut failures = Vec::new();
    for pid in descendants.into_iter().rev() {
        if let Err(error) = stop_sing_box_pid_escalating(pid) {
            failures.push(format!("failed to stop descendant pid {pid}: {error:#}"));
        }
    }
    let remaining = process_descendant_pids(child_pid)?;
    if !remaining.is_empty() {
        failures.push(format!("descendant process(es) still alive: {remaining:?}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("; "))
    }
}

#[cfg(windows)]
fn stop_sing_box_child_descendants(_child_pid: u32) -> Result<()> {
    // `taskkill /T` in `stop_sing_box_pid` terminates the whole child process tree.
    Ok(())
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn cleanup_uncommitted_sing_box_child(child: &mut Child) -> Result<()> {
    let child_pid = child.id();
    let mut failures = Vec::new();
    let child_already_exited = child.try_wait().ok().flatten().is_some();

    if let Err(error) = stop_sing_box_child_descendants(child_pid) {
        failures.push(format!(
            "failed to stop startup child descendants: {error:#}"
        ));
    }

    if !child_already_exited {
        match stop_sing_box_pid_escalating(child_pid) {
            Ok(()) => {
                if let Err(error) = child.wait() {
                    failures.push(format!(
                        "failed to reap startup child pid {child_pid}: {error}"
                    ));
                }
            }
            Err(error) => {
                if child.try_wait().ok().flatten().is_none() {
                    failures.push(format!(
                        "failed to stop startup child pid {child_pid}: {error:#}"
                    ));
                }
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("; "))
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn startup_error_with_cleanup(error: anyhow::Error, cleanup: Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => error.context(format!(
            "failed to clean up an uncommitted sing-box start: {cleanup_error:#}"
        )),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_sing_box_pid_escalating(pid: u32) -> Result<()> {
    match stop_sing_box_pid(pid) {
        Ok(()) => Ok(()),
        Err(error) => {
            // `process_exists` uses `kill -0`, which reports EPERM (and therefore false) for a
            // root-owned process from a non-root user, so it cannot tell "already gone" apart from
            // "still running but owned by root". Use `ps` instead, which lists processes regardless
            // of ownership, so an elevated sing-box is still killed through sudo.
            if !process_alive_via_ps(pid) {
                return Ok(());
            }
            stop_sing_box_pid_sudo(pid)
                .with_context(|| format!("unprivileged stop attempt failed first: {error:#}"))
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_sing_box_pid_sudo(pid: u32) -> Result<()> {
    stop_sing_box_pid_sudo_verified(pid, |_| Ok(true))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_sing_box_pid_sudo_verified<F>(pid: u32, mut verify_identity: F) -> Result<()>
where
    F: FnMut(u32) -> Result<bool>,
{
    stop_sing_box_pid_with_escalation(
        pid,
        SING_BOX_STOP_GRACE,
        &mut verify_identity,
        sudo_signal_sing_box_process,
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_sing_box_pid_with_escalation<V, S>(
    pid: u32,
    grace: Duration,
    mut verify_identity: V,
    mut send_signal: S,
) -> Result<()>
where
    V: FnMut(u32) -> Result<bool>,
    S: FnMut(u32, bool) -> Result<()>,
{
    if !process_alive_via_ps(pid) {
        return Ok(());
    }
    let start_token = process_start_token(pid);
    if verify_process_instance_before_force_stop(pid, start_token, &mut verify_identity)
        .context("refusing to signal an unverified elevated sing-box process")?
        == ProcessInstanceVerification::Gone
    {
        return Ok(());
    }
    send_signal(pid, false)?;
    if !process_alive_via_ps(pid) {
        return Ok(());
    }
    if wait_for_processes_to_exit_with_timeout(&[pid], grace).is_ok() {
        return Ok(());
    }
    if !process_alive_via_ps(pid) {
        return Ok(());
    }

    let Some(start_token) = start_token else {
        bail!(
            "elevated sing-box process {pid} did not exit after SIGTERM; refusing to force stop because its process instance could not be recorded"
        );
    };
    if verify_process_instance_before_force_stop(pid, Some(start_token), &mut verify_identity)
        .context("refusing to force stop an unverified elevated sing-box process")?
        == ProcessInstanceVerification::Gone
    {
        return Ok(());
    }

    send_signal(pid, true)?;
    wait_for_processes_to_exit_with_timeout(&[pid], grace)
        .with_context(|| format!("elevated sing-box process {pid} did not exit after SIGKILL"))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sudo_signal_sing_box_process(pid: u32, force: bool) -> Result<()> {
    let mut command = Command::new("sudo");
    command.args(["-n", "kill"]);
    if force {
        command.arg("-9");
    }
    let output = command.arg(pid.to_string()).output().with_context(|| {
        format!(
            "failed to {} elevated sing-box process {pid}",
            if force { "force stop" } else { "stop" }
        )
    })?;
    if !output.status.success() && process_alive_via_ps(pid) {
        bail!(
            "failed to {} elevated sing-box process {pid}: sudo kill{} exited with {}{}",
            if force { "force stop" } else { "stop" },
            if force { " -9" } else { "" },
            output.status,
            process_command_stderr(&output)
        );
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessInstanceVerification {
    Gone,
    Verified,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_process_instance_before_force_stop<F>(
    pid: u32,
    expected_start: Option<ProcessStartToken>,
    verify_identity: &mut F,
) -> Result<ProcessInstanceVerification>
where
    F: FnMut(u32) -> Result<bool>,
{
    let Some(expected_start) = expected_start else {
        if !process_alive_via_ps(pid) {
            return Ok(ProcessInstanceVerification::Gone);
        }
        bail!("process instance token is unavailable for pid {pid}");
    };
    match process_start_token(pid) {
        Some(actual_start) if actual_start == expected_start => {}
        Some(_) => bail!("PID {pid} now identifies a different process instance"),
        None if !process_alive_via_ps(pid) => {
            return Ok(ProcessInstanceVerification::Gone);
        }
        None => bail!("process instance token is unavailable for pid {pid}"),
    }
    if !verify_identity(pid)? {
        if !process_alive_via_ps(pid) {
            return Ok(ProcessInstanceVerification::Gone);
        }
        bail!("process identity no longer matches pid {pid}");
    }
    match process_start_token(pid) {
        Some(actual_start) if actual_start == expected_start => {}
        Some(_) => bail!("PID {pid} changed process instance during identity verification"),
        None if !process_alive_via_ps(pid) => {
            return Ok(ProcessInstanceVerification::Gone);
        }
        None => bail!(
            "process instance token became unavailable during identity verification for pid {pid}"
        ),
    }
    Ok(ProcessInstanceVerification::Verified)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_command_stderr(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn stop_sing_box_pid_escalating(pid: u32) -> Result<()> {
    // Elevated restart is never requested outside macOS/Linux, so this just falls
    // back to the platform stop path (taskkill on Windows). It keeps the elevated
    // branch in `stop_managed_sing_box_process` compilable on every platform.
    stop_sing_box_pid(pid)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_owned_elevated_sing_box_pid(
    pid: u32,
    wrapper_pid: u32,
    executable: &Path,
    config_path: &Path,
) -> Result<()> {
    stop_sing_box_pid_sudo_verified(pid, |pid| {
        ensure_owned_elevated_sing_box_pid_identity(pid, wrapper_pid, executable, config_path)
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn stop_owned_elevated_sing_box_pid(
    pid: u32,
    _wrapper_pid: u32,
    _executable: &Path,
    _config_path: &Path,
) -> Result<()> {
    stop_sing_box_pid_escalating(pid)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn managed_sing_box_pid_is_alive(pid: u32) -> bool {
    process_alive_via_ps(pid)
}

#[cfg(windows)]
fn managed_sing_box_pid_is_alive(pid: u32) -> bool {
    process_exists(pid)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn managed_sing_box_pid_is_alive(_pid: u32) -> bool {
    false
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn ensure_managed_sing_box_pid_identity(
    pid: u32,
    executable: &Path,
    config_path: &Path,
) -> Result<bool> {
    if !managed_sing_box_pid_is_alive(pid) {
        return Ok(false);
    }
    let executable = resolve_sing_box_executable(executable).with_context(|| {
        format!(
            "failed to verify managed sing-box executable {} before stopping pid {pid}",
            executable.display()
        )
    })?;
    let config_path = config_path.canonicalize().with_context(|| {
        format!(
            "failed to verify managed sing-box config {} before stopping pid {pid}",
            config_path.display()
        )
    })?;
    if find_sing_box_run_pids_for_config(&executable, &config_path)?.contains(&pid) {
        return Ok(true);
    }
    if !managed_sing_box_pid_is_alive(pid) {
        return Ok(false);
    }
    bail!(
        "refusing to stop managed sing-box pid {pid}: process identity no longer matches {} run --config {}",
        executable.display(),
        config_path.display()
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn ensure_owned_elevated_sing_box_pid_identity(
    pid: u32,
    wrapper_pid: u32,
    executable: &Path,
    config_path: &Path,
) -> Result<bool> {
    if !managed_sing_box_pid_is_alive(pid) {
        return Ok(false);
    }
    let descendants = process_descendant_pids(wrapper_pid)?;
    if pid != wrapper_pid && !descendants.contains(&pid) {
        if !managed_sing_box_pid_is_alive(pid) {
            return Ok(false);
        }
        bail!(
            "refusing to stop elevated sing-box pid {pid}: it is no longer owned by wrapper pid {wrapper_pid}"
        );
    }
    ensure_managed_sing_box_pid_identity(pid, executable, config_path)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn ensure_owned_elevated_sing_box_pid_identity(
    pid: u32,
    _wrapper_pid: u32,
    executable: &Path,
    config_path: &Path,
) -> Result<bool> {
    ensure_managed_sing_box_pid_identity(pid, executable, config_path)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn ensure_managed_sing_box_pid_identity(
    _pid: u32,
    _executable: &Path,
    _config_path: &Path,
) -> Result<bool> {
    Ok(false)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn find_sing_box_run_pids_for_config(executable: &Path, config_path: &Path) -> Result<Vec<u32>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .context("failed to list processes with ps")?;
    if !output.status.success() {
        bail!("ps exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut pids = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((pid, command)) = parse_ps_pid_command(line) else {
            continue;
        };
        if command_matches_sing_box_run_for_config(command, executable, config_path) {
            pids.push(pid);
        }
    }
    Ok(pids)
}

#[cfg(windows)]
fn find_sing_box_run_pids_for_config(executable: &Path, config_path: &Path) -> Result<Vec<u32>> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process | Select-Object ProcessId,CommandLine | ConvertTo-Json -Compress",
        ])
        .output()
        .context("failed to list sing-box processes with PowerShell")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "PowerShell process query exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }
    parse_windows_process_json(
        &String::from_utf8_lossy(&output.stdout),
        executable,
        config_path,
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_ps_pid_command(line: &str) -> Option<(u32, &str)> {
    let mut parts = line.trim().splitn(2, char::is_whitespace);
    let pid = parts.next()?.parse::<u32>().ok()?;
    let command = parts.next()?.trim();
    Some((pid, command))
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffer_size: libc::c_int,
    ) -> libc::c_int;
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MacosProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: libc::uid_t,
    pbi_gid: libc::gid_t,
    pbi_ruid: libc::uid_t,
    pbi_rgid: libc::gid_t,
    pbi_svuid: libc::uid_t,
    pbi_svgid: libc::gid_t,
    rfu_1: u32,
    pbi_comm: [libc::c_char; 16],
    pbi_name: [libc::c_char; 32],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessStartToken {
    primary: u64,
    secondary: u64,
}

#[cfg(target_os = "macos")]
fn process_start_token(pid: u32) -> Option<ProcessStartToken> {
    const PROC_PIDTBSDINFO: libc::c_int = 3;
    let mut info = std::mem::MaybeUninit::<MacosProcBsdInfo>::zeroed();
    let expected_size = std::mem::size_of::<MacosProcBsdInfo>();
    let returned_size = unsafe {
        proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            expected_size as libc::c_int,
        )
    };
    if returned_size != expected_size as libc::c_int {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some(ProcessStartToken {
        primary: info.pbi_start_tvsec,
        secondary: info.pbi_start_tvusec,
    })
}

#[cfg(target_os = "linux")]
fn process_start_token(pid: u32) -> Option<ProcessStartToken> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields_after_name = stat.rsplit_once(')')?.1.split_whitespace();
    let start_time = fields_after_name.nth(19)?.parse().ok()?;
    Some(ProcessStartToken {
        primary: start_time,
        secondary: 0,
    })
}

#[cfg(windows)]
fn parse_windows_process_json(
    text: &str,
    executable: &Path,
    config_path: &Path,
) -> Result<Vec<u32>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let value: Value =
        serde_json::from_str(text).context("failed to parse PowerShell process JSON")?;
    let processes = match &value {
        Value::Array(processes) => processes.iter().collect::<Vec<_>>(),
        Value::Object(_) => vec![&value],
        _ => bail!("PowerShell process JSON had unexpected shape"),
    };
    let mut pids = Vec::new();
    for process in processes {
        let command = process
            .get("CommandLine")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if command_matches_sing_box_run_for_config(command, executable, config_path)
            && let Some(pid) = process.get("ProcessId").and_then(Value::as_u64)
        {
            pids.push(pid as u32);
        }
    }
    Ok(pids)
}

const CONTROLLER_READY_TIMEOUT: Duration = Duration::from_secs(8);
fn path_text_has_directory(path: &str) -> bool {
    path.rsplit_once(['/', '\\'])
        .is_some_and(|(parent, _)| !parent.is_empty())
}

fn path_text_is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    Path::new(path).is_absolute()
        || path.starts_with(r"\\")
        || (bytes.get(1) == Some(&b':') && matches!(bytes.get(2), Some(b'/' | b'\\')))
}

fn executable_basename(executable: &Path) -> String {
    let text = executable.to_string_lossy();
    text.rsplit(['/', '\\']).next().unwrap_or(&text).to_string()
}

#[cfg(windows)]
fn command_path_text_matches(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected)
        || actual
            .strip_suffix(".exe")
            .is_some_and(|base| base.eq_ignore_ascii_case(expected))
        || expected
            .strip_suffix(".exe")
            .is_some_and(|base| actual.eq_ignore_ascii_case(base))
}

#[cfg(not(windows))]
fn command_path_text_matches(actual: &str, expected: &str) -> bool {
    actual == expected
}

fn command_program_matches_executable(program: &str, executable: &Path) -> bool {
    let executable_text = executable.to_string_lossy();
    let expected_name = executable_basename(executable);
    let executable_has_directory = path_text_has_directory(&executable_text);
    let program_has_directory = path_text_has_directory(program);

    if executable_has_directory {
        if path_text_is_absolute(&executable_text) && !path_text_is_absolute(program) {
            return false;
        }
        if !program_has_directory {
            return false;
        }
        match (Path::new(program).canonicalize(), executable.canonicalize()) {
            (Ok(actual), Ok(expected)) => return actual == expected,
            (Ok(_), Err(_)) | (Err(_), Ok(_)) => return false,
            (Err(_), Err(_)) => {}
        }
        return command_path_text_matches(program, &executable_text);
    }

    command_program_name_matches(program, &expected_name)
}

fn command_args_for_executable(command: &str, executable: &Path) -> Vec<String> {
    let executable_text = executable.to_string_lossy();
    let expected_program = executable_basename(executable);
    let mut prefixes = vec![executable_text.to_string(), expected_program];
    prefixes.sort_by_key(|prefix| std::cmp::Reverse(prefix.len()));
    prefixes.dedup();

    for prefix in prefixes {
        let Some(rest) = command.strip_prefix(&prefix) else {
            continue;
        };
        if !rest.chars().next().is_some_and(char::is_whitespace) {
            continue;
        }
        let mut args = vec![prefix];
        args.extend(command_tokens(rest.trim_start()));
        return args;
    }

    command_tokens(command)
}

fn command_matches_sing_box_run_for_config(
    command: &str,
    executable: &Path,
    config_path: &Path,
) -> bool {
    let args = command_args_for_executable(command, executable);
    if args.len() < 3 {
        return false;
    }
    let program_is_sing_box = args
        .first()
        .is_some_and(|program| command_program_matches_executable(program, executable));
    if !program_is_sing_box || !args.iter().any(|arg| arg == "run") {
        return false;
    }
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let config_values = sing_box_config_args(&arg_refs);
    !config_values.is_empty()
        && config_values
            .iter()
            .any(|value| config_arg_matches_path(value, config_path))
}

fn sing_box_config_args<'a>(args: &'a [&'a str]) -> Vec<&'a str> {
    let mut values = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--config" | "-c" => {
                if let Some(value) = args.get(i + 1) {
                    values.push(*value);
                    i += 1;
                }
            }
            value if value.starts_with("--config=") => {
                values.push(value.trim_start_matches("--config="));
            }
            value if value.starts_with("-c=") => {
                values.push(value.trim_start_matches("-c="));
            }
            _ => {}
        }
        i += 1;
    }
    values
}

fn config_arg_matches_path(value: &str, config_path: &Path) -> bool {
    let value_path = Path::new(value);
    if path_text_is_absolute(&config_path.to_string_lossy()) && !path_text_is_absolute(value) {
        return false;
    }
    if value_path == config_path {
        return true;
    }
    match (config_path.canonicalize(), value_path.canonicalize()) {
        (Ok(canonical_config), Ok(canonical_value)) => {
            return canonical_value == canonical_config;
        }
        // If only one side resolves, falling back to a basename could match a different config
        // from another working directory. Process termination must fail closed here.
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => return false,
        (Err(_), Err(_)) => {}
    }
    let config_is_bare = config_path
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty() || parent == Path::new("."));
    config_is_bare
        && config_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|file_name| value == file_name || value == format!("./{file_name}"))
}

const CONTROLLER_READY_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) trait ControllerProbe {
    fn probe_controller(&self) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthorizationRequirement {
    None,
    InteractiveSudo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExitPolicy {
    StopOnDrop,
    LeaveRunning,
}

enum Ownership<C> {
    Stopped,
    Direct {
        pid: u32,
        child: C,
    },
    Elevated {
        pid: u32,
        wrapper: C,
    },
    #[cfg(target_os = "macos")]
    MacosHelper {
        pid: u32,
    },
}

enum StartedProcess<C> {
    Direct {
        pid: u32,
        child: C,
    },
    Elevated {
        pid: u32,
        wrapper: C,
    },
    #[cfg(target_os = "macos")]
    MacosHelper {
        pid: u32,
    },
}

struct BackendRestart<C> {
    process: StartedProcess<C>,
}

trait ProcessBackend {
    type Child;

    fn restart(
        &mut self,
        executable: &Path,
        config_path: &Path,
        elevate: bool,
    ) -> Result<BackendRestart<Self::Child>>;

    fn stop_direct(&mut self, child: &mut Self::Child) -> Result<()>;

    fn stop_elevated(
        &mut self,
        pid: u32,
        wrapper: &mut Self::Child,
        executable: &Path,
        config_path: &Path,
    ) -> Result<()>;

    #[cfg(target_os = "macos")]
    fn stop_macos_helper(&mut self) -> Result<()>;

    fn sudo_required(&self) -> bool;

    fn helper_available(&self) -> bool;
    fn helper_has_managed_process(&self) -> Result<bool>;
}

struct SystemProcessBackend;

impl ProcessBackend for SystemProcessBackend {
    type Child = Child;

    fn restart(
        &mut self,
        executable: &Path,
        config_path: &Path,
        elevate: bool,
    ) -> Result<BackendRestart<Self::Child>> {
        let SingBoxRestartResult {
            started_pid,
            child,
            elevated,
            privileged_helper,
        } = restart_sing_box_for_config(executable, config_path, elevate)?;
        #[cfg(target_os = "macos")]
        if privileged_helper {
            if child.is_some() || !elevated {
                bail!("macOS helper returned an invalid managed sing-box ownership state");
            }
            return Ok(BackendRestart {
                process: StartedProcess::MacosHelper { pid: started_pid },
            });
        }
        #[cfg(not(target_os = "macos"))]
        if privileged_helper {
            bail!("privileged helper ownership is only valid on macOS");
        }
        let child = child.context("managed sing-box restart returned no child process")?;
        let process = if elevated {
            StartedProcess::Elevated {
                pid: started_pid,
                wrapper: child,
            }
        } else {
            StartedProcess::Direct {
                pid: started_pid,
                child,
            }
        };
        Ok(BackendRestart { process })
    }

    fn stop_direct(&mut self, child: &mut Self::Child) -> Result<()> {
        stop_sing_box_child(child)
    }

    fn stop_elevated(
        &mut self,
        pid: u32,
        wrapper: &mut Self::Child,
        executable: &Path,
        config_path: &Path,
    ) -> Result<()> {
        let wrapper_pid = wrapper.id();
        stop_owned_elevated_sing_box_pid(pid, wrapper_pid, executable, config_path)
            .with_context(|| format!("failed to stop elevated sing-box pid {pid}"))?;
        stop_elevated_sing_box_child(wrapper)
            .with_context(|| format!("failed to reap elevated sing-box wrapper pid {wrapper_pid}"))
    }

    #[cfg(target_os = "macos")]
    fn stop_macos_helper(&mut self) -> Result<()> {
        crate::macos_privileged_helper::stop()
            .context("failed to stop sing-box through macOS privileged helper")
    }

    fn sudo_required(&self) -> bool {
        tun_helper_needs_sudo()
    }

    fn helper_available(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            crate::macos_privileged_helper::helper_available()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    fn helper_has_managed_process(&self) -> Result<bool> {
        #[cfg(target_os = "macos")]
        {
            Ok(crate::macos_privileged_helper::status()?.is_some())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(false)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LifecycleReport {
    restarted_pids: Vec<u32>,
    started_pid: u32,
}

impl LifecycleReport {
    #[cfg(test)]
    pub(crate) fn for_test(restarted_pids: Vec<u32>, started_pid: u32) -> Self {
        Self {
            restarted_pids,
            started_pid,
        }
    }

    pub(crate) fn replaced_existing(&self) -> bool {
        !self.restarted_pids.is_empty()
    }

    pub(crate) fn started_process(&self) -> StartedProcessDisplay<'_> {
        StartedProcessDisplay(self)
    }

    pub(crate) fn transition(&self) -> RestartTransitionDisplay<'_> {
        RestartTransitionDisplay(self)
    }
}

pub(crate) struct StartedProcessDisplay<'a>(&'a LifecycleReport);

impl fmt::Display for StartedProcessDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "pid {}", self.0.started_pid)
    }
}

pub(crate) struct RestartTransitionDisplay<'a>(&'a LifecycleReport);

impl fmt::Display for RestartTransitionDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "pid(s) {:?} -> {}",
            self.0.restarted_pids, self.0.started_pid
        )
    }
}

#[derive(Debug)]
pub(crate) struct RestartReceipt {
    report: LifecycleReport,
}

impl RestartReceipt {
    pub(crate) fn report(&self) -> &LifecycleReport {
        &self.report
    }

    pub(crate) fn observe_controller(&self, probe: &dyn ControllerProbe) -> Result<()> {
        wait_for_controller_ready(probe)
    }

    pub(crate) fn into_report(self) -> LifecycleReport {
        self.report
    }
}

pub(crate) struct ManagedSingBox {
    core: ManagedSingBoxCore<SystemProcessBackend>,
}

impl ManagedSingBox {
    pub(crate) fn new(executable: PathBuf, config_path: PathBuf, keep_running: bool) -> Self {
        Self {
            core: ManagedSingBoxCore::with_backend(
                executable,
                config_path,
                keep_running,
                SystemProcessBackend,
            ),
        }
    }

    pub(crate) fn start(&mut self, probe: &dyn ControllerProbe) -> Result<LifecycleReport> {
        self.core.start(probe)
    }

    pub(crate) fn restart(&mut self) -> Result<RestartReceipt> {
        self.core.restart()
    }

    pub(crate) fn startup_authorization_requirement(&self) -> Result<AuthorizationRequirement> {
        self.core.startup_authorization_requirement()
    }

    pub(crate) fn restart_authorization_requirement(
        &self,
        next_run_needs_elevation: bool,
    ) -> AuthorizationRequirement {
        self.core
            .restart_authorization_requirement(next_run_needs_elevation)
    }

    pub(crate) fn shutdown(&mut self) -> Result<()> {
        self.core.shutdown()
    }

    pub(crate) fn leave_running(&mut self) {
        self.core.leave_running();
    }

    pub(crate) fn is_leaving_running(&self) -> bool {
        self.core.is_leaving_running()
    }

    pub(crate) fn diagnostics(&self) -> ManagedSingBoxDiagnostics {
        self.core.diagnostics()
    }
}

struct ManagedSingBoxCore<B: ProcessBackend> {
    executable: PathBuf,
    config_path: PathBuf,
    ownership: Ownership<B::Child>,
    exit_policy: ExitPolicy,
    backend: B,
}

impl<B: ProcessBackend> ManagedSingBoxCore<B> {
    fn with_backend(
        executable: PathBuf,
        config_path: PathBuf,
        keep_running: bool,
        backend: B,
    ) -> Self {
        Self {
            executable,
            config_path,
            ownership: Ownership::Stopped,
            exit_policy: if keep_running {
                ExitPolicy::LeaveRunning
            } else {
                ExitPolicy::StopOnDrop
            },
            backend,
        }
    }

    pub(crate) fn start(&mut self, probe: &dyn ControllerProbe) -> Result<LifecycleReport> {
        self.start_with_observer(|receipt| receipt.observe_controller(probe))
    }

    fn start_with_observer<F>(&mut self, observe: F) -> Result<LifecycleReport>
    where
        F: FnOnce(&RestartReceipt) -> Result<()>,
    {
        let receipt = self.restart()?;
        if let Err(error) = observe(&receipt) {
            let _ = self.shutdown();
            return Err(error);
        }
        Ok(receipt.report)
    }

    pub(crate) fn restart(&mut self) -> Result<RestartReceipt> {
        let elevate = self.current_config_needs_elevation()?;
        if matches!(self.ownership, Ownership::Stopped)
            && self.backend.helper_available()
            && self.backend.helper_has_managed_process()?
        {
            bail!(
                "refusing to replace an external sing-box process owned by the macOS helper; ManagedSingBox only manages processes it started"
            );
        }
        let previous_pid = self.owned_pid();
        if !self.helper_can_restart_in_place() {
            self.stop_owned_process()?;
        }
        let restarted = self
            .backend
            .restart(&self.executable, &self.config_path, elevate)?;
        let (started_pid, ownership) = match restarted.process {
            StartedProcess::Direct { pid, child } => (pid, Ownership::Direct { pid, child }),
            StartedProcess::Elevated { pid, wrapper } => {
                (pid, Ownership::Elevated { pid, wrapper })
            }
            #[cfg(target_os = "macos")]
            StartedProcess::MacosHelper { pid } => (pid, Ownership::MacosHelper { pid }),
        };
        self.ownership = ownership;
        Ok(RestartReceipt {
            report: LifecycleReport {
                restarted_pids: previous_pid.into_iter().collect(),
                started_pid,
            },
        })
    }

    fn helper_can_restart_in_place(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            matches!(self.ownership, Ownership::MacosHelper { .. })
                && self.backend.helper_available()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    pub(crate) fn startup_authorization_requirement(&self) -> Result<AuthorizationRequirement> {
        let next_run_needs_elevation = self.current_config_needs_elevation()?;
        if self.backend.helper_available() {
            return Ok(AuthorizationRequirement::None);
        }
        Ok(self.authorization_requirement_without_helper(next_run_needs_elevation))
    }

    pub(crate) fn restart_authorization_requirement(
        &self,
        next_run_needs_elevation: bool,
    ) -> AuthorizationRequirement {
        if !self.backend.sudo_required() {
            return AuthorizationRequirement::None;
        }
        if self.backend.helper_available() {
            return if matches!(self.ownership, Ownership::Elevated { .. }) {
                AuthorizationRequirement::InteractiveSudo
            } else {
                AuthorizationRequirement::None
            };
        }
        self.authorization_requirement_without_helper(
            next_run_needs_elevation || self.ownership_needs_sudo_without_helper(),
        )
    }

    fn ownership_needs_sudo_without_helper(&self) -> bool {
        match self.ownership {
            Ownership::Elevated { .. } => true,
            #[cfg(target_os = "macos")]
            Ownership::MacosHelper { .. } => true,
            Ownership::Stopped | Ownership::Direct { .. } => false,
        }
    }

    fn authorization_requirement_without_helper(
        &self,
        needs_elevation: bool,
    ) -> AuthorizationRequirement {
        if needs_elevation && self.backend.sudo_required() {
            AuthorizationRequirement::InteractiveSudo
        } else {
            AuthorizationRequirement::None
        }
    }

    pub(crate) fn shutdown(&mut self) -> Result<()> {
        if self.exit_policy == ExitPolicy::LeaveRunning {
            return Ok(());
        }
        self.stop_owned_process()
    }

    pub(crate) fn leave_running(&mut self) {
        self.exit_policy = ExitPolicy::LeaveRunning;
    }

    pub(crate) fn is_leaving_running(&self) -> bool {
        self.exit_policy == ExitPolicy::LeaveRunning
    }

    fn diagnostics(&self) -> ManagedSingBoxDiagnostics {
        let pid = self.owned_pid();
        ManagedSingBoxDiagnostics {
            pid,
            leaves_running: self.is_leaving_running(),
        }
    }

    fn owned_pid(&self) -> Option<u32> {
        match &self.ownership {
            Ownership::Stopped => None,
            Ownership::Direct { pid, .. } | Ownership::Elevated { pid, .. } => Some(*pid),
            #[cfg(target_os = "macos")]
            Ownership::MacosHelper { pid } => Some(*pid),
        }
    }

    fn current_config_needs_elevation(&self) -> Result<bool> {
        Ok(self.config_path.exists() && inspect_tun_config(&self.config_path)?.has_any_tun())
    }

    fn stop_owned_process(&mut self) -> Result<()> {
        let result = match &mut self.ownership {
            Ownership::Stopped => Ok(()),
            Ownership::Direct { child, .. } => self.backend.stop_direct(child),
            Ownership::Elevated { pid, wrapper } => {
                self.backend
                    .stop_elevated(*pid, wrapper, &self.executable, &self.config_path)
            }
            #[cfg(target_os = "macos")]
            Ownership::MacosHelper { .. } => self.backend.stop_macos_helper(),
        };
        if result.is_ok() {
            self.ownership = Ownership::Stopped;
        }
        result
    }
}

pub(crate) struct ManagedSingBoxDiagnostics {
    pid: Option<u32>,
    leaves_running: bool,
}

impl fmt::Display for ManagedSingBoxDiagnostics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let exit = if self.leaves_running {
            "keep-background"
        } else {
            "stop"
        };
        match self.pid {
            None => write!(formatter, "not managed exit={exit}"),
            Some(pid) => {
                write!(formatter, "managed pid={pid} exit={exit}")
            }
        }
    }
}

impl<B: ProcessBackend> Drop for ManagedSingBoxCore<B> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

pub(crate) fn wait_for_controller_ready(probe: &dyn ControllerProbe) -> Result<()> {
    let deadline = Instant::now() + CONTROLLER_READY_TIMEOUT;
    let mut last_error = None;
    while Instant::now() < deadline {
        match probe.probe_controller() {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                std::thread::sleep(CONTROLLER_READY_POLL_INTERVAL);
            }
        }
    }
    match last_error {
        Some(error) => {
            Err(error).context("sing-box started but controller API did not become ready")
        }
        None => bail!("sing-box started but controller API did not become ready"),
    }
}

#[cfg(unix)]
fn tun_helper_needs_sudo() -> bool {
    unsafe { libc::geteuid() != 0 }
}

#[cfg(not(unix))]
fn tun_helper_needs_sudo() -> bool {
    false
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn wait_for_processes_to_exit(pids: &[u32]) -> Result<()> {
    wait_for_processes_to_exit_with_timeout(pids, SING_BOX_STOP_GRACE)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn wait_for_processes_to_exit_with_timeout(pids: &[u32], timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pids.iter().all(|pid| !process_alive_via_ps(*pid)) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for sing-box process(es) to exit: {pids:?}")
}

#[cfg(windows)]
fn wait_for_processes_to_exit(pids: &[u32]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if pids.iter().all(|pid| !process_exists(*pid)) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for sing-box process(es) to exit: {pids:?}")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_sing_box_pid(pid: u32) -> Result<()> {
    let output = Command::new("kill")
        .arg(pid.to_string())
        .output()
        .with_context(|| format!("failed to stop sing-box process {pid}"))?;
    if !output.status.success() {
        if !process_alive_via_ps(pid) {
            return Ok(());
        }
        bail!(
            "failed to stop sing-box process {pid}: kill exited with {}{}",
            output.status,
            process_command_stderr(&output)
        );
    }
    wait_for_processes_to_exit(&[pid])
        .with_context(|| format!("sing-box process {pid} did not exit after SIGTERM"))
}

#[cfg(windows)]
fn stop_sing_box_pid(pid: u32) -> Result<()> {
    let output = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T"])
        .output()
        .with_context(|| format!("failed to stop sing-box process {pid}"))?;
    if !output.status.success() {
        if !process_exists(pid) {
            return Ok(());
        }
        bail!(
            "failed to stop sing-box process {pid}: taskkill exited with {}",
            output.status
        );
    }
    if !process_exists(pid) {
        return Ok(());
    }
    wait_for_processes_to_exit(&[pid])
        .with_context(|| format!("sing-box process {pid} did not exit after taskkill"))
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn stop_sing_box_pid(_pid: u32) -> Result<()> {
    bail!("managed sing-box shutdown is only available on macOS and Linux")
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn stop_sing_box_child(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("failed to inspect managed sing-box process")?
        .is_some()
    {
        return Ok(());
    }
    child
        .kill()
        .context("failed to kill managed sing-box process")?;
    child
        .wait()
        .context("failed to reap managed sing-box process")?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_elevated_sing_box_child(child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child
            .try_wait()
            .context("failed to inspect elevated sing-box wrapper")?
            .is_some()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let pid = child.id();
    stop_sing_box_pid_sudo(pid)
        .with_context(|| format!("failed to stop elevated sing-box wrapper pid {pid}"))?;
    child
        .wait()
        .context("failed to reap elevated sing-box wrapper")?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn stop_elevated_sing_box_child(child: &mut Child) -> Result<()> {
    stop_sing_box_child(child)
}

fn sing_box_process_log_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("sing-box.log")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use anyhow::{Result, bail};

    #[cfg(target_os = "macos")]
    use super::SystemProcessBackend;
    #[cfg(target_os = "linux")]
    use super::restart_sing_box_for_config;
    use super::{
        AuthorizationRequirement, BackendRestart, ManagedSingBoxCore, Ownership, ProcessBackend,
        StartedProcess, command_matches_sing_box_run_for_config, command_path_text_matches,
        config_arg_matches_path, ensure_managed_sing_box_pid_identity, path_text_is_absolute,
        resolve_sing_box_executable_from_path, sing_box_config_args,
        sing_box_log_reports_bind_conflict, spawn_sing_box_with_bind_retry,
        stop_elevated_sing_box_child, stop_sing_box_child, stop_sing_box_pid_escalating,
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use super::{
        ProcessStartToken, process_alive_via_ps, process_descendant_pids, process_start_token,
        resolve_new_sing_box_pid_or_cleanup, select_owned_sing_box_pid,
        stop_sing_box_pid_with_escalation, verify_process_instance_before_force_stop,
    };

    #[derive(Clone, Default)]
    struct BackendState {
        events: Rc<RefCell<Vec<String>>>,
    }

    struct FakeBackend {
        state: BackendState,
        restarts: VecDeque<Result<BackendRestart<u32>, &'static str>>,
        fail_stop_once: bool,
        sudo_required: bool,
        helper_available: bool,
        helper_has_process: bool,
    }

    impl FakeBackend {
        fn new(
            restarts: impl IntoIterator<Item = Result<BackendRestart<u32>, &'static str>>,
        ) -> Self {
            Self {
                state: BackendState::default(),
                restarts: restarts.into_iter().collect(),
                fail_stop_once: false,
                sudo_required: true,
                helper_available: false,
                helper_has_process: false,
            }
        }

        fn direct(pid: u32) -> BackendRestart<u32> {
            BackendRestart {
                process: StartedProcess::Direct { pid, child: pid },
            }
        }

        fn elevated(pid: u32, wrapper: u32) -> BackendRestart<u32> {
            BackendRestart {
                process: StartedProcess::Elevated { pid, wrapper },
            }
        }

        fn event(&self, event: impl Into<String>) {
            self.state.events.borrow_mut().push(event.into());
        }

        fn maybe_fail_stop(&mut self) -> Result<()> {
            if self.fail_stop_once {
                self.fail_stop_once = false;
                bail!("scripted stop failure");
            }
            Ok(())
        }
    }

    impl ProcessBackend for FakeBackend {
        type Child = u32;

        fn restart(
            &mut self,
            _executable: &Path,
            _config_path: &Path,
            elevate: bool,
        ) -> Result<BackendRestart<Self::Child>> {
            self.event(format!("restart elevate={elevate}"));
            match self.restarts.pop_front().expect("scripted restart") {
                Ok(restart) => Ok(restart),
                Err(message) => bail!(message),
            }
        }

        fn stop_direct(&mut self, child: &mut Self::Child) -> Result<()> {
            self.event(format!("stop direct {child}"));
            self.maybe_fail_stop()
        }

        fn stop_elevated(
            &mut self,
            pid: u32,
            wrapper: &mut Self::Child,
            _executable: &Path,
            _config_path: &Path,
        ) -> Result<()> {
            self.event(format!("stop elevated {pid} wrapper {wrapper}"));
            self.maybe_fail_stop()
        }

        #[cfg(target_os = "macos")]
        fn stop_macos_helper(&mut self) -> Result<()> {
            self.event("stop helper");
            self.maybe_fail_stop()
        }

        fn sudo_required(&self) -> bool {
            self.sudo_required
        }

        fn helper_available(&self) -> bool {
            self.helper_available
        }

        fn helper_has_managed_process(&self) -> Result<bool> {
            Ok(self.helper_has_process)
        }
    }

    fn manager(backend: FakeBackend, keep_running: bool) -> ManagedSingBoxCore<FakeBackend> {
        ManagedSingBoxCore::with_backend(
            PathBuf::from("sing-box"),
            PathBuf::from("missing-config.json"),
            keep_running,
            backend,
        )
    }

    fn unique_test_suffix() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{nanos}-{counter}")
    }

    #[test]
    fn sing_box_process_matcher_accepts_run_command_for_config() {
        let executable = PathBuf::from("sing-box");
        let config = PathBuf::from("config.json");

        assert!(command_matches_sing_box_run_for_config(
            "sing-box run --config ./config.json",
            &executable,
            &config
        ));
        assert!(command_matches_sing_box_run_for_config(
            "/usr/local/bin/sing-box run -c config.json",
            &executable,
            &config
        ));
    }

    #[test]
    fn sing_box_process_matcher_accepts_canonicalized_executable_path() {
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-canonical-executable-{}",
            unique_test_suffix()
        ));
        std::fs::create_dir_all(&root).expect("temporary executable directory creates");
        let executable = root.join("./fake-sing-box");
        std::fs::write(&executable, b"").expect("temporary executable writes");
        let canonical_executable = executable
            .canonicalize()
            .expect("temporary executable canonicalizes");
        let config = PathBuf::from("config.json");

        let command = format!(
            "{} run --config config.json",
            canonical_executable.display()
        );
        assert!(command_matches_sing_box_run_for_config(
            &command,
            &executable,
            &config
        ));

        std::fs::remove_dir_all(root).expect("temporary executable directory removes");
    }

    #[cfg(unix)]
    #[test]
    fn sing_box_program_name_is_resolved_from_search_path() {
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-executable-path-{}",
            unique_test_suffix()
        ));
        let executable = root.join("fake-sing-box");
        std::fs::create_dir_all(&root).expect("temporary executable directory creates");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("temporary executable writes");
        let mut permissions = std::fs::metadata(&executable)
            .expect("temporary executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions)
            .expect("temporary executable becomes runnable");
        let search_path = std::env::join_paths([&root]).expect("temporary PATH builds");

        let resolved =
            resolve_sing_box_executable_from_path(Path::new("fake-sing-box"), Some(&search_path))
                .expect("program name resolves from PATH");
        assert_eq!(
            resolved,
            executable
                .canonicalize()
                .expect("temporary executable canonicalizes")
        );

        std::fs::remove_dir_all(root).expect("temporary executable directory removes");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_executable_path_matching_is_case_and_suffix_sensitive() {
        assert!(!command_path_text_matches(
            "/nonexistent/SING-BOX",
            "/nonexistent/sing-box"
        ));
        assert!(!command_path_text_matches(
            "/nonexistent/sing-box.exe",
            "/nonexistent/sing-box"
        ));
    }

    #[test]
    fn windows_drive_relative_path_is_not_absolute() {
        assert!(!path_text_is_absolute(r"C:config.json"));
        assert!(path_text_is_absolute(r"C:\config.json"));
        assert!(path_text_is_absolute(r"C:/config.json"));
        assert!(path_text_is_absolute(r"\\server\share\config.json"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn elevated_pid_handoff_only_accepts_the_wrapper_process_tree() {
        let matching = [41, 42, 43, 44];
        let excluded = [41];
        let descendants = [43];

        assert_eq!(
            select_owned_sing_box_pid(&matching, &excluded, 42, &descendants),
            Some(42)
        );
        assert_eq!(
            select_owned_sing_box_pid(&matching, &[41, 42], 42, &descendants),
            Some(43)
        );
        assert_eq!(
            select_owned_sing_box_pid(&[41, 44], &excluded, 42, &descendants),
            None
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn signal_test_process(pid: u32, force: bool) -> anyhow::Result<()> {
        let mut command = std::process::Command::new("/bin/kill");
        if force {
            command.arg("-9");
        }
        let status = command.arg(pid.to_string()).status()?;
        anyhow::ensure!(status.success(), "test signal command exited with {status}");
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn spawn_sigterm_resistant_test_process() -> std::process::Child {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; printf 'READY\\n'; exec sleep 30"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("SIGTERM-resistant test process starts");
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(
                child
                    .stdout
                    .take()
                    .expect("test process stdout is captured"),
            ),
            &mut ready,
        )
        .expect("test process reports readiness");
        assert_eq!(ready.trim(), "READY");
        child
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn stop_escalation_force_stops_the_same_process_instance() {
        let mut child = spawn_sigterm_resistant_test_process();
        let pid = child.id();
        let signals = std::cell::RefCell::new(Vec::new());

        let result = stop_sing_box_pid_with_escalation(
            pid,
            Duration::from_millis(50),
            |_| Ok(true),
            |pid, force| {
                signals.borrow_mut().push(force);
                signal_test_process(pid, force)
            },
        );
        child.wait().expect("test process reaps");

        assert!(result.is_ok(), "stop escalation failed: {result:?}");
        assert_eq!(*signals.borrow(), vec![false, true]);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn stop_escalation_refuses_force_after_identity_changes() {
        let mut child = spawn_sigterm_resistant_test_process();
        let pid = child.id();
        let verify_calls = std::cell::Cell::new(0);
        let signals = std::cell::RefCell::new(Vec::new());

        let result = stop_sing_box_pid_with_escalation(
            pid,
            Duration::from_millis(50),
            |_| {
                let call = verify_calls.get();
                verify_calls.set(call + 1);
                Ok(call == 0)
            },
            |pid, force| {
                signals.borrow_mut().push(force);
                signal_test_process(pid, force)
            },
        );
        let _ = child.kill();
        child.wait().expect("test process reaps");

        let error = result.expect_err("changed identity prevents force stop");
        assert!(format!("{error:#}").contains("process identity no longer matches"));
        assert_eq!(verify_calls.get(), 2);
        assert_eq!(*signals.borrow(), vec![false]);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn stop_escalation_does_not_signal_a_pid_that_disappears_during_verification() {
        let child = std::cell::RefCell::new(Some(
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .expect("test process starts"),
        ));
        let pid = child.borrow().as_ref().expect("test process exists").id();
        let signal_calls = std::cell::Cell::new(0);

        let result = stop_sing_box_pid_with_escalation(
            pid,
            Duration::from_millis(50),
            |_| {
                let mut child = child
                    .borrow_mut()
                    .take()
                    .expect("identity verifier owns the test process");
                child
                    .kill()
                    .expect("test process terminates during verification");
                child
                    .wait()
                    .expect("test process reaps during verification");
                Ok(false)
            },
            |_, _| {
                signal_calls.set(signal_calls.get() + 1);
                Ok(())
            },
        );

        assert!(
            result.is_ok(),
            "a vanished process is already stopped: {result:?}"
        );
        assert_eq!(signal_calls.get(), 0);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn process_verification_rejects_a_changed_start_token_before_identity_probe() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("test process starts");
        let pid = child.id();
        let actual_start = process_start_token(pid).expect("test process has a start token");
        let changed_start = ProcessStartToken {
            primary: actual_start.primary ^ 1,
            secondary: actual_start.secondary,
        };
        let identity_calls = std::cell::Cell::new(0);

        let result =
            verify_process_instance_before_force_stop(pid, Some(changed_start), &mut |_| {
                identity_calls.set(identity_calls.get() + 1);
                Ok(true)
            });
        child.kill().expect("test process terminates");
        child.wait().expect("test process reaps");

        let error = result.expect_err("changed start token must be rejected");
        assert!(format!("{error:#}").contains("different process instance"));
        assert_eq!(identity_calls.get(), 0);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn restart_retries_a_transient_address_in_use_failure() {
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-restart-retry-{}",
            unique_test_suffix()
        ));
        std::fs::create_dir_all(&root).expect("temporary restart directory creates");
        let executable = PathBuf::from("fake-sing-box-retry");
        let config = root.join("config.json");
        let log_path = root.join("sing-box.log");
        std::fs::write(&config, b"{}").expect("temporary config writes");

        let mut attempt_count = 0_u32;
        let mut child = spawn_sing_box_with_bind_retry(
            &executable,
            &config,
            &log_path,
            || {
                attempt_count += 1;
                if attempt_count == 1 {
                    let mut command = std::process::Command::new("sh");
                    command.args([
                        "-c",
                        "printf '%s\\n' 'FATAL start inbound/mixed[0]: listen tcp 0.0.0.0:6780: bind: address already in use' >&2; exit 1",
                    ]);
                    command
                } else {
                    let mut command = std::process::Command::new("sleep");
                    command.arg("5");
                    command
                }
            },
        )
        .expect("transient address-in-use startup retries");
        assert!(
            child
                .try_wait()
                .expect("retried child can be inspected")
                .is_none(),
            "the successful retry must still be running"
        );
        stop_sing_box_child(&mut child).expect("fake sing-box stops");
        std::fs::remove_dir_all(&root).expect("temporary restart directory removes");

        assert_eq!(attempt_count, 2);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn restart_does_not_reuse_an_old_bind_error_for_a_new_failure() {
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-restart-log-offset-{}",
            unique_test_suffix()
        ));
        std::fs::create_dir_all(&root).expect("temporary restart directory creates");
        let executable = PathBuf::from("fake-sing-box-log-offset");
        let config = root.join("config.json");
        let log_path = root.join("sing-box.log");
        std::fs::write(&config, b"{}").expect("temporary config writes");

        let mut attempt_count = 0_u32;
        let error = spawn_sing_box_with_bind_retry(&executable, &config, &log_path, || {
            attempt_count += 1;
            let message = if attempt_count == 1 {
                "FATAL start inbound/mixed[0]: bind: address already in use"
            } else {
                "FATAL decode config: unknown field definitely-invalid"
            };
            let mut command = std::process::Command::new("sh");
            command.args(["-c", &format!("printf '%s\\n' '{message}' >&2; exit 1")]);
            command
        })
        .expect_err("a non-bind startup failure is returned immediately");
        std::fs::remove_dir_all(&root).expect("temporary restart directory removes");

        assert_eq!(attempt_count, 2);
        assert!(format!("{error:#}").contains("definitely-invalid"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn elevated_pid_resolution_failure_cleans_up_uncommitted_child() {
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-pid-resolution-cleanup-{}",
            unique_test_suffix()
        ));
        std::fs::create_dir_all(&root).expect("temporary cleanup directory creates");
        let executable = root.join("fake-sing-box-never-started");
        let config = root.join("config.json");
        std::fs::write(&executable, b"").expect("fake executable writes");
        std::fs::write(&config, b"{}").expect("temporary config writes");
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("uncommitted startup wrapper starts");
        let wrapper_pid = child.id();
        let descendant_deadline = Instant::now() + Duration::from_secs(2);
        let descendant_pid = loop {
            if let Some(pid) = process_descendant_pids(wrapper_pid)
                .expect("wrapper descendants can be inspected")
                .into_iter()
                .next()
            {
                break pid;
            }
            assert!(
                Instant::now() < descendant_deadline,
                "startup wrapper did not create its test descendant"
            );
            std::thread::sleep(Duration::from_millis(20));
        };

        let error = resolve_new_sing_box_pid_or_cleanup(&executable, &config, &[], &mut child)
            .expect_err("missing elevated sing-box pid fails ownership handoff");
        assert!(format!("{error:#}").contains("did not appear in the process list"));
        let wrapper_alive = process_alive_via_ps(wrapper_pid);
        let descendant_alive = process_alive_via_ps(descendant_pid);
        let wrapper_reaped = child
            .try_wait()
            .expect("cleaned wrapper can be inspected")
            .is_some();
        if wrapper_alive {
            let _ = stop_sing_box_pid_escalating(wrapper_pid);
        }
        if descendant_alive {
            let _ = stop_sing_box_pid_escalating(descendant_pid);
        }
        std::fs::remove_dir_all(&root).expect("temporary cleanup directory removes");

        assert!(
            !wrapper_alive,
            "failed ownership handoff left its wrapper alive"
        );
        assert!(
            !descendant_alive,
            "failed ownership handoff left descendant pid {descendant_pid} alive"
        );
        assert!(
            wrapper_reaped,
            "failed ownership handoff did not reap its wrapper"
        );
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    #[test]
    fn sing_box_bind_retry_classifier_is_specific() {
        assert!(sing_box_log_reports_bind_conflict(
            "start inbound/mixed[0]: bind: address already in use"
        ));
        assert!(!sing_box_log_reports_bind_conflict(
            "decode config: address already in use is not a valid field"
        ));
        assert!(!sing_box_log_reports_bind_conflict(
            "start inbound/mixed[0]: bind: permission denied"
        ));
        assert!(sing_box_log_reports_bind_conflict(
            "bind: Only one usage of each socket address is normally permitted"
        ));
    }

    #[test]
    fn sing_box_process_matcher_uses_only_custom_executable() {
        let executable = PathBuf::from(r#"C:\Program Files\Vendor\proxy-core.exe"#);
        let config = PathBuf::from("config.json");

        assert!(command_matches_sing_box_run_for_config(
            r#""C:\Program Files\Vendor\proxy-core.exe" run --config config.json"#,
            &executable,
            &config
        ));
        assert!(!command_matches_sing_box_run_for_config(
            "proxy-core.exe run -c ./config.json",
            &executable,
            &config
        ));
        assert!(!command_matches_sing_box_run_for_config(
            r#"C:\Other Vendor\proxy-core.exe run --config config.json"#,
            &executable,
            &config
        ));
        assert!(!command_matches_sing_box_run_for_config(
            "/opt/vendor/proxy-core run -c ./config.json",
            &executable,
            &config
        ));
        let executable_with_space = PathBuf::from("/tmp/review path/proxy-core");
        assert!(command_matches_sing_box_run_for_config(
            "/tmp/review path/proxy-core run -c ./config.json",
            &executable_with_space,
            &config
        ));
        assert!(!command_matches_sing_box_run_for_config(
            "/tmp/other path/proxy-core run -c ./config.json",
            &executable_with_space,
            &config
        ));
        assert!(!command_matches_sing_box_run_for_config(
            "sing-box run --config config.json",
            &executable,
            &config
        ));
    }

    #[test]
    fn sing_box_process_matcher_rejects_non_matching_commands() {
        let executable = PathBuf::from("proxy-core");
        let config = PathBuf::from("config.json");

        assert!(!command_matches_sing_box_run_for_config(
            "proxy-core version",
            &executable,
            &config
        ));
        assert!(!command_matches_sing_box_run_for_config(
            "other-core run --config config.json",
            &executable,
            &config
        ));
        assert!(!command_matches_sing_box_run_for_config(
            "proxy-core run --config ./other.json",
            &executable,
            &config
        ));
    }

    #[test]
    fn sing_box_config_args_support_common_forms() {
        assert_eq!(
            sing_box_config_args(&["sing-box", "run", "--config", "./config.json"]),
            vec!["./config.json"]
        );
        assert_eq!(
            sing_box_config_args(&["sing-box", "run", "-c=config.json"]),
            vec!["config.json"]
        );
        assert!(config_arg_matches_path(
            "./config.json",
            &PathBuf::from("config.json")
        ));
    }

    #[test]
    fn config_path_matcher_rejects_different_existing_files_with_same_name() {
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-config-identity-{}",
            unique_test_suffix()
        ));
        let first = root.join("first/config.json");
        let second = root.join("second/config.json");
        std::fs::create_dir_all(first.parent().expect("first parent exists"))
            .expect("first directory creates");
        std::fs::create_dir_all(second.parent().expect("second parent exists"))
            .expect("second directory creates");
        std::fs::write(&first, b"{}").expect("first config writes");
        std::fs::write(&second, b"{}").expect("second config writes");

        assert!(!config_arg_matches_path(
            second.to_str().expect("second path is UTF-8"),
            &first
        ));

        std::fs::remove_dir_all(root).expect("temporary configs remove");
    }

    #[test]
    fn config_path_matcher_rejects_missing_same_name_path_when_target_exists() {
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-config-missing-identity-{}",
            unique_test_suffix()
        ));
        let target = root.join("target/config.json");
        let missing = root.join("missing/config.json");
        std::fs::create_dir_all(target.parent().expect("target parent exists"))
            .expect("target directory creates");
        std::fs::write(&target, b"{}").expect("target config writes");

        assert!(!config_arg_matches_path(
            missing.to_str().expect("missing path is UTF-8"),
            &target
        ));

        std::fs::remove_dir_all(root).expect("temporary configs remove");
    }

    #[test]
    fn absolute_process_identity_rejects_relative_executable_and_config() {
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-absolute-process-identity-{}",
            unique_test_suffix()
        ));
        let executable = root.join("sing-box");
        let config = root.join("config.json");
        std::fs::create_dir_all(&root).expect("temporary identity directory creates");
        std::fs::write(&executable, b"").expect("temporary executable writes");
        std::fs::write(&config, b"{}").expect("temporary config writes");
        let executable = executable
            .canonicalize()
            .expect("temporary executable canonicalizes");
        let config = config
            .canonicalize()
            .expect("temporary config canonicalizes");

        assert!(!command_matches_sing_box_run_for_config(
            "sing-box run --config config.json",
            &executable,
            &config
        ));
        assert!(!config_arg_matches_path("config.json", &config));

        std::fs::remove_dir_all(root).expect("temporary identity directory removes");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn managed_pid_identity_rejects_an_unrelated_live_process() {
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-managed-pid-identity-{}",
            unique_test_suffix()
        ));
        let executable = root.join("sing-box");
        let config = root.join("config.json");
        std::fs::create_dir_all(&root).expect("temporary identity directory creates");
        std::fs::write(&executable, b"").expect("temporary executable writes");
        std::fs::write(&config, b"{}").expect("temporary config writes");

        let error = ensure_managed_sing_box_pid_identity(std::process::id(), &executable, &config)
            .expect_err("an unrelated live process must not pass sing-box identity validation");
        assert!(format!("{error:#}").contains("refusing to stop managed sing-box pid"));

        std::fs::remove_dir_all(root).expect("temporary identity directory removes");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn elevated_wrapper_is_reaped_after_its_natural_exit() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 0.05"])
            .spawn()
            .expect("short-lived wrapper starts");

        stop_elevated_sing_box_child(&mut child)
            .expect("short-lived wrapper exits without a signal");
        assert!(
            child
                .try_wait()
                .expect("reaped wrapper can be inspected")
                .is_some()
        );
    }

    #[test]
    fn restart_replaces_direct_ownership_and_keeps_diagnostics_opaque() {
        let backend = FakeBackend::new([Ok(FakeBackend::direct(11)), Ok(FakeBackend::direct(22))]);
        let events = backend.state.events.clone();
        let mut managed = manager(backend, false);

        let first = managed.restart().expect("first restart");
        assert!(!first.report().replaced_existing());
        assert_eq!(first.report().started_process().to_string(), "pid 11");
        let second = managed.restart().expect("second restart");
        assert_eq!(
            second.report().transition().to_string(),
            "pid(s) [11] -> 22"
        );
        assert!(matches!(
            managed.ownership,
            Ownership::Direct { pid: 22, .. }
        ));
        assert_eq!(
            events.borrow().as_slice(),
            [
                "restart elevate=false",
                "stop direct 11",
                "restart elevate=false"
            ]
        );
    }

    #[test]
    fn stop_failure_retains_ownership_for_retry() {
        let mut backend = FakeBackend::new([Ok(FakeBackend::direct(41))]);
        backend.fail_stop_once = true;
        let events = backend.state.events.clone();
        let mut managed = manager(backend, false);
        managed.restart().expect("process starts");

        let error = managed.shutdown().expect_err("first stop fails");
        assert!(format!("{error:#}").contains("scripted stop failure"));
        assert!(matches!(
            managed.ownership,
            Ownership::Direct { pid: 41, .. }
        ));

        managed.shutdown().expect("second stop succeeds");
        assert!(matches!(managed.ownership, Ownership::Stopped));
        assert_eq!(
            events
                .borrow()
                .iter()
                .filter(|event| event.starts_with("stop"))
                .count(),
            2
        );
    }

    #[test]
    fn failed_spawn_after_stop_leaves_manager_stopped() {
        let backend =
            FakeBackend::new([Ok(FakeBackend::direct(51)), Err("scripted spawn failure")]);
        let mut managed = manager(backend, false);
        managed.restart().expect("first process starts");

        let error = managed.restart().expect_err("replacement spawn fails");
        assert!(format!("{error:#}").contains("scripted spawn failure"));
        assert!(matches!(managed.ownership, Ownership::Stopped));
    }

    #[test]
    fn invalid_next_config_does_not_stop_the_current_process() {
        let config_path = std::env::temp_dir().join(format!(
            "sing-box-tui-managed-invalid-config-{}",
            unique_test_suffix()
        ));
        let backend = FakeBackend::new([Ok(FakeBackend::direct(56))]);
        let events = backend.state.events.clone();
        let mut managed = ManagedSingBoxCore::with_backend(
            PathBuf::from("sing-box"),
            config_path.clone(),
            false,
            backend,
        );
        managed.restart().expect("first process starts");
        std::fs::write(&config_path, b"{").expect("invalid config writes");

        managed
            .restart()
            .expect_err("invalid replacement config is rejected");

        assert!(matches!(
            managed.ownership,
            Ownership::Direct { pid: 56, .. }
        ));
        assert!(
            !events
                .borrow()
                .iter()
                .any(|event| event.starts_with("stop"))
        );
        let _ = std::fs::remove_file(config_path);
    }

    #[test]
    fn startup_readiness_failure_rolls_back_when_exit_policy_stops() {
        let backend = FakeBackend::new([Ok(FakeBackend::direct(61))]);
        let events = backend.state.events.clone();
        let mut managed = manager(backend, false);

        let error = managed
            .start_with_observer(|_| bail!("controller unavailable"))
            .expect_err("readiness failure is fatal");

        assert!(format!("{error:#}").contains("controller unavailable"));
        assert!(matches!(managed.ownership, Ownership::Stopped));
        assert!(events.borrow().contains(&"stop direct 61".to_string()));
    }

    #[test]
    fn startup_readiness_failure_honors_leave_running_policy() {
        let backend = FakeBackend::new([Ok(FakeBackend::direct(71))]);
        let events = backend.state.events.clone();
        let mut managed = manager(backend, true);

        managed
            .start_with_observer(|_| bail!("controller unavailable"))
            .expect_err("readiness failure is fatal to startup");

        assert!(matches!(
            managed.ownership,
            Ownership::Direct { pid: 71, .. }
        ));
        assert!(
            !events
                .borrow()
                .iter()
                .any(|event| event.starts_with("stop"))
        );
    }

    #[test]
    fn drop_stops_owned_process_unless_leave_running_was_selected() {
        let stopping_backend = FakeBackend::new([Ok(FakeBackend::direct(81))]);
        let stopping_events = stopping_backend.state.events.clone();
        {
            let mut managed = manager(stopping_backend, false);
            managed.restart().expect("process starts");
        }
        assert!(
            stopping_events
                .borrow()
                .contains(&"stop direct 81".to_string())
        );

        let leaving_backend = FakeBackend::new([Ok(FakeBackend::direct(82))]);
        let leaving_events = leaving_backend.state.events.clone();
        {
            let mut managed = manager(leaving_backend, false);
            managed.restart().expect("process starts");
            managed.leave_running();
        }
        assert!(
            !leaving_events
                .borrow()
                .iter()
                .any(|event| event.starts_with("stop"))
        );
    }

    #[test]
    fn authorization_hides_elevated_ownership_details() {
        let mut backend = FakeBackend::new([Ok(FakeBackend::elevated(91, 90))]);
        backend.sudo_required = true;
        let mut managed = manager(backend, true);
        managed.restart().expect("elevated process starts");

        assert_eq!(
            managed.restart_authorization_requirement(false),
            AuthorizationRequirement::InteractiveSudo
        );
    }

    #[test]
    fn stopped_manager_rejects_a_process_owned_by_the_macos_helper() {
        let mut backend = FakeBackend::new([]);
        backend.helper_available = true;
        backend.helper_has_process = true;
        let events = backend.state.events.clone();
        let mut managed = manager(backend, false);

        let error = managed
            .restart()
            .expect_err("external helper process is not adopted");

        assert!(format!("{error:#}").contains("only manages processes it started"));
        assert!(matches!(managed.ownership, Ownership::Stopped));
        assert!(events.borrow().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn restart_refuses_to_stop_an_external_matching_process() {
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-external-process-{}",
            unique_test_suffix()
        ));
        std::fs::create_dir_all(&root).expect("test directory creates");
        let executable = PathBuf::from("/bin/sh");
        let script = root.join("run");
        let config = root.join("config.json");
        std::fs::write(&script, b"sleep 30\n").expect("test script writes");
        std::fs::write(&config, b"{}").expect("test config writes");
        let mut external = std::process::Command::new(&executable)
            .args(["run", "--config"])
            .arg(&config)
            .current_dir(&root)
            .spawn()
            .expect("external process starts");

        let error = match restart_sing_box_for_config(&executable, &config, false) {
            Ok(_) => panic!("external process was replaced"),
            Err(error) => error,
        };

        assert!(format!("{error:#}").contains("only manages processes it started"));
        assert!(
            external
                .try_wait()
                .expect("external process status")
                .is_none()
        );
        external.kill().expect("external process stops");
        external.wait().expect("external process reaps");
        std::fs::remove_dir_all(root).expect("test directory removes");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn installed_macos_helper_owns_sing_box_even_when_tun_is_disabled() {
        assert!(super::should_use_macos_privileged_helper(true));
        assert!(!super::should_use_macos_privileged_helper(false));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "targeted TUN-disable lifecycle reproducer; run alone with --test-threads=1"]
    fn tun_disable_releases_a_sigterm_resistant_elevated_listener() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        use serde_json::json;

        struct PathRestore(Option<std::ffi::OsString>);

        impl Drop for PathRestore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(path) => unsafe { std::env::set_var("PATH", path) },
                    None => unsafe { std::env::remove_var("PATH") },
                }
            }
        }

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sing-box-tui-tun-disable-repro-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("reproduction directory creates");
        let fake_sudo = root.join("sudo");
        std::fs::write(
            &fake_sudo,
            b"#!/bin/sh\nif [ \"$1\" = \"-n\" ]; then shift; fi\nif [ \"$1\" = \"kill\" ]; then shift; exec /bin/kill \"$@\"; fi\n\"$@\" &\nchild=$!\nwait \"$child\"\n",
        )
        .expect("fake sudo writes");
        let mut permissions = std::fs::metadata(&fake_sudo)
            .expect("fake sudo metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fake_sudo, permissions).expect("fake sudo is executable");

        let original_path = std::env::var_os("PATH");
        let mut search_paths = vec![root.clone()];
        if let Some(path) = original_path.as_deref() {
            search_paths.extend(std::env::split_paths(path));
        }
        let test_path = std::env::join_paths(search_paths).expect("test PATH joins");
        let _path_restore = PathRestore(original_path);
        unsafe { std::env::set_var("PATH", test_path) };

        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("loopback reproduction port reserves");
        let port = reservation
            .local_addr()
            .expect("reproduction port resolves")
            .port();
        drop(reservation);

        let config = root.join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec_pretty(&json!({
                "log": { "level": "error" },
                "inbounds": [{
                    "type": "mixed",
                    "tag": "repro-mixed-in",
                    "listen": "127.0.0.1",
                    "listen_port": port
                }],
                "outbounds": [{ "type": "direct", "tag": "direct" }]
            }))
            .expect("reproduction config serializes"),
        )
        .expect("reproduction config writes");
        let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".local/sing-box");
        assert!(
            executable.is_file(),
            "targeted reproduction needs {}",
            executable.display()
        );
        let wrapper = std::process::Command::new(&fake_sudo)
            .arg("-n")
            .arg(&executable)
            .arg("run")
            .arg("--config")
            .arg(&config)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("fake elevated wrapper starts");
        let wrapper_pid = wrapper.id();
        let ready_deadline = Instant::now() + Duration::from_secs(15);
        let listener_pid = loop {
            let listener_pid = process_descendant_pids(wrapper_pid)
                .expect("fake elevated wrapper descendants can be inspected")
                .into_iter()
                .next();
            let port_is_owned = std::net::TcpListener::bind(("127.0.0.1", port)).is_err();
            if let Some(listener_pid) = listener_pid
                && port_is_owned
            {
                break listener_pid;
            }
            assert!(
                Instant::now() < ready_deadline,
                "isolated sing-box did not start on loopback port {port}"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let stop_status = std::process::Command::new("/bin/kill")
            .args(["-STOP", &listener_pid.to_string()])
            .status()
            .expect("isolated sing-box can be suspended");
        assert!(stop_status.success(), "isolated sing-box suspends");

        let mut managed =
            ManagedSingBoxCore::with_backend(executable, config, false, SystemProcessBackend);
        managed.ownership = Ownership::Elevated {
            pid: listener_pid,
            wrapper,
        };

        let started = Instant::now();
        let stop_result = managed.stop_owned_process();
        let elapsed = started.elapsed();
        let port_was_still_owned = std::net::TcpListener::bind(("127.0.0.1", port)).is_err();

        if stop_result.is_err() {
            let ownership = std::mem::replace(&mut managed.ownership, Ownership::Stopped);
            if process_alive_via_ps(listener_pid) {
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-CONT", &listener_pid.to_string()])
                    .status();
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-9", &listener_pid.to_string()])
                    .status();
            }
            if let Ownership::Elevated { mut wrapper, .. } = ownership {
                let _ = wrapper.wait();
            }
        }
        std::fs::remove_dir_all(&root).expect("reproduction directory removes");

        assert!(
            stop_result.is_ok() && !port_was_still_owned,
            "stable TUN-disable reproduction: stop_result={}; loopback port {port} still owned={port_was_still_owned}; elapsed={elapsed:?}",
            stop_result
                .as_ref()
                .map(|_| "ok".to_string())
                .unwrap_or_else(|error| format!("{error:#}"))
        );
    }
}

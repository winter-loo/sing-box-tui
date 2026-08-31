use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::node_runtime_manager::NodeRuntimeTraversal;
use crate::sustained_quality::{SustainedProbeOutcome, probe_sustained_over_proxy};
use crate::usability_probe::{
    BoundedProbeProcessExit, UsabilityProbeJobEvent, UsabilityProbeNodeResult,
    UsabilityProbeProgress, UsabilityProbeRunCompletion, run_bounded_probe_process,
};

pub(crate) const AGY_EXECUTABLE_ENV: &str = "SING_BOX_TUI_AGY_EXECUTABLE";
const GITHUB_PREFILTER_URL: &str = "https://github.com/";
const GITHUB_HOST: &str = "github.com";
const GITHUB_PORT: u16 = 22;
const GITHUB_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const MAX_SSH_BANNER_LINES: usize = 50;
const MAX_SSH_LINE_BYTES: usize = 1024;
const AGY_PREFILTER_URL: &str = "http://www.gstatic.com/generate_204";
const AGY_PREFILTER_TIMEOUT_MS: u64 = 2_000;
const AGY_PROMPT: &str = "Reply with exactly: OK";
const AGY_PRINT_TIMEOUT_ARG: &str = "60s";
const AGY_PROCESS_TIMEOUT: Duration = Duration::from_secs(65);
const MAX_AGY_DIAGNOSTIC_BYTES: u64 = 64 * 1024;
const MAX_AGY_ERROR_CHARS: usize = 800;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BuiltinProbeKind {
    Streaming,
    GithubSsh,
    AgyGemini,
}

#[derive(Clone, Debug)]
pub(crate) struct BuiltinProbeContext {
    pub(crate) config_path: PathBuf,
    pub(crate) sing_box_executable: PathBuf,
    pub(crate) streaming_prefilter_url: String,
    pub(crate) streaming_target_url: String,
    pub(crate) connectivity_timeout_ms: u64,
    pub(crate) agy_executable: PathBuf,
}

pub(crate) fn run_builtin_probe(
    kind: BuiltinProbeKind,
    candidates: Vec<String>,
    context: BuiltinProbeContext,
    sender: mpsc::Sender<UsabilityProbeJobEvent>,
    cancelled: Arc<AtomicBool>,
) {
    let result = match kind {
        BuiltinProbeKind::Streaming => {
            run_streaming_probe(candidates, &context, &sender, &cancelled)
        }
        BuiltinProbeKind::GithubSsh => {
            run_github_ssh_probe(candidates, &context, &sender, &cancelled)
        }
        BuiltinProbeKind::AgyGemini => {
            run_agy_gemini_probe(candidates, &context, &sender, &cancelled)
        }
    };
    if let Err(error) = result {
        finish(
            &sender,
            false,
            None,
            Some(format!(
                "built-in {} probe incomplete: {error:#}",
                kind.label()
            )),
            BTreeMap::new(),
        );
    }
}

impl BuiltinProbeKind {
    fn label(self) -> &'static str {
        match self {
            Self::Streaming => "Streaming",
            Self::GithubSsh => "GitHub SSH",
            Self::AgyGemini => "Agy Gemini",
        }
    }
}

fn run_streaming_probe(
    candidates: Vec<String>,
    context: &BuiltinProbeContext,
    sender: &mpsc::Sender<UsabilityProbeJobEvent>,
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut runtime = NodeRuntimeTraversal::start(
        &context.config_path,
        &context.sing_box_executable,
        &candidates,
        &context.streaming_prefilter_url,
        context.connectivity_timeout_ms,
    )?;
    let total = runtime.candidates().len();
    let mut results = BTreeMap::new();
    let mut transfer_total = 0;
    let mut transfer_completed = 0;
    let mut accepted = 0;
    send_status(
        sender,
        format!("Scanning {total} candidate(s) for Streaming reachability..."),
        None,
        false,
        progress("HTTPS", 0, total, "Stream", 0, 0, 0),
    );
    for (index, expected) in candidates.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            finish_cancelled(sender, results);
            return Ok(());
        }
        send_status(
            sender,
            format!(
                "Streaming prefilter checking {}/{}: {expected}",
                index + 1,
                total
            ),
            Some(expected.clone()),
            false,
            progress(
                "HTTPS",
                index,
                total,
                "Stream",
                transfer_completed,
                transfer_total,
                accepted,
            ),
        );
        let candidate = runtime
            .next()?
            .context("node runtime ended before every Streaming candidate was scanned")?;
        if candidate.node != *expected || candidate.scanned != index + 1 {
            bail!("node runtime returned candidates out of order");
        }
        if !candidate.reachable {
            let result = UsabilityProbeNodeResult {
                node: candidate.node.clone(),
                usable: false,
                detail: Some("Streaming HTTPS prefilter failed".to_string()),
                sustained_quality: None,
            };
            results.insert(candidate.node.clone(), result.clone());
            let _ = sender.send(UsabilityProbeJobEvent::Progress(result));
            continue;
        }

        transfer_total += 1;
        send_status(
            sender,
            format!(
                "Streaming prefilter scanned {}/{}; checking 512 KiB transfer for {}",
                candidate.scanned, total, candidate.node
            ),
            Some(candidate.node.clone()),
            true,
            progress(
                "HTTPS",
                candidate.scanned,
                total,
                "Stream",
                transfer_completed,
                transfer_total,
                accepted,
            ),
        );
        let quality = probe_sustained_over_proxy(
            candidate.node.clone(),
            runtime.proxy_url(),
            &context.streaming_target_url,
            cancelled,
        );
        let (usable, detail, attributable) = match &quality.outcome {
            SustainedProbeOutcome::Completed(completion) => (
                true,
                format!(
                    "Streaming {:.1} MiB/s · first byte {}ms · 512 KiB in {}ms",
                    completion.throughput_bytes_per_second as f64 / (1024.0 * 1024.0),
                    completion.first_byte_ms,
                    completion.completion_ms
                ),
                true,
            ),
            SustainedProbeOutcome::TransferFailed { detail } => (false, detail.clone(), true),
            SustainedProbeOutcome::RuntimeFailed { detail } => {
                finish(sender, false, None, Some(detail.clone()), results);
                return Ok(());
            }
            SustainedProbeOutcome::Cancelled => {
                finish_cancelled(sender, results);
                return Ok(());
            }
        };
        transfer_completed += 1;
        accepted += usize::from(usable);
        let result = UsabilityProbeNodeResult {
            node: candidate.node.clone(),
            usable,
            detail: Some(detail),
            sustained_quality: attributable.then_some(quality),
        };
        results.insert(candidate.node, result.clone());
        let _ = sender.send(UsabilityProbeJobEvent::Progress(result));
    }
    finish(
        sender,
        true,
        Some(format!(
            "Streaming available on {accepted}/{transfer_completed} assessed node(s)"
        )),
        None,
        results,
    );
    Ok(())
}

fn run_github_ssh_probe(
    candidates: Vec<String>,
    context: &BuiltinProbeContext,
    sender: &mpsc::Sender<UsabilityProbeJobEvent>,
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut runtime = NodeRuntimeTraversal::start(
        &context.config_path,
        &context.sing_box_executable,
        &candidates,
        GITHUB_PREFILTER_URL,
        context.connectivity_timeout_ms,
    )?;
    let total = runtime.candidates().len();
    let mut results = BTreeMap::new();
    let mut tcp_total = 0;
    let mut tcp_completed = 0;
    let mut accepted = 0;
    send_status(
        sender,
        format!("Scanning {total} candidate(s) for GitHub HTTPS reachability..."),
        None,
        false,
        progress("HTTPS", 0, total, "TCP 22", 0, 0, 0),
    );
    for (index, expected) in candidates.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            finish_cancelled(sender, results);
            return Ok(());
        }
        send_status(
            sender,
            format!(
                "GitHub HTTPS prefilter checking {}/{}: {expected}",
                index + 1,
                total
            ),
            Some(expected.clone()),
            false,
            progress(
                "HTTPS",
                index,
                total,
                "TCP 22",
                tcp_completed,
                tcp_total,
                accepted,
            ),
        );
        let candidate = runtime
            .next()?
            .context("node runtime ended before every GitHub candidate was scanned")?;
        if candidate.node != *expected || candidate.scanned != index + 1 {
            bail!("node runtime returned candidates out of order");
        }
        if !candidate.reachable {
            let result = UsabilityProbeNodeResult {
                node: candidate.node.clone(),
                usable: false,
                detail: Some("GitHub HTTPS prefilter failed".to_string()),
                sustained_quality: None,
            };
            results.insert(candidate.node.clone(), result.clone());
            let _ = sender.send(UsabilityProbeJobEvent::Progress(result));
            continue;
        }
        tcp_total += 1;
        send_status(
            sender,
            format!(
                "GitHub HTTPS prefilter scanned {}/{}; checking TCP 22 for {}",
                candidate.scanned, total, candidate.node
            ),
            Some(candidate.node.clone()),
            true,
            progress(
                "HTTPS",
                candidate.scanned,
                total,
                "TCP 22",
                tcp_completed,
                tcp_total,
                accepted,
            ),
        );
        let (usable, detail) = probe_github_ssh(runtime.proxy_url())?;
        tcp_completed += 1;
        accepted += usize::from(usable);
        let result = UsabilityProbeNodeResult {
            node: candidate.node.clone(),
            usable,
            detail: Some(detail),
            sustained_quality: None,
        };
        results.insert(candidate.node, result.clone());
        let _ = sender.send(UsabilityProbeJobEvent::Progress(result));
    }
    finish(
        sender,
        true,
        Some(format!(
            "GitHub SSH available on {accepted}/{tcp_completed} assessed node(s)"
        )),
        None,
        results,
    );
    Ok(())
}

fn run_agy_gemini_probe(
    candidates: Vec<String>,
    context: &BuiltinProbeContext,
    sender: &mpsc::Sender<UsabilityProbeJobEvent>,
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut runtime = NodeRuntimeTraversal::start(
        &context.config_path,
        &context.sing_box_executable,
        &candidates,
        AGY_PREFILTER_URL,
        AGY_PREFILTER_TIMEOUT_MS,
    )?;
    let total = runtime.candidates().len();
    let mut results = BTreeMap::new();
    let mut agy_total = 0;
    let mut agy_completed = 0;
    let mut accepted = 0;
    send_status(
        sender,
        format!("Scanning {total} candidate(s) with the 2-second Agy prefilter..."),
        None,
        false,
        progress("HTTPS", 0, total, "Agy", 0, 0, 0),
    );
    for (index, expected) in candidates.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            finish_cancelled(sender, results);
            return Ok(());
        }
        send_status(
            sender,
            format!(
                "Agy ordinary-connectivity prefilter checking {}/{}: {expected}",
                index + 1,
                total
            ),
            Some(expected.clone()),
            false,
            progress(
                "HTTPS",
                index,
                total,
                "Agy",
                agy_completed,
                agy_total,
                accepted,
            ),
        );
        let candidate = runtime
            .next()?
            .context("node runtime ended before every Agy Gemini candidate was scanned")?;
        if candidate.node != *expected || candidate.scanned != index + 1 {
            bail!("node runtime returned candidates out of order");
        }
        if !candidate.reachable {
            let result = UsabilityProbeNodeResult {
                node: candidate.node.clone(),
                usable: false,
                detail: Some("Agy 2-second ordinary-connectivity prefilter failed".to_string()),
                sustained_quality: None,
            };
            results.insert(candidate.node.clone(), result.clone());
            let _ = sender.send(UsabilityProbeJobEvent::Progress(result));
            continue;
        }

        agy_total += 1;
        send_status(
            sender,
            format!(
                "Agy prefilter scanned {}/{}; checking real Agy Gemini request for {}",
                candidate.scanned, total, candidate.node
            ),
            Some(candidate.node.clone()),
            true,
            progress(
                "HTTPS",
                candidate.scanned,
                total,
                "Agy",
                agy_completed,
                agy_total,
                accepted,
            ),
        );
        match run_agy_command(&context.agy_executable, runtime.proxy_url(), cancelled) {
            AgyCommandOutcome::Succeeded(elapsed_ms) => {
                agy_completed += 1;
                accepted += 1;
                let result = UsabilityProbeNodeResult {
                    node: candidate.node.clone(),
                    usable: true,
                    detail: Some(format!("Agy Gemini command succeeded in {elapsed_ms}ms")),
                    sustained_quality: None,
                };
                results.insert(candidate.node, result.clone());
                let _ = sender.send(UsabilityProbeJobEvent::Progress(result));
            }
            AgyCommandOutcome::NodeRejected(detail) => {
                agy_completed += 1;
                let result = UsabilityProbeNodeResult {
                    node: candidate.node.clone(),
                    usable: false,
                    detail: Some(detail),
                    sustained_quality: None,
                };
                results.insert(candidate.node, result.clone());
                let _ = sender.send(UsabilityProbeJobEvent::Progress(result));
            }
            AgyCommandOutcome::Cancelled => {
                finish_cancelled(sender, results);
                return Ok(());
            }
            AgyCommandOutcome::InfrastructureFailure { code, detail } => {
                finish(
                    sender,
                    false,
                    None,
                    Some(format!("Agy Gemini probe incomplete ({code}): {detail}")),
                    results,
                );
                return Ok(());
            }
        }
    }
    finish(
        sender,
        true,
        Some(format!(
            "Agy Gemini command succeeded on {accepted}/{agy_completed} assessed node(s)"
        )),
        None,
        results,
    );
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AgyCommandOutcome {
    Succeeded(u128),
    NodeRejected(String),
    InfrastructureFailure { code: &'static str, detail: String },
    Cancelled,
}

fn run_agy_command(
    executable: &Path,
    proxy_url: &str,
    cancelled: &AtomicBool,
) -> AgyCommandOutcome {
    let diagnostic_log = match AgyDiagnosticLog::create() {
        Ok(log) => log,
        Err(error) => {
            return AgyCommandOutcome::InfrastructureFailure {
                code: "agy_log_setup_failed",
                detail: format!("Could not create the private Agy diagnostic log: {error}"),
            };
        }
    };
    let mut command = build_agy_command(executable, proxy_url, diagnostic_log.path());
    let output = match run_bounded_probe_process(&mut command, AGY_PROCESS_TIMEOUT, cancelled) {
        Ok(output) => output,
        Err(error) => {
            return AgyCommandOutcome::InfrastructureFailure {
                code: "agy_start_failed",
                detail: format!("Could not start or supervise Agy: {error:#}"),
            };
        }
    };
    let log_output = diagnostic_log.read_tail().unwrap_or_default();
    let diagnostic = format!("{}\n{}\n{log_output}", output.stdout, output.stderr);
    match output.exit {
        BoundedProbeProcessExit::Cancelled => AgyCommandOutcome::Cancelled,
        BoundedProbeProcessExit::TimedOut if agy_authentication_required(&diagnostic) => {
            AgyCommandOutcome::InfrastructureFailure {
                code: "agy_authentication_required",
                detail: "Agy Gemini authentication is required".to_string(),
            }
        }
        BoundedProbeProcessExit::TimedOut => AgyCommandOutcome::InfrastructureFailure {
            code: "agy_timeout",
            detail: format!(
                "Agy Gemini did not finish within {} seconds{}",
                AGY_PROCESS_TIMEOUT.as_secs(),
                diagnostic_suffix(&diagnostic)
            ),
        },
        BoundedProbeProcessExit::Completed(status) => classify_completed_agy_command(
            status.success(),
            output.elapsed.as_millis(),
            &diagnostic,
        ),
    }
}

fn classify_completed_agy_command(
    success: bool,
    elapsed_ms: u128,
    diagnostic: &str,
) -> AgyCommandOutcome {
    if success {
        return AgyCommandOutcome::Succeeded(elapsed_ms);
    }
    match classify_agy_failure(diagnostic) {
        AgyFailureDisposition::NodeRejected(detail) => AgyCommandOutcome::NodeRejected(detail),
        AgyFailureDisposition::InfrastructureFailure { code, detail } => {
            AgyCommandOutcome::InfrastructureFailure { code, detail }
        }
    }
}

fn build_agy_command(executable: &Path, proxy_url: &str, diagnostic_log: &Path) -> Command {
    let mut command = Command::new(executable);
    command.args([
        "--agent",
        "gemini",
        "--print",
        AGY_PROMPT,
        "--output-format",
        "json",
        "--print-timeout",
        AGY_PRINT_TIMEOUT_ARG,
        "--disable-slash-commands",
        "--log-file",
    ]);
    command.arg(diagnostic_log);
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(name, proxy_url);
    }
    command.env("NO_PROXY", "").env("no_proxy", "");
    command
}

fn agy_authentication_required(output: &str) -> bool {
    let normalized = output.to_ascii_lowercase();
    [
        "authentication required",
        "unauthenticated",
        "login required",
        "please login",
        "please log in",
        "invalid_grant",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AgyFailureDisposition {
    NodeRejected(String),
    InfrastructureFailure { code: &'static str, detail: String },
}

fn classify_agy_failure(diagnostic: &str) -> AgyFailureDisposition {
    if diagnostic
        .to_ascii_lowercase()
        .contains("user location is not supported for the api use")
    {
        return AgyFailureDisposition::NodeRejected(
            "Agy Gemini rejected this node: User location is not supported for the API use"
                .to_string(),
        );
    }
    if agy_authentication_required(diagnostic) {
        return AgyFailureDisposition::InfrastructureFailure {
            code: "agy_authentication_required",
            detail: "Agy Gemini authentication is required".to_string(),
        };
    }
    AgyFailureDisposition::InfrastructureFailure {
        code: "agy_process_failed",
        detail: format!("Agy Gemini process failed{}", diagnostic_suffix(diagnostic)),
    }
}

struct AgyDiagnosticLog {
    path: PathBuf,
}

impl AgyDiagnosticLog {
    fn create() -> std::io::Result<Self> {
        let directory = std::env::temp_dir();
        for _ in 0..8 {
            let path = directory.join(format!(
                "sing-box-tui-agy-{}-{:016x}.log",
                std::process::id(),
                rand::random::<u64>()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    drop(file);
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique diagnostic log name",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn read_tail(&self) -> std::io::Result<String> {
        let mut file = File::open(&self.path)?;
        let length = file.metadata()?.len();
        if length > MAX_AGY_DIAGNOSTIC_BYTES {
            file.seek(SeekFrom::Start(length - MAX_AGY_DIAGNOSTIC_BYTES))?;
        }
        let mut bytes = Vec::new();
        file.take(MAX_AGY_DIAGNOSTIC_BYTES)
            .read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

impl Drop for AgyDiagnosticLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn diagnostic_suffix(diagnostic: &str) -> String {
    extract_agy_error_detail(diagnostic)
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default()
}

fn extract_agy_error_detail(diagnostic: &str) -> Option<String> {
    let preferred_markers = [
        "failed_precondition",
        "resource_exhausted",
        "permission_denied",
        "unauthenticated",
        "deadline_exceeded",
        "pre-invocation hook",
        "agent executor error",
    ];
    let preferred = diagnostic.lines().rev().find(|line| {
        let normalized = line.to_ascii_lowercase();
        preferred_markers
            .iter()
            .any(|marker| normalized.contains(marker))
    });
    let fallback = || {
        diagnostic.lines().rev().find(|line| {
            let normalized = line.to_ascii_lowercase();
            !line.trim().is_empty()
                && (normalized.contains("error") || normalized.contains("failed"))
        })
    };
    preferred
        .or_else(fallback)
        .and_then(sanitize_agy_error_detail)
}

fn sanitize_agy_error_detail(line: &str) -> Option<String> {
    let line = line.trim();
    let without_prefix = if line.starts_with('[') {
        line.split_once("] ").map_or(line, |(_, detail)| detail)
    } else {
        line
    };
    let collapsed = without_prefix
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let redacted = collapsed
        .split_whitespace()
        .map(|word| {
            if looks_like_email(word) {
                "<redacted-email>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let bounded = redacted
        .chars()
        .take(MAX_AGY_ERROR_CHARS)
        .collect::<String>();
    (!bounded.is_empty()).then_some(bounded)
}

fn looks_like_email(word: &str) -> bool {
    let trimmed = word.trim_matches(|character: char| {
        character.is_ascii_punctuation() && !matches!(character, '@' | '.' | '_' | '-' | '+')
    });
    let Some((local, domain)) = trimmed.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
}

fn probe_github_ssh(proxy_url: &str) -> Result<(bool, String)> {
    let parsed = reqwest::Url::parse(proxy_url).context("invalid isolated proxy URL")?;
    if parsed.scheme() != "http" {
        bail!("isolated proxy did not use HTTP");
    }
    let host = parsed
        .host_str()
        .context("isolated proxy URL omitted host")?;
    let port = parsed.port().context("isolated proxy URL omitted port")?;
    let address = (host, port)
        .to_socket_addrs()
        .context("failed to resolve isolated proxy")?
        .next()
        .context("isolated proxy had no socket address")?;
    let mut stream = TcpStream::connect_timeout(&address, GITHUB_TIMEOUT)
        .context("failed to connect to isolated proxy")?;
    stream.set_read_timeout(Some(GITHUB_TIMEOUT))?;
    stream.set_write_timeout(Some(GITHUB_TIMEOUT))?;
    let started = Instant::now();
    let authority = format!("{GITHUB_HOST}:{GITHUB_PORT}");
    write!(
        stream,
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: close\r\n\r\n"
    )?;
    stream.flush()?;

    let mut pending = Vec::new();
    let header = match read_until(
        &mut stream,
        &mut pending,
        b"\r\n\r\n",
        MAX_HTTP_HEADER_BYTES,
    ) {
        Ok(header) => header,
        Err(_) => return Ok((false, "GitHub SSH connection closed".to_string())),
    };
    let status_line = String::from_utf8_lossy(
        header
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or(&header),
    );
    let status = status_line
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok());
    if status != Some(200) {
        return Ok((
            false,
            status.map_or_else(
                || "invalid CONNECT response".to_string(),
                |status| format!("CONNECT rejected ({status})"),
            ),
        ));
    }
    for _ in 0..MAX_SSH_BANNER_LINES {
        let line = match read_until(&mut stream, &mut pending, b"\n", MAX_SSH_LINE_BYTES) {
            Ok(line) => line,
            Err(_) => return Ok((false, "GitHub SSH banner not received".to_string())),
        };
        let banner = String::from_utf8_lossy(&line);
        let banner = banner.trim_end_matches(['\r', '\n']);
        if banner.starts_with("SSH-") {
            let protocol = banner.split('-').take(2).collect::<Vec<_>>().join("-");
            return Ok((
                true,
                format!(
                    "GitHub SSH banner {protocol} in {}ms",
                    started.elapsed().as_millis()
                ),
            ));
        }
    }
    Ok((false, "GitHub SSH banner not received".to_string()))
}

fn read_until(
    stream: &mut TcpStream,
    pending: &mut Vec<u8>,
    marker: &[u8],
    limit: usize,
) -> Result<Vec<u8>> {
    loop {
        if let Some(index) = pending
            .windows(marker.len())
            .position(|window| window == marker)
        {
            let end = index + marker.len();
            let tail = pending.split_off(end);
            let head = std::mem::replace(pending, tail);
            return Ok(head);
        }
        if pending.len() >= limit {
            bail!("proxy response exceeded limit");
        }
        let mut buffer = [0_u8; 4096];
        let read_limit = buffer.len().min(limit - pending.len());
        let count = stream.read(&mut buffer[..read_limit])?;
        if count == 0 {
            bail!("proxy response ended early");
        }
        pending.extend_from_slice(&buffer[..count]);
    }
}

fn progress(
    stage_one_label: &str,
    stage_one_completed: usize,
    stage_one_total: usize,
    stage_two_label: &str,
    stage_two_completed: usize,
    stage_two_total: usize,
    accepted: usize,
) -> UsabilityProbeProgress {
    UsabilityProbeProgress {
        stage_one_completed,
        stage_one_total,
        stage_two_completed,
        stage_two_total,
        stage_one_label: stage_one_label.to_string(),
        stage_two_label: stage_two_label.to_string(),
        accepted,
    }
}

fn send_status(
    sender: &mpsc::Sender<UsabilityProbeJobEvent>,
    message: String,
    node: Option<String>,
    candidate: bool,
    progress: UsabilityProbeProgress,
) {
    let _ = sender.send(UsabilityProbeJobEvent::Status {
        message,
        node,
        candidate,
        progress: Some(progress),
    });
}

fn finish_cancelled(
    sender: &mpsc::Sender<UsabilityProbeJobEvent>,
    results: BTreeMap<String, UsabilityProbeNodeResult>,
) {
    finish(
        sender,
        false,
        None,
        Some("built-in usability probe was cancelled".to_string()),
        results,
    );
}

fn finish(
    sender: &mpsc::Sender<UsabilityProbeJobEvent>,
    complete: bool,
    summary: Option<String>,
    diagnostic: Option<String>,
    results: BTreeMap<String, UsabilityProbeNodeResult>,
) {
    let _ = sender.send(UsabilityProbeJobEvent::Finished(
        UsabilityProbeRunCompletion {
            complete,
            summary,
            diagnostic,
            results: results.into_values().collect(),
        },
    ));
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn agy_command_is_bound_to_the_isolated_proxy_and_real_gemini_agent() {
        let command = build_agy_command(
            Path::new("agy-test"),
            "http://127.0.0.1:43210",
            Path::new("agy-diagnostic.log"),
        );
        assert_eq!(command.get_program(), "agy-test");
        assert_eq!(
            command
                .get_args()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "--agent",
                "gemini",
                "--print",
                AGY_PROMPT,
                "--output-format",
                "json",
                "--print-timeout",
                AGY_PRINT_TIMEOUT_ARG,
                "--disable-slash-commands",
                "--log-file",
                "agy-diagnostic.log",
            ]
        );
        let environment = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let environment_value = |name: &str| {
            if cfg!(windows) {
                environment
                    .iter()
                    .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
                    .and_then(|(_, value)| value.as_deref())
            } else {
                environment.get(name).and_then(Option::as_deref)
            }
        };
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            assert_eq!(environment_value(name), Some("http://127.0.0.1:43210"));
        }
        assert_eq!(environment_value("NO_PROXY"), Some(""));
        assert_eq!(environment_value("no_proxy"), Some(""));
    }

    #[test]
    fn agy_authentication_detection_is_case_insensitive_and_bounded_to_known_markers() {
        assert!(agy_authentication_required("Error: LOGIN REQUIRED"));
        assert!(agy_authentication_required("request was unauthenticated"));
        assert!(!agy_authentication_required(
            "Print mode: not authenticated, trying silent auth"
        ));
        assert!(!agy_authentication_required("Gemini request failed"));
    }

    #[test]
    fn agy_location_rejection_is_node_attributable_instead_of_aborting_the_run() {
        assert_eq!(
            classify_agy_failure(
                "Print mode: not authenticated, trying silent auth\n\
                 pre-invocation hook: FAILED_PRECONDITION (code 400): User location is not supported for the API use."
            ),
            AgyFailureDisposition::NodeRejected(
                "Agy Gemini rejected this node: User location is not supported for the API use"
                    .to_string()
            )
        );
    }

    #[test]
    fn agy_success_wins_over_transient_silent_auth_logging() {
        assert_eq!(
            classify_completed_agy_command(
                true,
                42,
                "Print mode: not authenticated, trying silent auth"
            ),
            AgyCommandOutcome::Succeeded(42)
        );
    }

    #[test]
    fn agy_unknown_failure_keeps_the_underlying_safe_error() {
        assert_eq!(
            classify_agy_failure(
                "[2026-08-31T00:00:00Z ERROR agy] RESOURCE_EXHAUSTED: daily quota failed for person@example.com"
            ),
            AgyFailureDisposition::InfrastructureFailure {
                code: "agy_process_failed",
                detail: "Agy Gemini process failed: RESOURCE_EXHAUSTED: daily quota failed for <redacted-email>"
                    .to_string(),
            }
        );
    }

    #[test]
    fn agy_diagnostic_log_is_private_to_the_run_and_removed_after_use() {
        let log = AgyDiagnosticLog::create().expect("diagnostic log should be created");
        let path = log.path().to_path_buf();
        assert!(path.exists());
        drop(log);
        assert!(!path.exists());
    }
}

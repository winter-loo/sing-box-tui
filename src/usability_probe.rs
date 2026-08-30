use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client as AsyncClient;
use serde::Deserialize;
use tokio::runtime::Builder as TokioRuntimeBuilder;
use tokio::task::JoinSet;

use crate::automatic_selection::{NodeViewId, RankingPolicy};
use crate::controller::{ProbeOutcome, measure_usability_probe_outcome};

pub(crate) const USABILITY_PROBE_DIRECTORY_ENV: &str = "SING_BOX_TUI_USABILITY_PROBE_DIR";
const DEFAULT_MANIFEST_DIRECTORY: &str = "usability-probes";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_DISCOVERED_MANIFESTS: usize = 64;
const MAX_DIRECTORY_ENTRIES_SCANNED: usize = 1024;
const MAX_DISCOVERY_DIAGNOSTICS: usize = 8;
const MAX_DIAGNOSTIC_CHARS: usize = 240;
const MAX_PROTOCOL_LINE_BYTES: usize = 64 * 1024;
const MAX_PROTOCOL_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;
const MAX_RESULT_DETAIL_CHARS: usize = 512;
const MAX_SELECTOR_MEMBERS: usize = 4096;
const URL_PROBE_CONCURRENCY: usize = 8;
const URL_TARGET_TIMEOUT: Duration = Duration::from_secs(5);
const URL_CONTROLLER_TIMEOUT: Duration = Duration::from_secs(7);

#[derive(Clone, Copy)]
struct UrlProbeTimeouts {
    target: Duration,
    controller: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UsabilityProbeSource {
    Url(String),
    Executable {
        executable: PathBuf,
        args: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UsabilityProbeManifest {
    pub(crate) id: NodeViewId,
    pub(crate) label: String,
    pub(crate) ranking_policy: RankingPolicy,
    pub(crate) source: UsabilityProbeSource,
    pub(crate) source_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManifestDiagnostic {
    pub(crate) path: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UsabilityProbeDiscovery {
    pub(crate) manifests: Vec<UsabilityProbeManifest>,
    pub(crate) diagnostics: Vec<ManifestDiagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UsabilityProbeNodeResult {
    pub(crate) node: String,
    pub(crate) usable: bool,
    pub(crate) detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UsabilityProbeRunCompletion {
    pub(crate) complete: bool,
    pub(crate) summary: Option<String>,
    pub(crate) diagnostic: Option<String>,
    pub(crate) results: Vec<UsabilityProbeNodeResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UsabilityProbeJobEvent {
    Progress(UsabilityProbeNodeResult),
    Finished(UsabilityProbeRunCompletion),
}

pub(crate) struct UsabilityProbeJob {
    receiver: Receiver<UsabilityProbeJobEvent>,
    worker: Option<JoinHandle<()>>,
    cancellation: Arc<AtomicBool>,
}

struct SpawnedProgramGuard {
    child: Option<Child>,
}

impl SpawnedProgramGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn into_child(mut self) -> Child {
        self.child
            .take()
            .expect("spawned program guard must contain its child")
    }
}

impl Drop for SpawnedProgramGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            // `std::process::Child` does not kill on drop. Keep ownership behind this guard until
            // the worker thread has actually started so an OS thread-spawn failure cannot orphan
            // an already-running manifest executable.
            kill_and_wait(child);
        }
    }
}

impl UsabilityProbeJob {
    pub(crate) fn try_recv(&self) -> Option<UsabilityProbeJobEvent> {
        self.receiver.try_recv().ok()
    }

    pub(crate) fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.store(true, Ordering::Relaxed);
    }
}

impl Drop for UsabilityProbeJob {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Relaxed);
        self.join();
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ProgramOutput {
    NodeResult {
        node: String,
        usable: bool,
        #[serde(default)]
        detail: Option<String>,
    },
    Summary {
        complete: bool,
        #[serde(default)]
        message: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestDocument {
    id: String,
    label: String,
    #[serde(alias = "ranking_policy")]
    ranking: RankingPolicy,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    executable: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
}

pub(crate) fn usability_probe_manifest_directory(config_path: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os(USABILITY_PROBE_DIRECTORY_ENV)
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(DEFAULT_MANIFEST_DIRECTORY)
}

pub(crate) fn discover_usability_probe_manifests(
    directory: &Path,
) -> Result<UsabilityProbeDiscovery> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UsabilityProbeDiscovery::default());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read usability probe manifest directory {}",
                    directory.display()
                )
            });
        }
    };
    let mut paths = BTreeSet::new();
    let mut discovery = UsabilityProbeDiscovery::default();
    let mut candidates_truncated = false;
    for (entries_scanned, entry) in entries.enumerate() {
        if entries_scanned == MAX_DIRECTORY_ENTRIES_SCANNED {
            push_diagnostic(
                &mut discovery,
                directory,
                format!(
                    "manifest discovery stopped after {MAX_DIRECTORY_ENTRIES_SCANNED} directory entries; use a dedicated manifest directory"
                ),
            );
            break;
        }
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
                {
                    paths.insert(path);
                    if paths.len() > MAX_DISCOVERED_MANIFESTS + 1 {
                        paths.pop_last();
                    }
                    candidates_truncated |= paths.len() > MAX_DISCOVERED_MANIFESTS;
                }
            }
            Err(error) => push_diagnostic(
                &mut discovery,
                directory,
                format!("failed to inspect manifest directory entry: {error}"),
            ),
        }
    }
    if candidates_truncated {
        push_diagnostic(
            &mut discovery,
            directory,
            format!(
                "manifest discovery is limited to {MAX_DISCOVERED_MANIFESTS} JSON files; extra files were ignored"
            ),
        );
    }

    let mut ids = BTreeSet::new();
    for path in paths.into_iter().take(MAX_DISCOVERED_MANIFESTS) {
        match load_usability_probe_manifest(&path) {
            Ok(manifest) if ids.insert(manifest.id.clone()) => {
                discovery.manifests.push(manifest);
            }
            Ok(manifest) => push_diagnostic(
                &mut discovery,
                &path,
                format!("duplicate usability probe id '{}'", manifest.id),
            ),
            Err(error) => push_diagnostic(&mut discovery, &path, format!("{error:#}")),
        }
    }
    Ok(discovery)
}

pub(crate) fn load_usability_probe_manifest(path: &Path) -> Result<UsabilityProbeManifest> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect manifest {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("manifest is not a regular file: {}", path.display());
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        bail!(
            "manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit: {}",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .with_context(|| format!("failed to open manifest {}", path.display()))?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read manifest {}", path.display()))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!(
            "manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit: {}",
            path.display()
        );
    }
    let document = serde_json::from_slice::<ManifestDocument>(&bytes)
        .with_context(|| format!("invalid usability probe manifest {}", path.display()))?;
    manifest_from_document(document, path)
}

pub(crate) fn spawn_usability_probe_job(
    manifest: UsabilityProbeManifest,
    mut selector_members: Vec<String>,
    controller_base_url: String,
    client: AsyncClient,
) -> Result<UsabilityProbeJob> {
    if selector_members.is_empty() {
        bail!(
            "cannot run usability probe '{}' with no selector members",
            manifest.id
        );
    }
    let mut seen = BTreeSet::new();
    let mut unique_members = Vec::new();
    for member in selector_members {
        if member.trim().is_empty()
            || member.chars().count() > 256
            || member.chars().any(char::is_control)
        {
            bail!("usability selector members must contain 1 to 256 printable characters");
        }
        if seen.insert(member.clone()) {
            if unique_members.len() == MAX_SELECTOR_MEMBERS {
                bail!("usability probes are limited to {MAX_SELECTOR_MEMBERS} selector members");
            }
            unique_members.push(member);
        }
    }
    selector_members = unique_members;
    let cancellation = Arc::new(AtomicBool::new(false));
    let worker_cancellation = Arc::clone(&cancellation);
    // Cancellation joins the worker without draining UI events. A bounded channel would let a
    // verbose-but-valid probe block on progress delivery forever, preventing cancellation and
    // application shutdown; selector membership already bounds the maximum retained event set.
    let (sender, receiver) = mpsc::channel();
    let worker = match manifest.source.clone() {
        UsabilityProbeSource::Url(url) => thread::Builder::new()
            .name(format!("usability-url-{}", manifest.id))
            .spawn(move || {
                run_url_probe(
                    selector_members,
                    url,
                    controller_base_url,
                    client,
                    sender,
                    worker_cancellation,
                );
            })
            .context("failed to start URL usability probe worker")?,
        UsabilityProbeSource::Executable { executable, args } => {
            // The executable and each argument remain separate OS strings all the way into
            // `Command`; never introducing a shell is the security property promised by the
            // manifest format, not merely an escaping convention.
            let mut command = Command::new(&executable);
            command
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let child = command.spawn().with_context(|| {
                format!(
                    "failed to start usability probe '{}' executable {}",
                    manifest.id,
                    executable.display()
                )
            })?;
            let child = SpawnedProgramGuard::new(child);
            thread::Builder::new()
                .name(format!("usability-program-{}", manifest.id))
                .spawn(move || {
                    run_program_probe(
                        child.into_child(),
                        selector_members,
                        sender,
                        worker_cancellation,
                    );
                })
                .context("failed to start executable usability probe worker")?
        }
    };
    Ok(UsabilityProbeJob {
        receiver,
        worker: Some(worker),
        cancellation,
    })
}

fn run_url_probe(
    selector_members: Vec<String>,
    url: String,
    controller_base_url: String,
    client: AsyncClient,
    sender: mpsc::Sender<UsabilityProbeJobEvent>,
    cancellation: Arc<AtomicBool>,
) {
    run_url_probe_with_timeouts(
        selector_members,
        url,
        controller_base_url,
        client,
        sender,
        cancellation,
        UrlProbeTimeouts {
            target: URL_TARGET_TIMEOUT,
            controller: URL_CONTROLLER_TIMEOUT,
        },
    );
}

fn run_url_probe_with_timeouts(
    selector_members: Vec<String>,
    url: String,
    controller_base_url: String,
    client: AsyncClient,
    sender: mpsc::Sender<UsabilityProbeJobEvent>,
    cancellation: Arc<AtomicBool>,
    timeouts: UrlProbeTimeouts,
) {
    let runtime = match TokioRuntimeBuilder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            send_finished(
                &sender,
                false,
                None,
                Some(format!("failed to create URL probe runtime: {error}")),
                BTreeMap::new(),
            );
            return;
        }
    };
    runtime.block_on(async move {
        let mut pending = selector_members.into_iter();
        let mut tasks = JoinSet::new();
        let mut results = BTreeMap::new();
        let mut infrastructure_diagnostics = Vec::new();
        for _ in 0..URL_PROBE_CONCURRENCY {
            let Some(node) = pending.next() else { break };
            spawn_url_probe_task(
                &mut tasks,
                client.clone(),
                controller_base_url.clone(),
                node,
                url.clone(),
                timeouts,
            );
        }
        while let Some(joined) = tasks.join_next().await {
            if cancellation.load(Ordering::Relaxed) {
                tasks.abort_all();
                send_finished(
                    &sender,
                    false,
                    None,
                    Some("URL usability probe was cancelled".to_string()),
                    results,
                );
                return;
            }
            match joined {
                Ok((node, outcome)) => match url_result_from_outcome(node, outcome) {
                    Ok(result) => {
                        results.insert(result.node.clone(), result.clone());
                        let _ = sender.send(UsabilityProbeJobEvent::Progress(result));
                    }
                    Err(diagnostic) => infrastructure_diagnostics.push(diagnostic),
                },
                Err(error) => {
                    infrastructure_diagnostics.push(format!("URL probe worker failed: {error}"))
                }
            }
            if let Some(node) = pending.next() {
                spawn_url_probe_task(
                    &mut tasks,
                    client.clone(),
                    controller_base_url.clone(),
                    node,
                    url.clone(),
                    timeouts,
                );
            }
        }
        let complete = infrastructure_diagnostics.is_empty();
        send_finished(
            &sender,
            complete,
            complete.then(|| format!("{} selector nodes assessed", results.len())),
            (!complete).then(|| bounded_diagnostic(infrastructure_diagnostics.join("; "))),
            results,
        );
    });
}

fn spawn_url_probe_task(
    tasks: &mut JoinSet<(String, ProbeOutcome)>,
    client: AsyncClient,
    controller_base_url: String,
    node: String,
    url: String,
    timeouts: UrlProbeTimeouts,
) {
    tasks.spawn(async move {
        let outcome = measure_usability_probe_outcome(
            client,
            &controller_base_url,
            &node,
            &url,
            timeouts.target,
            timeouts.controller,
        )
        .await;
        (node, outcome)
    });
}

fn url_result_from_outcome(
    node: String,
    outcome: ProbeOutcome,
) -> std::result::Result<UsabilityProbeNodeResult, String> {
    match outcome {
        ProbeOutcome::Reachable { delay_ms } => Ok(UsabilityProbeNodeResult {
            node,
            usable: true,
            detail: Some(format!("live HTTP response in {delay_ms}ms")),
        }),
        ProbeOutcome::Timeout => Ok(UsabilityProbeNodeResult {
            node,
            usable: false,
            detail: Some("live HTTP response timed out".to_string()),
        }),
        ProbeOutcome::TransportFailure { detail } => Err(format!(
            "failed to reach the Clash controller while probing {node}: {}",
            bounded_result_detail(detail)
        )),
        ProbeOutcome::ControllerFailure { status: 503 | 504 } => Ok(UsabilityProbeNodeResult {
            node,
            usable: false,
            detail: Some("target did not return a valid HTTP response".to_string()),
        }),
        ProbeOutcome::ControllerFailure { status } => Err(format!(
            "controller returned HTTP {status} while probing {node}"
        )),
        ProbeOutcome::InvalidMeasurement => Err(format!(
            "controller returned an invalid delay measurement for {node}"
        )),
        ProbeOutcome::Cancelled => Err(format!("live probe for {node} was cancelled")),
    }
}

fn run_program_probe(
    mut child: Child,
    selector_members: Vec<String>,
    sender: mpsc::Sender<UsabilityProbeJobEvent>,
    cancellation: Arc<AtomicBool>,
) {
    let Some(stdout) = child.stdout.take() else {
        kill_and_wait(&mut child);
        send_finished(
            &sender,
            false,
            None,
            Some("usability probe stdout was not piped".to_string()),
            BTreeMap::new(),
        );
        return;
    };
    let Some(stderr) = child.stderr.take() else {
        kill_and_wait(&mut child);
        send_finished(
            &sender,
            false,
            None,
            Some("usability probe stderr was not piped".to_string()),
            BTreeMap::new(),
        );
        return;
    };
    let (line_sender, line_receiver) = mpsc::sync_channel(64);
    let stdout_worker = thread::spawn(move || read_protocol_lines(stdout, line_sender));
    let stderr_worker = thread::spawn(move || read_bounded_stderr(stderr));
    let allowed_nodes = selector_members.into_iter().collect::<BTreeSet<_>>();
    let mut results = BTreeMap::new();
    let mut terminal_summary = None;
    let mut fatal_error = None;
    let mut stdout_closed = false;

    while !stdout_closed {
        if cancellation.load(Ordering::Relaxed) && fatal_error.is_none() {
            fatal_error = Some("usability probe was cancelled".to_string());
            let _ = child.kill();
        }
        match line_receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(Ok(line)) => {
                if fatal_error.is_some() {
                    continue;
                }
                match parse_program_output(&line) {
                    Ok(ProgramOutput::NodeResult {
                        node,
                        usable,
                        detail,
                    }) => {
                        if terminal_summary.is_some() {
                            fatal_error =
                                Some("node_result appeared after the terminal summary".to_string());
                            let _ = child.kill();
                            continue;
                        }
                        if !allowed_nodes.contains(&node) {
                            // Programs may traverse every configured selector. The TUI publishes
                            // only the immutable selector snapshot that started this manual run,
                            // so a result can never leak a node from another selector into the tab.
                            continue;
                        }
                        if results.contains_key(&node) {
                            fatal_error = Some(format!(
                                "usability probe emitted duplicate result for node {node}"
                            ));
                            let _ = child.kill();
                            continue;
                        }
                        let result = UsabilityProbeNodeResult {
                            node: node.clone(),
                            usable,
                            detail: detail.map(bounded_result_detail),
                        };
                        results.insert(node, result.clone());
                        let _ = sender.send(UsabilityProbeJobEvent::Progress(result));
                    }
                    Ok(ProgramOutput::Summary { complete, message }) => {
                        if terminal_summary.is_some() {
                            fatal_error = Some(
                                "usability probe emitted more than one terminal summary"
                                    .to_string(),
                            );
                            let _ = child.kill();
                        } else {
                            terminal_summary = Some((complete, message.map(bounded_result_detail)));
                        }
                    }
                    Err(error) => {
                        fatal_error = Some(error);
                        let _ = child.kill();
                    }
                }
            }
            Ok(Err(error)) => {
                if fatal_error.is_none() {
                    fatal_error = Some(error);
                    let _ = child.kill();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => stdout_closed = true,
        }
    }
    let status = child.wait();
    let _ = stdout_worker.join();
    let stderr = stderr_worker.join().unwrap_or_default();
    if fatal_error.is_none() {
        match status {
            Ok(status) if !status.success() => {
                fatal_error = Some(format!("usability probe exited with {status}"));
            }
            Err(error) => {
                fatal_error = Some(format!("failed to wait for usability probe: {error}"))
            }
            _ => {}
        }
    }
    let (reported_complete, summary) = terminal_summary.unwrap_or_else(|| {
        if fatal_error.is_none() {
            fatal_error = Some("usability probe did not emit a terminal summary".to_string());
        }
        (false, None)
    });
    if !reported_complete && fatal_error.is_none() {
        fatal_error = Some(
            summary
                .clone()
                .unwrap_or_else(|| "usability probe reported an incomplete run".to_string()),
        );
    }
    let complete = reported_complete && fatal_error.is_none();
    let diagnostic = combine_program_diagnostic(fatal_error, stderr);
    send_finished(&sender, complete, summary, diagnostic, results);
}

fn kill_and_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn parse_program_output(line: &str) -> std::result::Result<ProgramOutput, String> {
    let output = serde_json::from_str::<ProgramOutput>(line)
        .map_err(|error| format!("invalid usability probe JSON Lines output: {error}"))?;
    match &output {
        ProgramOutput::NodeResult { node, detail, .. } => {
            if node.trim().is_empty()
                || node.chars().count() > 256
                || node.chars().any(char::is_control)
            {
                return Err(
                    "usability probe node names must contain 1 to 256 printable characters"
                        .to_string(),
                );
            }
            if detail.as_ref().is_some_and(|detail| detail.contains('\0')) {
                return Err("usability probe detail must not contain NUL bytes".to_string());
            }
        }
        ProgramOutput::Summary { message, .. } => {
            if message
                .as_ref()
                .is_some_and(|message| message.contains('\0'))
            {
                return Err("usability probe summary must not contain NUL bytes".to_string());
            }
        }
    }
    Ok(output)
}

fn read_protocol_lines(
    stdout: impl Read,
    sender: mpsc::SyncSender<std::result::Result<String, String>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut protocol_failed = false;
    let mut total_bytes = 0usize;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) => {
                let _ = sender.send(Err(format!(
                    "failed to read usability probe stdout: {error}"
                )));
                return;
            }
        };
        if available.is_empty() {
            if !protocol_failed && !line.is_empty() {
                send_protocol_line(&sender, &line);
            }
            return;
        }
        let chunk_len = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        total_bytes = total_bytes.saturating_add(chunk_len);
        if total_bytes > MAX_PROTOCOL_TOTAL_BYTES && !protocol_failed {
            let _ = sender.send(Err(format!(
                "usability probe stdout exceeds {MAX_PROTOCOL_TOTAL_BYTES} bytes"
            )));
            protocol_failed = true;
            line.clear();
        }
        if !protocol_failed {
            let content_len = if available.get(chunk_len.saturating_sub(1)) == Some(&b'\n') {
                chunk_len - 1
            } else {
                chunk_len
            };
            if line.len().saturating_add(content_len) > MAX_PROTOCOL_LINE_BYTES {
                let _ = sender.send(Err(format!(
                    "usability probe output line exceeds {MAX_PROTOCOL_LINE_BYTES} bytes"
                )));
                protocol_failed = true;
                line.clear();
            } else {
                line.extend_from_slice(&available[..content_len]);
                if content_len + 1 == chunk_len {
                    send_protocol_line(&sender, &line);
                    line.clear();
                }
            }
        }
        reader.consume(chunk_len);
    }
}

fn send_protocol_line(
    sender: &mpsc::SyncSender<std::result::Result<String, String>>,
    bytes: &[u8],
) {
    match std::str::from_utf8(bytes) {
        Ok(line) if line.trim().is_empty() => {}
        Ok(line) => {
            let _ = sender.send(Ok(line.trim_end_matches('\r').to_string()));
        }
        Err(error) => {
            let _ = sender.send(Err(format!(
                "usability probe stdout is not valid UTF-8: {error}"
            )));
        }
    }
}

fn read_bounded_stderr(mut stderr: impl Read) -> String {
    let mut output = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let remaining = MAX_STDERR_BYTES.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
        // Continue draining after the retained prefix is full so a verbose child cannot block on
        // its stderr pipe while the TUI still keeps diagnostic memory strictly bounded.
    }
    bounded_diagnostic(String::from_utf8_lossy(&output).trim().to_string())
}

fn combine_program_diagnostic(fatal: Option<String>, stderr: String) -> Option<String> {
    match (fatal, stderr.is_empty()) {
        (None, true) => None,
        (None, false) => Some(stderr),
        (Some(fatal), true) => Some(bounded_diagnostic(fatal)),
        (Some(fatal), false) => Some(bounded_diagnostic(format!("{fatal}; stderr: {stderr}"))),
    }
}

fn send_finished(
    sender: &mpsc::Sender<UsabilityProbeJobEvent>,
    complete: bool,
    summary: Option<String>,
    diagnostic: Option<String>,
    results: BTreeMap<String, UsabilityProbeNodeResult>,
) {
    let summary = summary.map(bounded_result_detail);
    let diagnostic = diagnostic.map(bounded_diagnostic);
    let results = results
        .into_values()
        .map(|mut result| {
            result.detail = result.detail.map(bounded_result_detail);
            result
        })
        .collect();
    let _ = sender.send(UsabilityProbeJobEvent::Finished(
        UsabilityProbeRunCompletion {
            complete,
            summary,
            diagnostic,
            results,
        },
    ));
}

fn bounded_result_detail(value: String) -> String {
    usability_presentation_text(&value)
}

pub(crate) fn usability_presentation_text(value: &str) -> String {
    printable_single_line(value, MAX_RESULT_DETAIL_CHARS)
}

fn bounded_diagnostic(value: String) -> String {
    printable_single_line(value, MAX_STDERR_BYTES)
}

fn printable_single_line(value: impl AsRef<str>, max_chars: usize) -> String {
    value
        .as_ref()
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            character if character.is_control() => '\u{fffd}',
            character => character,
        })
        .take(max_chars)
        .collect()
}

fn manifest_from_document(
    document: ManifestDocument,
    path: &Path,
) -> Result<UsabilityProbeManifest> {
    validate_manifest_id(&document.id)?;
    validate_manifest_label(&document.label)?;
    let source = match (document.url, document.executable, document.args) {
        (Some(url), None, None) => {
            let parsed = reqwest::Url::parse(url.trim())
                .with_context(|| format!("manifest '{}' has an invalid URL", document.id))?;
            if parsed.scheme() != "https" {
                bail!(
                    "manifest '{}' URL must use https; current sing-box Delay endpoints silently replace plain-http targets",
                    document.id
                );
            }
            if !parsed.username().is_empty() || parsed.password().is_some() {
                bail!(
                    "manifest '{}' URL must not contain credentials",
                    document.id
                );
            }
            UsabilityProbeSource::Url(parsed.to_string())
        }
        (None, Some(executable), Some(args)) => {
            validate_executable(&document.id, &executable, &args)?;
            let executable = PathBuf::from(executable.trim());
            let executable = if executable.is_absolute() {
                executable
            } else {
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .join(executable)
            };
            UsabilityProbeSource::Executable { executable, args }
        }
        (Some(_), Some(_), _) => bail!(
            "manifest '{}' must declare exactly one of url or executable",
            document.id
        ),
        (Some(_), None, Some(_)) => {
            bail!("manifest '{}' URL form must not declare args", document.id)
        }
        (None, Some(_), None) => bail!(
            "manifest '{}' executable form requires an args array",
            document.id
        ),
        (None, None, _) => bail!(
            "manifest '{}' must declare exactly one of url or executable",
            document.id
        ),
    };
    Ok(UsabilityProbeManifest {
        id: NodeViewId::new(document.id)
            .expect("validated manifest IDs are valid node-view identities"),
        label: document.label.trim().to_string(),
        ranking_policy: document.ranking,
        source,
        source_path: path.to_path_buf(),
    })
}

fn validate_manifest_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        bail!("manifest id must contain 1 to 64 ASCII characters");
    }
    if !id.as_bytes()[0].is_ascii_lowercase() && !id.as_bytes()[0].is_ascii_digit() {
        bail!("manifest id must start with a lowercase ASCII letter or digit");
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte))
    {
        bail!("manifest id may contain only lowercase ASCII letters, digits, '.', '_' and '-'");
    }
    if matches!(id, "current-selector" | "streaming") {
        bail!("manifest id '{id}' is reserved for a built-in node view");
    }
    Ok(())
}

fn validate_manifest_label(label: &str) -> Result<()> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 64 {
        bail!("manifest label must contain 1 to 64 characters");
    }
    if label.chars().any(char::is_control) {
        bail!("manifest label must not contain control characters");
    }
    Ok(())
}

fn validate_executable(id: &str, executable: &str, args: &[String]) -> Result<()> {
    if executable.trim().is_empty() || executable.chars().any(char::is_control) {
        bail!("manifest '{id}' executable must be a non-empty path without control characters");
    }
    if args.len() > 256 {
        bail!("manifest '{id}' args array is limited to 256 entries");
    }
    let mut total = 0usize;
    for argument in args {
        if argument.contains('\0') {
            bail!("manifest '{id}' args must not contain NUL bytes");
        }
        total = total.saturating_add(argument.len());
    }
    if total > 32 * 1024 {
        bail!("manifest '{id}' args exceed the 32768-byte limit");
    }
    Ok(())
}

fn push_diagnostic(discovery: &mut UsabilityProbeDiscovery, path: &Path, message: String) {
    if discovery.diagnostics.len() >= MAX_DISCOVERY_DIAGNOSTICS {
        return;
    }
    discovery
        .diagnostics
        .push(manifest_diagnostic(path, message));
}

pub(crate) fn manifest_diagnostic(path: &Path, message: impl AsRef<str>) -> ManifestDiagnostic {
    ManifestDiagnostic {
        path: printable_single_line(path.to_string_lossy(), MAX_DIAGNOSTIC_CHARS),
        message: printable_single_line(message, MAX_DIAGNOSTIC_CHARS),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, Write as _};
    use std::net::TcpListener;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("sing-box-tui-usability-{label}-{nanos}-{counter}"));
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    #[test]
    fn discovers_stable_url_and_executable_manifests_without_running_them() {
        let directory = temp_dir("discover");
        let marker = directory.join("must-not-exist");
        fs::write(
            directory.join("01-url.json"),
            r#"{
                "id":"github-web",
                "label":"GitHub Web",
                "ranking":"low-latency",
                "url":"https://github.com/404-is-still-http"
            }"#,
        )
        .expect("write URL manifest");
        fs::write(
            directory.join("02-program.json"),
            format!(
                r#"{{
                    "id":"agy-gemini",
                    "label":"Agy Gemini",
                    "ranking":"balanced",
                    "executable":"fixture-probe",
                    "args":["--marker","{}"]
                }}"#,
                marker.display()
            ),
        )
        .expect("write executable manifest");

        let discovered =
            discover_usability_probe_manifests(&directory).expect("discover manifests");
        assert!(discovered.diagnostics.is_empty());
        assert_eq!(
            discovered
                .manifests
                .iter()
                .map(|manifest| manifest.id.as_str())
                .collect::<Vec<_>>(),
            ["github-web", "agy-gemini"]
        );
        assert!(matches!(
            &discovered.manifests[0].source,
            UsabilityProbeSource::Url(url) if url.starts_with("https://github.com/")
        ));
        assert!(matches!(
            &discovered.manifests[1].source,
            UsabilityProbeSource::Executable { executable, args }
                if executable == &directory.join("fixture-probe") && args.len() == 2
        ));
        assert!(
            !marker.exists(),
            "manifest discovery must never execute a program"
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn invalid_manifests_produce_bounded_actionable_diagnostics() {
        let directory = temp_dir("invalid");
        for index in 0..12 {
            fs::write(
                directory.join(format!("{index:02}.json")),
                format!(
                    r#"{{"id":"Bad {index}","label":"Bad","ranking":"throughput","url":"ftp://example.test"}}"#
                ),
            )
            .expect("write invalid manifest");
        }
        let discovered =
            discover_usability_probe_manifests(&directory).expect("discover manifests");
        assert!(discovered.manifests.is_empty());
        assert_eq!(discovered.diagnostics.len(), MAX_DISCOVERY_DIAGNOSTICS);
        assert!(discovered.diagnostics.iter().all(|diagnostic| {
            diagnostic.path.ends_with(".json")
                && diagnostic.message.contains("manifest id")
                && diagnostic.message.chars().count() <= MAX_DIAGNOSTIC_CHARS
                && !diagnostic.path.chars().any(char::is_control)
                && !diagnostic.message.chars().any(char::is_control)
        }));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn manifest_diagnostics_normalize_path_and_reason_for_terminal_display() {
        let diagnostic = manifest_diagnostic(
            Path::new("invalid\n\u{1b}[31m.json"),
            "bad\tmanifest\r\nreason\u{1b}[0m",
        );

        assert!(!diagnostic.path.chars().any(char::is_control));
        assert!(!diagnostic.message.chars().any(char::is_control));
        assert!(diagnostic.path.ends_with("[31m.json"));
        assert!(diagnostic.message.contains("bad manifest  reason"));
    }

    #[test]
    fn discovery_bounds_json_candidates_before_parsing_them() {
        let directory = temp_dir("candidate-bound");
        for index in 0..80 {
            fs::write(
                directory.join(format!("{index:03}.json")),
                format!(
                    r#"{{
                        "id":"probe-{index:03}",
                        "label":"Probe {index:03}",
                        "ranking":"balanced",
                        "url":"https://example.test/{index}"
                    }}"#
                ),
            )
            .expect("write bounded manifest fixture");
        }

        let discovered =
            discover_usability_probe_manifests(&directory).expect("discover bounded manifests");
        assert_eq!(discovered.manifests.len(), MAX_DISCOVERED_MANIFESTS);
        assert_eq!(
            discovered.manifests.first().unwrap().id.as_str(),
            "probe-000"
        );
        assert_eq!(
            discovered.manifests.last().unwrap().id.as_str(),
            "probe-063"
        );
        assert!(
            discovered
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("extra files were ignored"))
        );
        assert!(discovered.diagnostics.len() <= MAX_DISCOVERY_DIAGNOSTICS);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn manifest_requires_exactly_one_well_formed_source() {
        let directory = temp_dir("source");
        let path = directory.join("criterion.json");
        fs::write(
            &path,
            r#"{
                "id":"criterion",
                "label":"Criterion",
                "ranking":"balanced",
                "url":"https://example.test/",
                "executable":"probe",
                "args":[]
            }"#,
        )
        .expect("write manifest");
        let error = load_usability_probe_manifest(&path).expect_err("two sources must fail");
        assert!(format!("{error:#}").contains("exactly one"));

        fs::write(
            &path,
            r#"{
                "id":"criterion",
                "label":"Criterion",
                "ranking":"balanced",
                "executable":"probe"
            }"#,
        )
        .expect("rewrite manifest");
        let error = load_usability_probe_manifest(&path).expect_err("args are required");
        assert!(format!("{error:#}").contains("requires an args array"));

        fs::write(
            &path,
            r#"{
                "id":"criterion",
                "label":"Criterion",
                "ranking":"balanced",
                "url":"http://example.test/"
            }"#,
        )
        .expect("rewrite plain HTTP manifest");
        let error = load_usability_probe_manifest(&path)
            .expect_err("plain HTTP would make sing-box test its default URL");
        assert!(format!("{error:#}").contains("must use https"));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn executable_fixture_streams_progress_and_publishes_only_selector_intersection() {
        let directory = temp_dir("program-fixture");
        let executable = compile_program_fixture(&directory);
        let shell_marker = directory.join("shell-must-not-run");
        let manifest = UsabilityProbeManifest {
            id: NodeViewId::new("fixture").unwrap(),
            label: "Fixture".to_string(),
            ranking_policy: RankingPolicy::Balanced,
            source: UsabilityProbeSource::Executable {
                executable,
                args: vec![format!(
                    "literal argument; touch {}",
                    shell_marker.display()
                )],
            },
            source_path: directory.join("fixture.json"),
        };
        let client = AsyncClient::builder()
            .no_proxy()
            .build()
            .expect("HTTP client");
        let mut job = spawn_usability_probe_job(
            manifest,
            vec!["node-a".to_string(), "node-b".to_string()],
            "http://127.0.0.1:1".to_string(),
            client,
        )
        .expect("spawn fixture");
        let (progress, completion) = collect_job(&job);
        job.join();

        assert_eq!(
            progress
                .iter()
                .map(|result| (result.node.as_str(), result.usable))
                .collect::<Vec<_>>(),
            [("node-a", true), ("node-b", false)]
        );
        assert!(completion.complete);
        assert_eq!(completion.results.len(), 2);
        assert_eq!(completion.summary.as_deref(), Some("fixture complete"));
        assert_eq!(
            completion.diagnostic.as_deref().map(str::len),
            Some(MAX_STDERR_BYTES)
        );
        assert!(
            !shell_marker.exists(),
            "argument metacharacters must never be interpreted by a shell"
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn cancellation_joins_after_more_than_sixty_four_unconsumed_progress_events() {
        let directory = temp_dir("cancel-progress-flood");
        let executable = compile_program_fixture(&directory);
        let ready = directory.join("all-output-written");
        let manifest = UsabilityProbeManifest {
            id: NodeViewId::new("progress-flood").unwrap(),
            label: "Progress Flood".to_string(),
            ranking_policy: RankingPolicy::Balanced,
            source: UsabilityProbeSource::Executable {
                executable,
                args: vec!["--flood".to_string(), ready.display().to_string()],
            },
            source_path: directory.join("progress-flood.json"),
        };
        let selector_members = (0..128).map(|index| format!("node-{index}")).collect();
        let client = AsyncClient::builder()
            .no_proxy()
            .build()
            .expect("HTTP client");
        let mut job = spawn_usability_probe_job(
            manifest,
            selector_members,
            "http://127.0.0.1:1".to_string(),
            client,
        )
        .expect("spawn flood fixture");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready.exists(),
            "fixture must write all progress before cancellation"
        );

        // Intentionally do not consume `job.receiver`: the shutdown path cancels and joins in
        // exactly this state. More than the old 64-event capacity makes the regression
        // deterministic instead of depending on UI polling timing.
        let (joined_sender, joined_receiver) = mpsc::channel();
        thread::spawn(move || {
            job.cancel();
            job.join();
            let _ = joined_sender.send(());
        });
        assert!(
            joined_receiver.recv_timeout(Duration::from_secs(2)).is_ok(),
            "cancellation must not deadlock behind undrained progress events"
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn executable_output_is_single_line_printable_before_presentation() {
        let directory = temp_dir("program-controls");
        let manifest = UsabilityProbeManifest {
            id: NodeViewId::new("fixture-controls").unwrap(),
            label: "Fixture Controls".to_string(),
            ranking_policy: RankingPolicy::Balanced,
            source: UsabilityProbeSource::Executable {
                executable: compile_program_fixture(&directory),
                args: vec!["--controls".to_string()],
            },
            source_path: directory.join("fixture-controls.json"),
        };
        let client = AsyncClient::builder()
            .no_proxy()
            .build()
            .expect("HTTP client");
        let mut job = spawn_usability_probe_job(
            manifest,
            vec!["node-a".to_string()],
            "http://127.0.0.1:1".to_string(),
            client,
        )
        .expect("spawn controls fixture");
        let (progress, completion) = collect_job(&job);
        job.join();

        let detail = progress[0].detail.as_deref().expect("fixture detail");
        let summary = completion.summary.as_deref().expect("fixture summary");
        let diagnostic = completion.diagnostic.as_deref().expect("fixture stderr");
        for value in [detail, summary, diagnostic] {
            assert!(
                !value.chars().any(char::is_control),
                "terminal-facing text must be printable: {value:?}"
            );
        }
        assert!(detail.contains("detail   next\u{fffd}[31m"));
        assert!(summary.contains("summary  value\u{fffd}[0m"));
        assert!(diagnostic.contains("\u{fffd}[33m stderr value"));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn url_fixture_uses_live_delay_endpoint_for_each_named_outbound() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind controller fixture");
        let address = listener.local_addr().expect("fixture address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let worker_requests = Arc::clone(&requests);
        let worker = thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let mut stream = stream.expect("accept fixture request");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request_line = String::new();
                reader
                    .read_line(&mut request_line)
                    .expect("read request line");
                worker_requests
                    .lock()
                    .expect("request lock")
                    .push(request_line);
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).expect("read header");
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                }
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"delay\":42}",
                    )
                    .expect("write fixture response");
            }
        });
        let manifest = UsabilityProbeManifest {
            id: NodeViewId::new("web").unwrap(),
            label: "Web".to_string(),
            ranking_policy: RankingPolicy::LowLatency,
            source: UsabilityProbeSource::Url("https://example.test/any-status".to_string()),
            source_path: PathBuf::from("web.json"),
        };
        let client = AsyncClient::builder()
            .no_proxy()
            .build()
            .expect("HTTP client");
        let mut job = spawn_usability_probe_job(
            manifest,
            vec!["node-a".to_string(), "node-b".to_string()],
            format!("http://{address}"),
            client,
        )
        .expect("spawn URL probe");
        let (progress, completion) = collect_job(&job);
        job.join();
        worker.join().expect("controller fixture exits");

        assert!(completion.complete);
        assert_eq!(progress.len(), 2);
        assert!(progress.iter().all(|result| result.usable));
        let requests = requests.lock().expect("request lock");
        assert!(
            requests
                .iter()
                .any(|line| line.contains("/proxies/node-a/delay?"))
        );
        assert!(
            requests
                .iter()
                .any(|line| line.contains("/proxies/node-b/delay?"))
        );
        assert!(
            requests
                .iter()
                .all(|line| line.contains("url=https%3A%2F%2Fexample.test%2Fany-status"))
        );
    }

    #[test]
    fn url_fixture_treats_delay_endpoint_503_and_504_as_node_unusable() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind controller fixture");
        let address = listener.local_addr().expect("fixture address");
        let worker = thread::spawn(move || {
            for (stream, status) in listener
                .incoming()
                .take(2)
                .zip(["503 Service Unavailable", "504 Gateway Timeout"])
            {
                let mut stream = stream.expect("accept fixture request");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read request line");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .expect("write target failure response");
            }
        });
        let manifest = UsabilityProbeManifest {
            id: NodeViewId::new("web-failures").unwrap(),
            label: "Web Failures".to_string(),
            ranking_policy: RankingPolicy::Balanced,
            source: UsabilityProbeSource::Url("https://example.test/".to_string()),
            source_path: PathBuf::from("web-failures.json"),
        };
        let client = AsyncClient::builder()
            .no_proxy()
            .build()
            .expect("HTTP client");
        let mut job = spawn_usability_probe_job(
            manifest,
            vec!["node-a".to_string(), "node-b".to_string()],
            format!("http://{address}"),
            client,
        )
        .expect("spawn URL probe");
        let (progress, completion) = collect_job(&job);
        job.join();
        worker.join().expect("controller fixture exits");

        assert!(completion.complete);
        assert_eq!(progress.len(), 2);
        assert!(progress.iter().all(|result| !result.usable));
        assert!(completion.results.iter().all(|result| !result.usable));
    }

    #[test]
    fn stalled_controller_is_incomplete_instead_of_publishing_node_rejection() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind stalled controller");
        let address = listener.local_addr().expect("stalled controller address");
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accept stalled request");
            thread::sleep(Duration::from_millis(200));
        });
        let client = AsyncClient::builder()
            .no_proxy()
            .build()
            .expect("HTTP client");
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_url_probe_with_timeouts(
                vec!["node-a".to_string()],
                "https://example.test/".to_string(),
                format!("http://{address}"),
                client,
                sender,
                Arc::new(AtomicBool::new(false)),
                UrlProbeTimeouts {
                    target: Duration::from_millis(20),
                    controller: Duration::from_millis(50),
                },
            );
        });
        let mut progress = Vec::new();
        let completion = loop {
            match receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("stalled controller probe must terminate")
            {
                UsabilityProbeJobEvent::Progress(result) => progress.push(result),
                UsabilityProbeJobEvent::Finished(completion) => break completion,
            }
        };
        worker.join().expect("URL probe worker exits");
        server.join().expect("stalled controller fixture exits");

        assert!(progress.is_empty());
        assert!(!completion.complete);
        assert!(completion.results.is_empty());
        assert!(
            completion
                .diagnostic
                .as_deref()
                .is_some_and(|value| value.contains("Clash controller did not answer"))
        );

        let directory = temp_dir("stalled-controller-preserves-prior");
        let database = directory.join("quality.sqlite3");
        let store = crate::storage::BenchmarkStore::open(&database).expect("open quality store");
        store
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [
                    {"type":"selector", "tag":"select", "outbounds":["node-a"]},
                    {"type":"direct", "tag":"node-a"}
                ]
            }))
            .expect("bind stalled-controller fixture identity");
        let (prior_run, generation) = store
            .begin_usability_probe_run("web", "select", store.quality_generation())
            .expect("begin prior run")
            .expect("quality generation is current");
        assert!(
            store
                .finish_usability_probe_run(
                    prior_run,
                    generation,
                    true,
                    Some("prior complete"),
                    None,
                    &[crate::storage::UsabilityProbeFactRecord {
                        node: "node-a".to_string(),
                        usable: true,
                        detail: Some("previously accepted".to_string()),
                    }],
                )
                .expect("publish prior projection")
        );
        let (stalled_run, stalled_generation) = store
            .begin_usability_probe_run("web", "select", store.quality_generation())
            .expect("begin stalled run")
            .expect("quality generation remains current");
        assert!(
            !store
                .finish_usability_probe_run(
                    stalled_run,
                    stalled_generation,
                    completion.complete,
                    completion.summary.as_deref(),
                    completion.diagnostic.as_deref(),
                    &[],
                )
                .expect("finalize stalled controller as incomplete")
        );
        let preserved = store
            .latest_usability_probe_run("web", "select", &["node-a".to_string()])
            .expect("read preserved projection")
            .expect("prior complete projection remains visible");
        assert_eq!(preserved.run_id, prior_run);
        assert!(preserved.results[0].usable);
        drop(store);
        fs::remove_dir_all(directory).expect("remove stalled-controller fixture database");
    }

    fn collect_job(
        job: &UsabilityProbeJob,
    ) -> (Vec<UsabilityProbeNodeResult>, UsabilityProbeRunCompletion) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut progress = Vec::new();
        while std::time::Instant::now() < deadline {
            match job.try_recv() {
                Some(UsabilityProbeJobEvent::Progress(result)) => progress.push(result),
                Some(UsabilityProbeJobEvent::Finished(completion)) => {
                    return (progress, completion);
                }
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        panic!("usability probe fixture did not finish before its deadline")
    }

    fn compile_program_fixture(directory: &Path) -> PathBuf {
        let source = directory.join("fixture.rs");
        fs::write(
            &source,
            r##"
                fn main() {
                    let args = std::env::args().collect::<Vec<_>>();
                    if args.get(1).map(String::as_str) == Some("--flood") {
                        if args.len() != 3 {
                            std::process::exit(8);
                        }
                        for index in 0..128 {
                            println!(
                                "{{\"type\":\"node_result\",\"node\":\"node-{index}\",\"usable\":true}}"
                            );
                        }
                        std::fs::write(&args[2], "ready").unwrap();
                        std::thread::sleep(std::time::Duration::from_secs(30));
                        return;
                    }
                    if args.get(1).map(String::as_str) == Some("--controls") {
                        println!("{}", r#"{"type":"node_result","node":"node-a","usable":true,"detail":"detail\n\t next\u001b[31m"}"#);
                        println!("{}", r#"{"type":"summary","complete":true,"message":"summary\r\nvalue\u001b[0m"}"#);
                        eprint!("\u{1b}[33m\nstderr\tvalue");
                        return;
                    }
                    if args.len() != 2 {
                        std::process::exit(9);
                    }
                    println!("{}", r#"{"type":"node_result","node":"node-a","usable":true,"detail":"first"}"#);
                    println!("{}", r#"{"type":"node_result","node":"outside-selector","usable":true}"#);
                    println!("{}", r#"{"type":"node_result","node":"node-b","usable":false,"detail":"application rejected"}"#);
                    println!("{}", r#"{"type":"summary","complete":true,"message":"fixture complete"}"#);
                    eprint!("{}", "x".repeat(20 * 1024));
                }
            "##,
        )
        .expect("write fixture source");
        let executable = directory.join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = Command::new(rustc)
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .status()
            .expect("start rustc for fixture");
        assert!(status.success(), "fixture compilation must succeed");
        executable
    }
}

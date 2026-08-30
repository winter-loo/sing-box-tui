use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, COOKIE, HeaderMap, HeaderValue, REFERER, USER_AGENT,
};
use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};

use crate::node_runtime_manager::{IsolatedNodeProxy, IsolatedRuntimeSnapshot};

pub(crate) const SUSTAINED_EXPECTED_BYTES: u64 = 512 * 1024;
pub(crate) const SUSTAINED_MAX_REDIRECTS: usize = 2;
pub(crate) const SUSTAINED_TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const SUSTAINED_MAX_CONCURRENCY: usize = 2;
pub(crate) const DEFAULT_SUSTAINED_TARGET_URL: &str =
    "https://speed.cloudflare.com/__down?bytes=524288";

#[derive(Clone, Debug)]
pub(crate) struct SustainedProbeRequest {
    pub(crate) selector: String,
    pub(crate) nodes: Vec<String>,
    pub(crate) target_url: String,
    pub(crate) config_path: PathBuf,
    pub(crate) sing_box_executable: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NodeSustainedQuality {
    pub(crate) name: String,
    pub(crate) outcome: SustainedProbeOutcome,
}

impl NodeSustainedQuality {
    pub(crate) fn completed(&self) -> Option<&SustainedCompletion> {
        match &self.outcome {
            SustainedProbeOutcome::Completed(completion) => Some(completion),
            _ => None,
        }
    }

    pub(crate) fn compact_evidence(&self) -> String {
        match &self.outcome {
            SustainedProbeOutcome::Completed(completion) => format!(
                "{:.1} MiB/s · first byte {}ms",
                completion.throughput_bytes_per_second as f64 / (1024.0 * 1024.0),
                completion.first_byte_ms
            ),
            SustainedProbeOutcome::TransferFailed { .. } => "sustained failed".to_string(),
            SustainedProbeOutcome::RuntimeFailed { .. } => "runtime failed".to_string(),
            SustainedProbeOutcome::Cancelled => "sustained cancelled".to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SustainedProbeOutcome {
    Completed(SustainedCompletion),
    TransferFailed { detail: String },
    RuntimeFailed { detail: String },
    Cancelled,
}

impl SustainedProbeOutcome {
    pub(crate) fn storage_kind(&self) -> &'static str {
        match self {
            Self::Completed(_) => "completed",
            Self::TransferFailed { .. } => "transfer_failed",
            Self::RuntimeFailed { .. } => "runtime_failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn is_node_attributable(&self) -> bool {
        matches!(self, Self::Completed(_) | Self::TransferFailed { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SustainedCompletion {
    pub(crate) first_byte_ms: u64,
    pub(crate) completion_ms: u64,
    pub(crate) bytes_read: u64,
    pub(crate) throughput_bytes_per_second: u64,
}

impl SustainedCompletion {
    pub(crate) fn from_facts(
        first_byte_ms: u64,
        completion_ms: u64,
        bytes_read: u64,
    ) -> Result<Self> {
        if bytes_read != SUSTAINED_EXPECTED_BYTES {
            bail!("completed sustained facts must contain exactly 512 KiB");
        }
        if completion_ms < first_byte_ms {
            bail!("sustained completion precedes first byte");
        }
        let transfer_ms = completion_ms.saturating_sub(first_byte_ms).max(1);
        Ok(Self {
            first_byte_ms,
            completion_ms,
            bytes_read,
            throughput_bytes_per_second: bytes_read.saturating_mul(1_000) / transfer_ms,
        })
    }
}

pub(crate) enum SustainedProbeEvent {
    Progress(NodeSustainedQuality),
    Finished,
}

#[derive(Clone)]
struct ValidatedSustainedTarget {
    url: String,
}

impl ValidatedSustainedTarget {
    fn parse(value: &str) -> Result<Self> {
        let url = reqwest::Url::parse(value).context("sustained target must be a valid URL")?;
        validate_account_free_https_url(&url)?;
        Ok(Self {
            url: url.to_string(),
        })
    }
}

fn validate_account_free_https_url(url: &reqwest::Url) -> Result<()> {
    if url.scheme() != "https" {
        bail!("sustained target must use HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        bail!("sustained target must not contain account material");
    }
    let query = url.query_pairs().collect::<Vec<_>>();
    if !query.is_empty() && (query.len() != 1 || query[0].0 != "bytes" || query[0].1 != "524288") {
        bail!("sustained target query is limited to bytes=524288");
    }
    Ok(())
}

pub(crate) fn validate_sustained_target(value: &str) -> Result<()> {
    ValidatedSustainedTarget::parse(value).map(|_| ())
}

pub(crate) fn normalize_sustained_target(value: &str) -> Result<String> {
    ValidatedSustainedTarget::parse(value).map(|target| target.url)
}

/// Stable, non-reversible partition key for facts produced by one account-free target.
pub(crate) fn sustained_target_identity(value: &str) -> Result<String> {
    let target = ValidatedSustainedTarget::parse(value)?;
    let digest = Sha256::digest(target.url.as_bytes());
    let mut identity = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut identity, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(identity)
}

pub(crate) fn spawn_sustained_probe_worker(
    request: SustainedProbeRequest,
    runtime_snapshot: Option<IsolatedRuntimeSnapshot>,
    tx: mpsc::Sender<SustainedProbeEvent>,
    cancelled: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        #[cfg(not(test))]
        let runtime_snapshot = Some(
            runtime_snapshot.expect("production sustained jobs require an immutable snapshot"),
        );
        let target = match ValidatedSustainedTarget::parse(&request.target_url) {
            Ok(target) => target,
            Err(_) => {
                for name in request.nodes {
                    let _ = tx.send(SustainedProbeEvent::Progress(NodeSustainedQuality {
                        name,
                        outcome: SustainedProbeOutcome::RuntimeFailed {
                            detail: "invalid sustained target".to_string(),
                        },
                    }));
                }
                let _ = tx.send(SustainedProbeEvent::Finished);
                return;
            }
        };
        let pending = Arc::new(Mutex::new(VecDeque::from(request.nodes)));
        let worker_count = pending
            .lock()
            .expect("sustained queue mutex poisoned")
            .len()
            .min(SUSTAINED_MAX_CONCURRENCY);
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let pending = Arc::clone(&pending);
            let tx = tx.clone();
            let cancelled = Arc::clone(&cancelled);
            let target = target.clone();
            let runtime_snapshot = runtime_snapshot.clone();
            #[cfg(test)]
            let config_path = request.config_path.clone();
            #[cfg(test)]
            let executable = request.sing_box_executable.clone();
            workers.push(std::thread::spawn(move || {
                loop {
                    let Some(name) = pending
                        .lock()
                        .expect("sustained queue mutex poisoned")
                        .pop_front()
                    else {
                        break;
                    };
                    let result = if cancelled.load(Ordering::Relaxed) {
                        NodeSustainedQuality {
                            name,
                            outcome: SustainedProbeOutcome::Cancelled,
                        }
                    } else {
                        match sustained_gate().acquire_unless_cancelled(&cancelled) {
                            Some(_permit) if !cancelled.load(Ordering::Relaxed) => probe_one_node(
                                runtime_snapshot.as_ref(),
                                #[cfg(test)]
                                &config_path,
                                #[cfg(test)]
                                &executable,
                                name,
                                &target,
                                &cancelled,
                            ),
                            _ => NodeSustainedQuality {
                                name,
                                outcome: SustainedProbeOutcome::Cancelled,
                            },
                        }
                    };
                    if tx.send(SustainedProbeEvent::Progress(result)).is_err() {
                        break;
                    }
                }
            }));
        }
        for worker in workers {
            let _ = worker.join();
        }
        let _ = tx.send(SustainedProbeEvent::Finished);
    })
}

fn probe_one_node(
    runtime_snapshot: Option<&IsolatedRuntimeSnapshot>,
    #[cfg(test)] config_path: &std::path::Path,
    #[cfg(test)] sing_box_executable: &std::path::Path,
    name: String,
    target: &ValidatedSustainedTarget,
    cancelled: &AtomicBool,
) -> NodeSustainedQuality {
    let isolated = match runtime_snapshot {
        Some(snapshot) => IsolatedNodeProxy::start_from_snapshot(snapshot, &name),
        #[cfg(test)]
        None => IsolatedNodeProxy::start(config_path, sing_box_executable, &name),
        #[cfg(not(test))]
        None => unreachable!("production sustained job has no immutable runtime snapshot"),
    };
    let isolated = match isolated {
        Ok(isolated) => isolated,
        Err(_) => {
            return NodeSustainedQuality {
                name,
                outcome: SustainedProbeOutcome::RuntimeFailed {
                    detail: "isolated runtime unavailable".to_string(),
                },
            };
        }
    };
    // Runtime construction and direct node binding have their own startup bound. Sustained
    // timing begins only once the candidate-bound data channel is ready to carry the transfer.
    let started = Instant::now();
    if cancelled.load(Ordering::Relaxed) {
        return NodeSustainedQuality {
            name,
            outcome: SustainedProbeOutcome::Cancelled,
        };
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            return NodeSustainedQuality {
                name,
                outcome: SustainedProbeOutcome::RuntimeFailed {
                    detail: "sustained probe runtime unavailable".to_string(),
                },
            };
        }
    };
    let proxy_url = isolated.http_proxy_url();
    let outcome = runtime
        .block_on(async {
            tokio::select! {
                result = read_expected_payload(&proxy_url, target, started) => result,
                () = wait_for_cancellation(cancelled) => Ok(SustainedProbeOutcome::Cancelled),
            }
        })
        .unwrap_or_else(|_| SustainedProbeOutcome::TransferFailed {
            // Never surface the reqwest error chain: it may contain redirect or target URLs.
            detail: "sustained transfer failed".to_string(),
        });
    NodeSustainedQuality { name, outcome }
}

async fn wait_for_cancellation(cancelled: &AtomicBool) {
    while !cancelled.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn read_expected_payload(
    proxy_url: &str,
    target: &ValidatedSustainedTarget,
    started: Instant,
) -> Result<SustainedProbeOutcome> {
    let proxy = reqwest::Proxy::all(proxy_url).context("failed to configure isolated proxy")?;
    let redirect = Policy::custom(|attempt| {
        if validate_account_free_https_url(attempt.url()).is_err() {
            attempt.error("sustained redirect was not account-free HTTPS")
        } else if sustained_redirect_limit_exceeded(attempt.previous().len()) {
            attempt.error("sustained redirect limit exceeded")
        } else {
            attempt.follow()
        }
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .proxy(proxy)
        .redirect(redirect)
        .referer(false)
        .cookie_store(false)
        .timeout(SUSTAINED_TOTAL_TIMEOUT)
        .build()
        .context("failed to build sustained HTTP client")?;
    read_expected_payload_with_client(
        &client,
        &target.url,
        started,
        ResponseTransport::AccountFreeHttps,
    )
    .await
}

#[derive(Clone, Copy)]
enum ResponseTransport {
    AccountFreeHttps,
    #[cfg(test)]
    LoopbackHttp,
}

async fn read_expected_payload_with_client(
    client: &reqwest::Client,
    target_url: &str,
    started: Instant,
    transport: ResponseTransport,
) -> Result<SustainedProbeOutcome> {
    let headers = sustained_probe_headers();
    debug_assert!(!headers.contains_key(REFERER));
    debug_assert!(!headers.contains_key(COOKIE));
    debug_assert!(!headers.contains_key(AUTHORIZATION));
    let mut response = client
        .get(target_url)
        .headers(headers)
        .send()
        .await
        .context("sustained request failed")?;

    // `error_for_status` accepts redirects. A final 3xx must never become throughput evidence,
    // even if that response happens to carry an exact-size body.
    if !response.status().is_success() {
        bail!("sustained target did not return a final 2xx response");
    }
    match transport {
        ResponseTransport::AccountFreeHttps => {
            validate_account_free_https_url(response.url())
                .context("sustained response left account-free HTTPS")?;
        }
        #[cfg(test)]
        ResponseTransport::LoopbackHttp => {
            if response.url().scheme() != "http"
                || !matches!(response.url().host_str(), Some("127.0.0.1" | "::1"))
            {
                bail!("test sustained response left loopback HTTP");
            }
        }
    }
    if response
        .content_length()
        .is_some_and(|length| length != SUSTAINED_EXPECTED_BYTES)
    {
        bail!(
            "sustained target declared an unexpected body length (expected {} bytes)",
            SUSTAINED_EXPECTED_BYTES
        );
    }
    let mut transfer = TransferAccumulator::default();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed while reading sustained response body")?
    {
        transfer.observe(chunk.len() as u64, started.elapsed())?;
    }
    Ok(SustainedProbeOutcome::Completed(
        transfer.finish(started.elapsed())?,
    ))
}

/// `reqwest` includes the initial request URL in `Attempt::previous()`, so a chain with `n`
/// already-followed redirects has `n + 1` previous URLs while deciding whether to follow the next
/// one. This matches `Policy::limited` and permits exactly `SUSTAINED_MAX_REDIRECTS` redirects.
fn sustained_redirect_limit_exceeded(previous_url_count: usize) -> bool {
    previous_url_count > SUSTAINED_MAX_REDIRECTS
}

fn sustained_probe_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/octet-stream"));
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("sing-box-tui-sustained-probe/1"),
    );
    headers
}

#[derive(Default)]
struct TransferAccumulator {
    bytes_read: u64,
    first_byte_ms: Option<u64>,
}

impl TransferAccumulator {
    fn observe(&mut self, bytes: u64, elapsed: Duration) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        self.first_byte_ms
            .get_or_insert(elapsed.as_millis().min(u64::MAX as u128) as u64);
        self.bytes_read = self.bytes_read.saturating_add(bytes);
        if self.bytes_read > SUSTAINED_EXPECTED_BYTES {
            bail!(
                "sustained response exceeded {} bytes",
                SUSTAINED_EXPECTED_BYTES
            );
        }
        Ok(())
    }

    fn finish(self, elapsed: Duration) -> Result<SustainedCompletion> {
        if self.bytes_read != SUSTAINED_EXPECTED_BYTES {
            bail!(
                "sustained response ended at {} bytes; expected {}",
                self.bytes_read,
                SUSTAINED_EXPECTED_BYTES
            );
        }
        let first_byte_ms = self
            .first_byte_ms
            .context("sustained response contained no body")?;
        let completion_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
        SustainedCompletion::from_facts(first_byte_ms, completion_ms, self.bytes_read)
    }
}

struct ConcurrencyGate {
    limit: usize,
    active: Mutex<usize>,
    changed: Condvar,
}

impl ConcurrencyGate {
    fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            active: Mutex::new(0),
            changed: Condvar::new(),
        }
    }

    #[cfg(test)]
    fn acquire(&self) -> ConcurrencyPermit<'_> {
        let mut active = self.active.lock().expect("sustained gate mutex poisoned");
        while *active >= self.limit {
            active = self
                .changed
                .wait(active)
                .expect("sustained gate mutex poisoned");
        }
        *active += 1;
        ConcurrencyPermit { gate: self }
    }

    fn acquire_unless_cancelled(&self, cancelled: &AtomicBool) -> Option<ConcurrencyPermit<'_>> {
        let mut active = self.active.lock().expect("sustained gate mutex poisoned");
        while *active >= self.limit {
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            (active, _) = self
                .changed
                .wait_timeout(active, Duration::from_millis(50))
                .expect("sustained gate mutex poisoned");
        }
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        *active += 1;
        Some(ConcurrencyPermit { gate: self })
    }
}

struct ConcurrencyPermit<'a> {
    gate: &'a ConcurrencyGate,
}

impl Drop for ConcurrencyPermit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .gate
            .active
            .lock()
            .expect("sustained gate mutex poisoned");
        *active = active.saturating_sub(1);
        self.gate.changed.notify_one();
    }
}

fn sustained_gate() -> &'static ConcurrencyGate {
    static GATE: OnceLock<ConcurrencyGate> = OnceLock::new();
    GATE.get_or_init(|| ConcurrencyGate::new(SUSTAINED_MAX_CONCURRENCY))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex, MutexGuard, OnceLock};
    use std::thread;

    use super::*;

    #[derive(Clone, Debug)]
    struct RecordedRequest {
        path: String,
        header_names: Vec<String>,
    }

    struct LocalHttpFixture {
        _serial_guard: MutexGuard<'static, ()>,
        address: SocketAddr,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        stopped: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl LocalHttpFixture {
        fn start() -> Self {
            // The fixture pushes full-size bodies through real sockets. Serialize these boundary
            // tests so macOS ephemeral-port/socket scheduling cannot make unrelated fixtures reset
            // each other's short-lived connections under the parallel test harness.
            let serial_guard = local_http_fixture_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("fixture listener binds");
            listener
                .set_nonblocking(true)
                .expect("fixture listener becomes nonblocking");
            let address = listener.local_addr().expect("fixture has a local address");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stopped = Arc::new(AtomicBool::new(false));
            let thread_requests = Arc::clone(&requests);
            let thread_stopped = Arc::clone(&stopped);
            let worker = thread::spawn(move || {
                while !thread_stopped.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if thread_stopped.load(Ordering::Acquire) {
                                break;
                            }
                            let _ = serve_fixture_request(stream, &thread_requests);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                _serial_guard: serial_guard,
                address,
                requests,
                stopped,
                worker: Some(worker),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.address, path)
        }

        fn client_with_timeout(&self, timeout: Duration) -> reqwest::Client {
            let expected_port = self.address.port();
            let redirect = Policy::custom(move |attempt| {
                let url = attempt.url();
                let is_fixture_url = url.scheme() == "http"
                    && url.host_str() == Some("127.0.0.1")
                    && url.port_or_known_default() == Some(expected_port)
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none();
                if !is_fixture_url {
                    attempt.error("test redirect left the loopback fixture")
                } else if sustained_redirect_limit_exceeded(attempt.previous().len()) {
                    attempt.error("sustained redirect limit exceeded")
                } else {
                    attempt.follow()
                }
            });
            reqwest::Client::builder()
                .no_proxy()
                .redirect(redirect)
                .referer(false)
                .cookie_store(false)
                .timeout(timeout)
                .build()
                .expect("fixture client builds")
        }

        fn probe(&self, path: &str) -> Result<SustainedProbeOutcome> {
            self.probe_with_timeout(path, Duration::from_secs(5))
        }

        fn probe_with_timeout(
            &self,
            path: &str,
            timeout: Duration,
        ) -> Result<SustainedProbeOutcome> {
            let client = self.client_with_timeout(timeout);
            let url = self.url(path);
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fixture runtime builds")
                .block_on(read_expected_payload_with_client(
                    &client,
                    &url,
                    Instant::now(),
                    ResponseTransport::LoopbackHttp,
                ))
        }

        fn recorded_requests(&self) -> Vec<RecordedRequest> {
            self.requests
                .lock()
                .expect("fixture request mutex poisoned")
                .clone()
        }
    }

    fn local_http_fixture_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    impl Drop for LocalHttpFixture {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            // Wake the nonblocking accept loop so the fixture never outlives its test.
            let _ = TcpStream::connect(self.address);
            if let Some(worker) = self.worker.take() {
                worker.join().expect("fixture server joins");
            }
        }
    }

    fn serve_fixture_request(
        mut stream: TcpStream,
        requests: &Mutex<Vec<RecordedRequest>>,
    ) -> std::io::Result<()> {
        // Accepted sockets inherit O_NONBLOCK from this fixture's listener on macOS. Restore a
        // blocking stream so a large boundary body cannot be truncated by transient EAGAIN.
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_nodelay(true)?;
        let mut request_bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        while !request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            request_bytes.extend_from_slice(&buffer[..read]);
            if request_bytes.len() > 16 * 1024 {
                return Ok(());
            }
        }
        let request = String::from_utf8_lossy(&request_bytes);
        let mut lines = request.split("\r\n");
        let path = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        let header_names = lines
            .filter_map(|line| {
                line.split_once(':')
                    .map(|(name, _)| name.to_ascii_lowercase())
            })
            .collect::<Vec<_>>();
        requests
            .lock()
            .expect("fixture request mutex poisoned")
            .push(RecordedRequest {
                path: path.clone(),
                header_names,
            });

        match path.as_str() {
            "/exact" => write_body_response(
                &mut stream,
                "200 OK",
                SUSTAINED_EXPECTED_BYTES as usize,
                false,
            ),
            "/short" => write_body_response(
                &mut stream,
                "200 OK",
                SUSTAINED_EXPECTED_BYTES as usize - 1,
                false,
            ),
            "/overlong" => write_body_response(
                &mut stream,
                "200 OK",
                SUSTAINED_EXPECTED_BYTES as usize + 1,
                false,
            ),
            "/final-302" => write_body_response(
                &mut stream,
                "302 Found",
                SUSTAINED_EXPECTED_BYTES as usize,
                true,
            ),
            "/redirect/1" => write_redirect(&mut stream, "/exact"),
            "/redirect/2" => write_redirect(&mut stream, "/redirect/1"),
            "/redirect/3" => write_redirect(&mut stream, "/redirect/2"),
            "/stall" => {
                thread::sleep(Duration::from_millis(250));
                write_body_response(
                    &mut stream,
                    "200 OK",
                    SUSTAINED_EXPECTED_BYTES as usize,
                    true,
                )
            }
            _ => write_body_response(&mut stream, "404 Not Found", 0, true),
        }
    }

    fn write_redirect(stream: &mut TcpStream, location: &str) -> std::io::Result<()> {
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        stream.flush()
    }

    fn write_body_response(
        stream: &mut TcpStream,
        status: &str,
        body_len: usize,
        declare_length: bool,
    ) -> std::io::Result<()> {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n"
        )?;
        if declare_length {
            write!(stream, "Content-Length: {body_len}\r\n")?;
        }
        stream.write_all(b"\r\n")?;
        stream.write_all(&vec![b'x'; body_len])?;
        stream.flush()
    }

    #[test]
    fn target_is_https_and_contains_no_userinfo() {
        validate_sustained_target("https://example.test/payload").unwrap();
        validate_sustained_target("https://example.test/payload?bytes=524288").unwrap();
        assert!(validate_sustained_target("http://example.test/payload").is_err());
        assert!(validate_sustained_target("https://alice:secret@example.test/payload").is_err());
        assert!(validate_sustained_target("https://example.test/payload?token=private").is_err());
        assert!(validate_sustained_target("https://example.test/payload?API_KEY=private").is_err());
        assert!(
            validate_sustained_target("https://example.test/payload?X-Amz-Signature=private")
                .is_err()
        );
        assert!(validate_sustained_target("https://example.test/payload#private").is_err());
        assert!(validate_sustained_target("https://example.test/payload?bytes=524287").is_err());
        assert!(
            validate_sustained_target("https://example.test/payload?bytes=524288&x=1").is_err()
        );
        let error = validate_sustained_target("https://example.test/payload?jwt=private-value")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("private-value"));
    }

    #[test]
    fn target_identity_is_canonical_stable_and_does_not_expose_the_url() {
        let canonical = sustained_target_identity("https://EXAMPLE.test:443/payload").unwrap();
        let equivalent = sustained_target_identity("https://example.test/payload").unwrap();

        assert_eq!(canonical, equivalent);
        assert_eq!(canonical.len(), 64);
        assert!(!canonical.contains("example"));
    }

    #[test]
    fn sustained_request_construction_contains_no_account_headers() {
        let headers = sustained_probe_headers();
        assert!(!headers.contains_key(REFERER));
        assert!(!headers.contains_key(COOKIE));
        assert!(!headers.contains_key(AUTHORIZATION));
    }

    #[test]
    fn redirect_policy_permits_exactly_two_redirects() {
        // Before the first, second, and third candidate redirects, reqwest reports one, two, and
        // three previous URLs respectively (the first is the initial request).
        assert!(!sustained_redirect_limit_exceeded(0));
        assert!(!sustained_redirect_limit_exceeded(1));
        assert!(!sustained_redirect_limit_exceeded(2));
        assert!(sustained_redirect_limit_exceeded(3));
    }

    #[test]
    fn transfer_requires_exactly_the_expected_payload() {
        let mut exact = TransferAccumulator::default();
        exact
            .observe(256 * 1024, Duration::from_millis(10))
            .unwrap();
        exact
            .observe(256 * 1024, Duration::from_millis(110))
            .unwrap();
        let completion = exact.finish(Duration::from_millis(110)).unwrap();
        assert_eq!(completion.bytes_read, SUSTAINED_EXPECTED_BYTES);
        assert_eq!(completion.first_byte_ms, 10);
        assert_eq!(completion.completion_ms, 110);
        assert_eq!(completion.throughput_bytes_per_second, 5_242_880);

        let mut short = TransferAccumulator::default();
        short.observe(1, Duration::from_millis(1)).unwrap();
        assert!(short.finish(Duration::from_millis(2)).is_err());

        let mut oversized = TransferAccumulator::default();
        assert!(
            oversized
                .observe(SUSTAINED_EXPECTED_BYTES + 1, Duration::from_millis(1))
                .is_err()
        );
    }

    #[test]
    fn real_http_exact_512_kib_body_is_accepted() {
        let fixture = LocalHttpFixture::start();

        let outcome = fixture.probe("/exact").unwrap();

        let SustainedProbeOutcome::Completed(completion) = outcome else {
            panic!("exact fixture response should complete");
        };
        assert_eq!(completion.bytes_read, SUSTAINED_EXPECTED_BYTES);
    }

    #[test]
    fn real_http_redirect_cap_allows_two_and_rejects_three() {
        let fixture = LocalHttpFixture::start();

        assert!(matches!(
            fixture.probe("/redirect/2").unwrap(),
            SustainedProbeOutcome::Completed(_)
        ));
        assert!(fixture.probe("/redirect/3").is_err());

        let paths = fixture
            .recorded_requests()
            .into_iter()
            .map(|request| request.path)
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "/redirect/2",
                "/redirect/1",
                "/exact",
                "/redirect/3",
                "/redirect/2",
                "/redirect/1",
            ]
        );
    }

    #[test]
    fn real_http_final_redirect_with_exact_body_is_rejected() {
        let fixture = LocalHttpFixture::start();

        assert!(fixture.probe("/final-302").is_err());
    }

    #[test]
    fn real_http_short_and_overlong_bodies_are_rejected() {
        let fixture = LocalHttpFixture::start();

        assert!(fixture.probe("/short").is_err());
        assert!(fixture.probe("/overlong").is_err());
    }

    #[test]
    fn real_http_stalled_response_obeys_the_client_total_timeout() {
        let fixture = LocalHttpFixture::start();
        let started = Instant::now();

        fixture
            .probe_with_timeout("/stall", Duration::from_millis(50))
            .expect_err("stalled response must hit the total request timeout");

        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn real_http_requests_and_redirects_send_no_account_headers() {
        let fixture = LocalHttpFixture::start();

        assert!(matches!(
            fixture.probe("/redirect/2").unwrap(),
            SustainedProbeOutcome::Completed(_)
        ));

        let requests = fixture.recorded_requests();
        assert_eq!(requests.len(), 3);
        for request in requests {
            assert!(
                !request.header_names.iter().any(|name| {
                    matches!(name.as_str(), "authorization" | "cookie" | "referer")
                })
            );
        }
    }

    #[test]
    fn shared_gate_never_allows_more_than_two_sustained_probes() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(9));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            let start = Arc::clone(&start);
            workers.push(std::thread::spawn(move || {
                start.wait();
                // Exercise the process-wide gate used by every sustained worker, rather than a
                // test-local gate that could miss accidental per-job concurrency limits.
                let _permit = sustained_gate().acquire();
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        start.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        let observed = maximum.load(Ordering::SeqCst);
        // Other concurrently running tests share this process-wide gate and may occupy a slot;
        // the invariant under test is the hard upper bound, not scheduler-dependent saturation.
        assert!((1..=SUSTAINED_MAX_CONCURRENCY).contains(&observed));
    }
}

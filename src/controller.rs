use std::collections::BTreeMap;
use std::env;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client as AsyncClient;
use reqwest::Version;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use tokio::process::Command as TokioCommand;
use tokio::runtime::{Builder as TokioRuntimeBuilder, Runtime as TokioRuntime};
use tokio::task::JoinSet;
use urlencoding::encode;

use crate::defaults::DEFAULT_CONTROLLER;

pub(crate) fn run_selectors(options: SelectorsOptions) -> Result<()> {
    let client = build_api_client(options.controller)?;
    let groups = if let Some(selector) = options.selector {
        vec![client.fetch_selector_group(&selector)?]
    } else {
        client.fetch_selector_groups()?
    };

    let output = SelectorsOutput {
        groups: groups
            .into_iter()
            .map(|group| ProxyGroupOutput {
                name: group.name,
                current: group.current,
                members: group.members,
            })
            .collect(),
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

pub(crate) fn run_status(options: StatusOptions) -> Result<()> {
    let client = build_api_client(options.controller)?;
    let status = client.fetch_status()?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

pub(crate) fn run_benchmark(options: BenchmarkOptions) -> Result<()> {
    let client = build_api_client(options.controller)?;
    let summary = client.benchmark_selector(&BenchmarkRequest {
        selector: options.selector,
        pattern: options.pattern,
        url: options.url,
        timeout_ms: options.timeout_ms,
        request_timeout: options.request_timeout,
        max_concurrency: options.max_concurrency,
        nodes: None,
    })?;

    let mut final_node = summary.current.clone();
    let mut switched = false;
    if options.switch {
        if let Some(best) = summary.best_success() {
            client.switch_proxy(&summary.selector, &best.name)?;
            final_node = Some(best.name.clone());
            switched = true;
        }
    }

    let verification = if options.verify {
        Some(run_verification(options.verify_discord))
    } else {
        None
    };
    let best = summary.best_success().cloned();

    println!(
        "{}",
        serde_json::to_string_pretty(&BenchmarkOutput {
            selector: summary.selector,
            current: summary.current,
            pattern: summary.pattern,
            test_url: summary.url,
            timeout_ms: summary.timeout_ms,
            max_concurrency: summary.max_concurrency,
            results: summary.results,
            best,
            switched,
            final_node,
            verification,
        })?
    );
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct ProxyGroup {
    pub(crate) name: String,
    pub(crate) current: Option<String>,
    pub(crate) members: Vec<String>,
}

pub(crate) struct ApiClient {
    pub(crate) base_url: String,
    pub(crate) runtime: TokioRuntime,
    pub(crate) client: AsyncClient,
}

impl ApiClient {
    pub(crate) fn new(base_url: String, secret: Option<String>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        if let Some(secret) = secret {
            headers.insert(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {secret}"))
                    .context("invalid SING_BOX_SECRET header value")?,
            );
        }
        let runtime = TokioRuntimeBuilder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to build Tokio runtime for API client")?;
        let client = AsyncClient::builder()
            .default_headers(headers)
            .build()
            .context("failed to build async HTTP client")?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            runtime,
            client,
        })
    }

    pub(crate) fn fetch_selector_groups(&self) -> Result<Vec<ProxyGroup>> {
        self.runtime.block_on(self.fetch_selector_groups_async())
    }

    pub(crate) fn fetch_selector_group(&self, selector: &str) -> Result<ProxyGroup> {
        self.runtime
            .block_on(self.fetch_selector_group_async(selector))
    }

    async fn fetch_selector_groups_async(&self) -> Result<Vec<ProxyGroup>> {
        let payload: ProxiesResponse = self
            .client
            .get(format!("{}/proxies", self.base_url))
            .send()
            .await
            .context("failed to query Clash API /proxies")?
            .error_for_status()
            .context("Clash API /proxies returned an error")?
            .json()
            .await
            .context("failed to decode Clash API /proxies response")?;

        Ok(selectors_from_payload(payload))
    }

    async fn fetch_selector_group_async(&self, selector: &str) -> Result<ProxyGroup> {
        let proxy = self.fetch_selector_async(selector).await?;
        proxy_group_from_node(proxy).with_context(|| format!("{selector} is not a selector group"))
    }

    async fn fetch_selector_async(&self, selector: &str) -> Result<ProxyNode> {
        let encoded = encode(selector);
        self.client
            .get(format!("{}/proxies/{}", self.base_url, encoded))
            .send()
            .await
            .with_context(|| format!("failed to query Clash API selector {selector}"))?
            .error_for_status()
            .with_context(|| format!("Clash API rejected selector read for {selector}"))?
            .json()
            .await
            .context("failed to decode Clash API selector response")
    }

    pub(crate) fn benchmark_selector(
        &self,
        request: &BenchmarkRequest,
    ) -> Result<BenchmarkSummary> {
        self.runtime
            .block_on(self.benchmark_selector_async(request))
    }

    async fn benchmark_selector_async(
        &self,
        request: &BenchmarkRequest,
    ) -> Result<BenchmarkSummary> {
        let selector = self.fetch_selector_async(&request.selector).await?;
        let current = selector.now;
        let candidates = filter_benchmark_candidates(&selector.all, request);

        if candidates.is_empty() {
            return Ok(BenchmarkSummary {
                selector: request.selector.clone(),
                current,
                pattern: request.pattern.clone(),
                url: request.url.clone(),
                timeout_ms: request.timeout_ms,
                max_concurrency: request.max_concurrency,
                results: Vec::new(),
            });
        }

        let base_url = self.base_url.clone();
        let client = self.client.clone();
        let url = request.url.clone();
        let timeout_ms = request.timeout_ms;
        let request_timeout = request.request_timeout;

        let max_concurrency = request.max_concurrency.max(1);
        let mut results = {
            let mut tasks = JoinSet::new();
            let mut pending = candidates.into_iter();

            for _ in 0..max_concurrency {
                let Some(name) = pending.next() else {
                    break;
                };
                spawn_benchmark_task(
                    &mut tasks,
                    client.clone(),
                    base_url.clone(),
                    name,
                    url.clone(),
                    timeout_ms,
                    request_timeout,
                );
            }

            let mut results = Vec::new();
            while let Some(result) = tasks.join_next().await {
                results.push(result.expect("benchmark worker panicked"));
                if let Some(name) = pending.next() {
                    spawn_benchmark_task(
                        &mut tasks,
                        client.clone(),
                        base_url.clone(),
                        name,
                        url.clone(),
                        timeout_ms,
                        request_timeout,
                    );
                }
            }
            results
        };
        results.sort_by_key(|item| (item.delay.is_none(), item.delay.unwrap_or(u64::MAX)));

        Ok(BenchmarkSummary {
            selector: request.selector.clone(),
            current,
            pattern: request.pattern.clone(),
            url: request.url.clone(),
            timeout_ms: request.timeout_ms,
            max_concurrency,
            results,
        })
    }

    pub(crate) fn switch_proxy(&self, group: &str, proxy: &str) -> Result<()> {
        self.runtime.block_on(self.switch_proxy_async(group, proxy))
    }

    pub(crate) fn set_mode(&self, mode: &str) -> Result<()> {
        self.runtime.block_on(self.set_mode_async(mode))
    }

    pub(crate) fn fetch_status(&self) -> Result<StatusOutput> {
        self.runtime.block_on(self.fetch_status_async())
    }

    async fn switch_proxy_async(&self, group: &str, proxy: &str) -> Result<()> {
        let encoded_group = encode(group);
        self.client
            .put(format!("{}/proxies/{}", self.base_url, encoded_group))
            .json(&SwitchProxyRequest {
                name: proxy.to_string(),
            })
            .send()
            .await
            .with_context(|| format!("failed to send switch request for {group}"))?
            .error_for_status()
            .with_context(|| format!("controller rejected switch request for {group}"))?;
        Ok(())
    }

    async fn set_mode_async(&self, mode: &str) -> Result<()> {
        self.client
            .patch(format!("{}/configs", self.base_url))
            .json(&UpdateConfigRequest {
                mode: mode.to_string(),
            })
            .send()
            .await
            .context("failed to send Clash API mode update")?
            .error_for_status()
            .context("controller rejected Clash API mode update")?;
        Ok(())
    }

    async fn fetch_status_async(&self) -> Result<StatusOutput> {
        let version: VersionResponse = self
            .client
            .get(format!("{}/version", self.base_url))
            .send()
            .await
            .context("failed to query Clash API /version")?
            .error_for_status()
            .context("Clash API /version returned an error")?
            .json()
            .await
            .context("failed to decode Clash API /version response")?;

        let traffic: TrafficSnapshot = self
            .client
            .get(format!("{}/traffic", self.base_url))
            .send()
            .await
            .context("failed to query Clash API /traffic")?
            .error_for_status()
            .context("Clash API /traffic returned an error")?
            .json()
            .await
            .context("failed to decode Clash API /traffic response")?;

        let connections: ConnectionsResponse = self
            .client
            .get(format!("{}/connections", self.base_url))
            .send()
            .await
            .context("failed to query Clash API /connections")?
            .error_for_status()
            .context("Clash API /connections returned an error")?
            .json()
            .await
            .context("failed to decode Clash API /connections response")?;

        Ok(status_from_parts(version.version, traffic, connections))
    }
}

fn build_api_client(controller: Option<String>) -> Result<ApiClient> {
    let controller = controller
        .or_else(|| env::var("SING_BOX_CONTROLLER").ok())
        .unwrap_or_else(|| DEFAULT_CONTROLLER.to_string());
    let secret = env::var("SING_BOX_SECRET")
        .ok()
        .filter(|value| !value.is_empty());
    ApiClient::new(controller, secret)
}

fn selectors_from_payload(payload: ProxiesResponse) -> Vec<ProxyGroup> {
    let mut groups = payload
        .proxies
        .into_values()
        .filter_map(|proxy| proxy_group_from_node(proxy).ok())
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| left.name.cmp(&right.name));
    groups
}

fn proxy_group_from_node(proxy: ProxyNode) -> Result<ProxyGroup> {
    if !proxy.kind.eq_ignore_ascii_case("selector") {
        bail!("proxy is not a selector group");
    }
    Ok(ProxyGroup {
        name: proxy.name,
        current: proxy.now,
        members: proxy.all,
    })
}

fn status_from_parts(
    version: String,
    traffic: TrafficSnapshot,
    connections: ConnectionsResponse,
) -> StatusOutput {
    StatusOutput {
        version,
        traffic,
        upload_total: connections.upload_total,
        download_total: connections.download_total,
        memory: connections.memory,
        connection_count: connections.connections.len(),
        connections: connections.connections,
    }
}

pub(crate) fn filter_benchmark_candidates(
    all: &[String],
    request: &BenchmarkRequest,
) -> Vec<String> {
    if let Some(nodes) = &request.nodes {
        let wanted = nodes.iter().collect::<std::collections::BTreeSet<_>>();
        all.iter()
            .filter(|name| wanted.contains(name))
            .cloned()
            .collect()
    } else {
        all.iter()
            .filter(|name| name.contains(&request.pattern))
            .cloned()
            .collect()
    }
}

pub(crate) fn spawn_benchmark_worker(
    base_url: String,
    client: AsyncClient,
    request: BenchmarkRequest,
    tx: Sender<BenchmarkEvent>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let runtime = match TokioRuntimeBuilder::new_multi_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(_) => {
                let _ = tx.send(BenchmarkEvent::Finished);
                return;
            }
        };

        runtime.block_on(async move {
            let selector_name = request.selector.clone();
            let selector =
                match fetch_selector_for_benchmark(&client, &base_url, &selector_name).await {
                    Ok(selector) => selector,
                    Err(_) => {
                        let _ = tx.send(BenchmarkEvent::Finished);
                        return;
                    }
                };

            let candidates = filter_benchmark_candidates(&selector.all, &request);

            let max_concurrency = request.max_concurrency.max(1);
            let mut tasks = JoinSet::new();
            let mut pending = candidates.into_iter();

            for _ in 0..max_concurrency {
                let Some(name) = pending.next() else {
                    break;
                };
                spawn_benchmark_task(
                    &mut tasks,
                    client.clone(),
                    base_url.clone(),
                    name,
                    request.url.clone(),
                    request.timeout_ms,
                    request.request_timeout,
                );
            }

            while let Some(result) = tasks.join_next().await {
                if let Ok(result) = result {
                    let _ = tx.send(BenchmarkEvent::Progress(result));
                }
                if let Some(name) = pending.next() {
                    spawn_benchmark_task(
                        &mut tasks,
                        client.clone(),
                        base_url.clone(),
                        name,
                        request.url.clone(),
                        request.timeout_ms,
                        request.request_timeout,
                    );
                }
            }

            let _ = tx.send(BenchmarkEvent::Finished);
        });
    })
}

async fn fetch_selector_for_benchmark(
    client: &AsyncClient,
    base_url: &str,
    selector: &str,
) -> Result<ProxyNode> {
    let encoded = encode(selector);
    client
        .get(format!("{}/proxies/{}", base_url, encoded))
        .send()
        .await
        .with_context(|| format!("failed to query Clash API selector {selector}"))?
        .error_for_status()
        .with_context(|| format!("Clash API rejected selector read for {selector}"))?
        .json()
        .await
        .context("failed to decode Clash API selector response")
}

fn spawn_benchmark_task(
    tasks: &mut JoinSet<BenchmarkResult>,
    client: AsyncClient,
    base_url: String,
    name: String,
    url: String,
    timeout_ms: u64,
    request_timeout: f64,
) {
    tasks.spawn(async move {
        let delay = measure_delay(
            client,
            base_url,
            name.clone(),
            url,
            timeout_ms,
            request_timeout,
        )
        .await;
        BenchmarkResult {
            name,
            delay,
            completed: true,
        }
    });
}

async fn measure_delay(
    client: AsyncClient,
    base_url: String,
    proxy_name: String,
    url: String,
    timeout_ms: u64,
    request_timeout: f64,
) -> Option<u64> {
    let encoded_name = encode(&proxy_name);
    let encoded_url = encode(&url);
    let response = client
        .get(format!(
            "{}/proxies/{}/delay?timeout={}&url={}",
            base_url, encoded_name, timeout_ms, encoded_url
        ))
        .timeout(Duration::from_secs_f64(request_timeout))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let payload: DelayResponse = response.json().await.ok()?;
    payload.delay
}

#[derive(Deserialize)]
struct ProxiesResponse {
    proxies: BTreeMap<String, ProxyNode>,
}

#[derive(Deserialize)]
struct ProxyNode {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    now: Option<String>,
    #[serde(default)]
    all: Vec<String>,
}

#[derive(Deserialize)]
struct DelayResponse {
    #[serde(default)]
    delay: Option<u64>,
}

#[derive(Deserialize)]
struct VersionResponse {
    version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TrafficSnapshot {
    #[serde(rename = "up", alias = "upload")]
    pub(crate) upload: u64,
    #[serde(rename = "down", alias = "download")]
    pub(crate) download: u64,
}

#[derive(Deserialize)]
struct ConnectionsResponse {
    #[serde(rename = "uploadTotal", default)]
    upload_total: Option<u64>,
    #[serde(rename = "downloadTotal", default)]
    download_total: Option<u64>,
    #[serde(default)]
    memory: Option<u64>,
    #[serde(default)]
    connections: Vec<ConnectionInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ConnectionInfo {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) download: u64,
    #[serde(default)]
    pub(crate) upload: u64,
    #[serde(default)]
    pub(crate) start: Option<String>,
    #[serde(default)]
    pub(crate) chains: Vec<String>,
    #[serde(default)]
    pub(crate) rule: Option<String>,
    #[serde(rename = "rulePayload", default)]
    pub(crate) rule_payload: Option<String>,
    #[serde(default)]
    pub(crate) metadata: ConnectionMetadata,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ConnectionMetadata {
    #[serde(default, rename = "network")]
    pub(crate) network: Option<String>,
    #[serde(default, rename = "type")]
    pub(crate) kind: Option<String>,
    #[serde(default, rename = "sourceIP")]
    pub(crate) source_ip: Option<String>,
    #[serde(default, rename = "destinationIP")]
    pub(crate) destination_ip: Option<String>,
    #[serde(default)]
    pub(crate) host: Option<String>,
    #[serde(default, rename = "destinationPort")]
    pub(crate) destination_port: Option<String>,
    #[serde(default, rename = "sourcePort")]
    pub(crate) source_port: Option<String>,
    #[serde(default, rename = "processPath")]
    pub(crate) process_path: Option<String>,
}

#[derive(Serialize)]
struct SwitchProxyRequest {
    name: String,
}

#[derive(Serialize)]
struct UpdateConfigRequest {
    mode: String,
}

pub(crate) struct SelectorsOptions {
    pub(crate) controller: Option<String>,
    pub(crate) selector: Option<String>,
}

pub(crate) struct StatusOptions {
    pub(crate) controller: Option<String>,
}

pub(crate) struct BenchmarkOptions {
    pub(crate) controller: Option<String>,
    pub(crate) selector: String,
    pub(crate) pattern: String,
    pub(crate) url: String,
    pub(crate) timeout_ms: u64,
    pub(crate) request_timeout: f64,
    pub(crate) max_concurrency: usize,
    pub(crate) switch: bool,
    pub(crate) verify: bool,
    pub(crate) verify_discord: bool,
}

#[derive(Clone)]
pub(crate) struct BenchmarkRequest {
    pub(crate) selector: String,
    pub(crate) pattern: String,
    pub(crate) url: String,
    pub(crate) timeout_ms: u64,
    pub(crate) request_timeout: f64,
    pub(crate) max_concurrency: usize,
    pub(crate) nodes: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BenchmarkResult {
    pub(crate) name: String,
    pub(crate) delay: Option<u64>,
    #[serde(skip)]
    pub(crate) completed: bool,
}

impl BenchmarkResult {
    pub(crate) fn display_delay(&self) -> String {
        match (self.delay, self.completed) {
            (Some(delay), _) => format!("{delay}ms"),
            (None, false) => "...".to_string(),
            (None, true) => "fail".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BenchmarkSummary {
    pub(crate) selector: String,
    pub(crate) current: Option<String>,
    pub(crate) pattern: String,
    pub(crate) url: String,
    pub(crate) timeout_ms: u64,
    pub(crate) max_concurrency: usize,
    pub(crate) results: Vec<BenchmarkResult>,
}

#[derive(Clone)]
pub(crate) enum BenchmarkJobKind {
    Group,
    AutoSelect,
    SingleNode { node: String },
}

pub(crate) struct BenchmarkJob {
    pub(crate) group: String,
    pub(crate) nodes: Vec<String>,
    pub(crate) kind: BenchmarkJobKind,
    pub(crate) receiver: std::sync::mpsc::Receiver<BenchmarkEvent>,
    pub(crate) worker: JoinHandle<()>,
}

pub(crate) enum BenchmarkEvent {
    Progress(BenchmarkResult),
    Finished,
}

impl BenchmarkSummary {
    pub(crate) fn empty(selector: String) -> Self {
        Self {
            selector,
            current: None,
            pattern: String::new(),
            url: String::new(),
            timeout_ms: 0,
            max_concurrency: 1,
            results: Vec::new(),
        }
    }

    pub(crate) fn upsert_pending(&mut self, name: String) {
        if let Some(existing) = self.results.iter_mut().find(|item| item.name == name) {
            existing.delay = None;
            existing.completed = false;
        } else {
            self.results.push(BenchmarkResult {
                name,
                delay: None,
                completed: false,
            });
        }
    }

    pub(crate) fn update_result(&mut self, result: BenchmarkResult) {
        if let Some(existing) = self
            .results
            .iter_mut()
            .find(|item| item.name == result.name)
        {
            *existing = result;
        } else {
            self.results.push(result);
        }
    }

    pub(crate) fn best_label(&self) -> String {
        self.best_success()
            .map(|item| format!("{} ({})", item.name, item.display_delay()))
            .unwrap_or_else(|| "pending".to_string())
    }

    pub(crate) fn best_success(&self) -> Option<&BenchmarkResult> {
        self.results
            .iter()
            .filter(|item| item.completed)
            .filter_map(|item| item.delay.map(|delay| (item, delay)))
            .min_by_key(|(_, delay)| *delay)
            .map(|(item, _)| item)
    }

    pub(crate) fn find_result(&self, name: &str) -> Option<&BenchmarkResult> {
        self.results.iter().find(|item| item.name == name)
    }
}

#[derive(Serialize)]
pub(crate) struct BenchmarkOutput {
    pub(crate) selector: String,
    pub(crate) current: Option<String>,
    pub(crate) pattern: String,
    pub(crate) test_url: String,
    pub(crate) timeout_ms: u64,
    pub(crate) max_concurrency: usize,
    pub(crate) results: Vec<BenchmarkResult>,
    pub(crate) best: Option<BenchmarkResult>,
    pub(crate) switched: bool,
    pub(crate) final_node: Option<String>,
    pub(crate) verification: Option<VerificationReport>,
}

#[derive(Serialize)]
pub(crate) struct SelectorsOutput {
    pub(crate) groups: Vec<ProxyGroupOutput>,
}

#[derive(Serialize)]
pub(crate) struct ProxyGroupOutput {
    pub(crate) name: String,
    pub(crate) current: Option<String>,
    pub(crate) members: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StatusOutput {
    pub(crate) version: String,
    pub(crate) traffic: TrafficSnapshot,
    pub(crate) upload_total: Option<u64>,
    pub(crate) download_total: Option<u64>,
    pub(crate) memory: Option<u64>,
    pub(crate) connection_count: usize,
    pub(crate) connections: Vec<ConnectionInfo>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ShellCheck {
    pub(crate) code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl ShellCheck {
    pub(crate) fn ok(&self) -> bool {
        self.code == 0
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct VerificationReport {
    pub(crate) google_v4: ShellCheck,
    pub(crate) github: ShellCheck,
    pub(crate) discord_gateway_rest: Option<ShellCheck>,
    pub(crate) discord_gateway_logs: Option<ShellCheck>,
}

impl VerificationReport {
    pub(crate) fn summary_line(&self) -> String {
        let mut parts = vec![
            format!("google={}", if self.google_v4.ok() { "ok" } else { "fail" }),
            format!("github={}", if self.github.ok() { "ok" } else { "fail" }),
        ];
        if let Some(rest) = &self.discord_gateway_rest {
            parts.push(format!(
                "discord_rest={}",
                if rest.ok() { "ok" } else { "fail" }
            ));
        }
        if let Some(logs) = &self.discord_gateway_logs {
            parts.push(format!(
                "discord_logs={}",
                if logs.ok() { "hits" } else { "clean/none" }
            ));
        }
        format!("Verification: {}", parts.join("  "))
    }
}

pub(crate) fn run_verification(include_discord: bool) -> VerificationReport {
    let google_v4 =
        run_http_verification("https://www.google.com", true, 5, Some(("accept", "*/*")));
    let github = run_http_verification("https://github.com", false, 5, Some(("accept", "*/*")));
    let discord_gateway_rest = include_discord.then(|| {
        run_http_verification(
            "https://discord.com/api/v10/gateway",
            false,
            8,
            Some(("accept", "application/json")),
        )
    });
    let discord_gateway_logs = include_discord.then(run_journalctl_verification);

    VerificationReport {
        google_v4,
        github,
        discord_gateway_rest,
        discord_gateway_logs,
    }
}

fn run_http_verification(
    url: &str,
    force_ipv4: bool,
    max_lines: usize,
    extra_header: Option<(&str, &str)>,
) -> ShellCheck {
    let runtime = match TokioRuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return ShellCheck {
                code: -1,
                stdout: String::new(),
                stderr: format!("failed to build Tokio runtime for verification: {error}"),
            };
        }
    };

    runtime.block_on(async move {
        let mut builder = AsyncClient::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(12))
            .user_agent("sing-box-tui/0.1 verification");
        if force_ipv4 {
            builder = builder.local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        }
        let client = match builder.build() {
            Ok(client) => client,
            Err(error) => {
                return ShellCheck {
                    code: -1,
                    stdout: String::new(),
                    stderr: format!("failed to build verification HTTP client: {error}"),
                };
            }
        };

        let mut request = client.head(url);
        if let Some((name, value)) = extra_header {
            request = request.header(name, value);
        }

        match request.send().await {
            Ok(response) => ShellCheck {
                code: if response.status().is_success() { 0 } else { 1 },
                stdout: format_response_head(&response, max_lines),
                stderr: String::new(),
            },
            Err(error) => ShellCheck {
                code: 1,
                stdout: String::new(),
                stderr: error.to_string(),
            },
        }
    })
}

fn format_response_head(response: &reqwest::Response, max_lines: usize) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "{} {} {}",
        format_http_version(response.version()),
        response.status().as_u16(),
        response.status().canonical_reason().unwrap_or("")
    ));
    for (name, value) in response.headers() {
        let value = value.to_str().unwrap_or("<binary>");
        lines.push(format!("{}: {}", name.as_str(), value));
    }
    lines
        .into_iter()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_http_version(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2.0",
        Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/?",
    }
}

fn run_journalctl_verification() -> ShellCheck {
    let runtime = match TokioRuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return ShellCheck {
                code: -1,
                stdout: String::new(),
                stderr: format!(
                    "failed to build Tokio runtime for journalctl verification: {error}"
                ),
            };
        }
    };

    runtime.block_on(async move {
        match TokioCommand::new("journalctl")
            .args([
                "--user",
                "-u",
                "openclaw-gateway",
                "--since",
                "5 min ago",
                "--no-pager",
                "-l",
            ])
            .output()
            .await
        {
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if !output.status.success() {
                    return ShellCheck {
                        code: output.status.code().unwrap_or(-1),
                        stdout: String::new(),
                        stderr,
                    };
                }

                let patterns = [
                    "discord",
                    "1006",
                    "econnreset",
                    "fetch failed",
                    "gateway error",
                ];
                let lines = String::from_utf8_lossy(&output.stdout);
                let matched = lines
                    .lines()
                    .filter(|line| {
                        let lower = line.to_ascii_lowercase();
                        patterns.iter().any(|pattern| lower.contains(pattern))
                    })
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let start = matched.len().saturating_sub(40);

                ShellCheck {
                    code: if matched.is_empty() { 1 } else { 0 },
                    stdout: matched[start..].join("\n"),
                    stderr,
                }
            }
            Err(error) => ShellCheck {
                code: -1,
                stdout: String::new(),
                stderr: error.to_string(),
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        BenchmarkOutput, BenchmarkRequest, BenchmarkResult, BenchmarkSummary, ConnectionsResponse,
        ProxiesResponse, TrafficSnapshot, UpdateConfigRequest, selectors_from_payload,
        status_from_parts,
    };

    #[test]
    fn benchmark_summary_picks_lowest_successful_delay() {
        let summary = BenchmarkSummary {
            selector: "select".to_string(),
            current: Some("node-b".to_string()),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 16,
            results: vec![
                BenchmarkResult {
                    name: "node-a".to_string(),
                    delay: Some(100),
                    completed: true,
                },
                BenchmarkResult {
                    name: "node-b".to_string(),
                    delay: Some(80),
                    completed: true,
                },
                BenchmarkResult {
                    name: "node-c".to_string(),
                    delay: None,
                    completed: true,
                },
            ],
        };

        let best = summary.best_success().expect("best result");
        assert_eq!(best.name, "node-b");
        assert_eq!(best.delay, Some(80));
    }

    #[test]
    fn benchmark_output_serializes_max_concurrency() {
        let output = BenchmarkOutput {
            selector: "select".to_string(),
            current: Some("node-a".to_string()),
            pattern: "美国".to_string(),
            test_url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 7,
            results: vec![BenchmarkResult {
                name: "node-a".to_string(),
                delay: Some(42),
                completed: true,
            }],
            best: None,
            switched: false,
            final_node: Some("node-a".to_string()),
            verification: None,
        };

        let json = serde_json::to_value(output).expect("serialize benchmark output");
        assert_eq!(json["max_concurrency"], 7);
    }

    #[test]
    fn benchmark_request_carries_max_concurrency() {
        let request = BenchmarkRequest {
            selector: "select".to_string(),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            request_timeout: 12.0,
            max_concurrency: 3,
            nodes: None,
        };

        assert_eq!(request.max_concurrency, 3);
    }

    #[test]
    fn update_config_request_serializes_mode() {
        let json = serde_json::to_value(UpdateConfigRequest {
            mode: "直连".to_string(),
        })
        .expect("serialize update config request");

        assert_eq!(json["mode"], "直连");
    }

    #[test]
    fn fetch_selector_groups_returns_sorted_selectors() {
        let payload: ProxiesResponse = serde_json::from_str(
            r#"{"proxies":{"DIRECT":{"name":"DIRECT","type":"Direct"},"auto":{"name":"auto","type":"Selector","now":"node-b","all":["node-a","node-b"]},"select":{"name":"select","type":"Selector","now":"node-a","all":["node-a","node-b"]}}}"#,
        )
        .expect("parse proxies payload");
        let groups = selectors_from_payload(payload);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "auto");
        assert_eq!(groups[0].current.as_deref(), Some("node-b"));
        assert_eq!(groups[1].name, "select");
        assert_eq!(groups[1].members, vec!["node-a", "node-b"]);
    }

    #[test]
    fn fetch_status_combines_version_traffic_and_connections() {
        let traffic: TrafficSnapshot =
            serde_json::from_str(r#"{"up":123,"down":456}"#).expect("parse traffic payload");
        let connections: ConnectionsResponse = serde_json::from_str(
            r#"{"downloadTotal":4096,"uploadTotal":2048,"memory":512,"connections":[{"id":"conn-1","download":300,"upload":100,"chains":["select","node-a"],"rule":"MATCH","rulePayload":"","metadata":{"network":"tcp","type":"http","sourceIP":"127.0.0.1","destinationIP":"1.1.1.1","host":"example.com","destinationPort":"443"}}]}"#,
        )
        .expect("parse connections payload");
        let status = status_from_parts("1.12.0".to_string(), traffic, connections);

        assert_eq!(status.version, "1.12.0");
        assert_eq!(status.traffic.upload, 123);
        assert_eq!(status.traffic.download, 456);
        assert_eq!(status.memory, Some(512));
        assert_eq!(status.connection_count, 1);
        assert_eq!(status.upload_total, Some(2048));
        assert_eq!(status.download_total, Some(4096));
        assert_eq!(status.connections.len(), 1);
        assert_eq!(status.connections[0].id, "conn-1");
        assert_eq!(status.connections[0].chains, vec!["select", "node-a"]);
        assert_eq!(
            status.connections[0].metadata.host.as_deref(),
            Some("example.com")
        );
    }
}

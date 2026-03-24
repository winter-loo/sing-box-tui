use std::collections::BTreeMap;
use std::env;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Version;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::Client as AsyncClient;
use serde::{Deserialize, Serialize};
use tokio::process::Command as TokioCommand;
use tokio::runtime::{Builder as TokioRuntimeBuilder, Runtime as TokioRuntime};
use tokio::task::JoinSet;
use urlencoding::encode;

use crate::defaults::DEFAULT_CONTROLLER;

pub(crate) fn run_benchmark(options: BenchmarkOptions) -> Result<()> {
    let controller = options
        .controller
        .or_else(|| env::var("SING_BOX_CONTROLLER").ok())
        .unwrap_or_else(|| DEFAULT_CONTROLLER.to_string());
    let secret = env::var("SING_BOX_SECRET")
        .ok()
        .filter(|value| !value.is_empty());

    let client = ApiClient::new(controller, secret)?;
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

        let mut groups = payload
            .proxies
            .into_values()
            .filter(|proxy| proxy.kind.eq_ignore_ascii_case("selector"))
            .map(|proxy| ProxyGroup {
                name: proxy.name,
                current: proxy.now,
                members: proxy.all,
            })
            .collect::<Vec<_>>();

        groups.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(groups)
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

    pub(crate) fn benchmark_selector(&self, request: &BenchmarkRequest) -> Result<BenchmarkSummary> {
        self.runtime.block_on(self.benchmark_selector_async(request))
    }

    pub(crate) fn fetch_benchmark_candidates(&self, request: &BenchmarkRequest) -> Result<Vec<String>> {
        self.runtime.block_on(self.fetch_benchmark_candidates_async(request))
    }

    async fn fetch_benchmark_candidates_async(
        &self,
        request: &BenchmarkRequest,
    ) -> Result<Vec<String>> {
        let selector = self.fetch_selector_async(&request.selector).await?;
        Ok(filter_benchmark_candidates(&selector.all, request))
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
}

pub(crate) fn filter_benchmark_candidates(all: &[String], request: &BenchmarkRequest) -> Vec<String> {
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

#[derive(Serialize)]
struct SwitchProxyRequest {
    name: String,
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
        if let Some(existing) = self.results.iter_mut().find(|item| item.name == result.name) {
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
    use super::{BenchmarkOutput, BenchmarkRequest, BenchmarkResult, BenchmarkSummary};

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
}

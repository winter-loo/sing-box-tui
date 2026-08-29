use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use reqwest::Url;
use serde::Serialize;
use serde_json::json;
use tokio::runtime::Builder as TokioRuntimeBuilder;

use crate::atomic_file::write_atomic;
use crate::config::{
    DefaultConfigOptions, ensure_bypass_rule_set_file_for_config,
    resolved_bypass_rule_set_path_for_config,
};
use crate::config_mutation::{lock_config_mutation_for, paths_refer_to_same_target};
use crate::import::{
    SubscriptionConfigRequest, build_full_config_from_singbox_subscription_with_options,
    commit_subscription_payload_to_active_config,
};
use crate::node_quality_path::default_benchmark_db_path_for_config;

mod airtcp;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + 'a>>;

trait ProviderPlugin {
    fn key(&self) -> &'static str;
    fn matches(&self, provider_url: &Url) -> bool;
    fn login<'a>(
        &'a self,
        client: &'a Client,
        provider_url: &'a Url,
        credentials: &'a ProviderCredentials,
    ) -> BoxFuture<'a, String>;
    fn fetch_subscription_url<'a>(
        &'a self,
        client: &'a Client,
        provider_url: &'a Url,
        cookies: &'a str,
        format: SubscriptionFormat,
    ) -> BoxFuture<'a, Url>;
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubscriptionFormat {
    SingBox,
    Clash,
}

impl SubscriptionFormat {
    fn marker(self) -> &'static str {
        match self {
            Self::SingBox => "singbox=1",
            Self::Clash => "mihomo=1",
        }
    }
}

pub(crate) struct ProviderSyncOptions {
    pub(crate) provider: String,
    pub(crate) account_file: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) output: Option<PathBuf>,
    pub(crate) subscription_output: Option<PathBuf>,
    pub(crate) replace_nodes: bool,
    pub(crate) include_geosite_rules: bool,
    pub(crate) include_tun_mode: bool,
    pub(crate) write: bool,
}

pub(crate) fn run_provider_sync(options: ProviderSyncOptions) -> Result<()> {
    let ProviderSyncOptions {
        provider,
        account_file,
        config_path,
        output,
        subscription_output,
        replace_nodes,
        include_geosite_rules,
        include_tun_mode,
        write,
    } = options;
    let output = output.as_ref();
    let subscription_output = subscription_output.as_ref();
    let requested_merged_path =
        provider_merged_path(&config_path, output.map(PathBuf::as_path), write)?;
    let writes_active_config = paths_refer_to_same_target(&config_path, &requested_merged_path)?;
    let bypass_path = resolved_bypass_rule_set_path_for_config(&requested_merged_path)?;
    let resolved_database_path = default_benchmark_db_path_for_config(&config_path)?;
    let database_path = if writes_active_config {
        let mut auxiliary = vec![("provider account file", account_file.as_path())];
        if let Some(path) = subscription_output {
            auxiliary.push(("subscription output", path.as_path()));
        }
        auxiliary.push(("bypass rule-set", bypass_path.as_path()));
        crate::node_quality_path::ensure_active_config_paths_are_distinct(
            &config_path,
            &resolved_database_path,
            &auxiliary,
        )?;
        Some(resolved_database_path)
    } else {
        let mut paths = vec![
            ("provider account file", account_file.as_path()),
            ("merged provider output", requested_merged_path.as_path()),
        ];
        if let Some(path) = subscription_output {
            paths.push(("subscription output", path.as_path()));
        }
        paths.push(("bypass rule-set", bypass_path.as_path()));
        crate::node_quality_path::ensure_active_config_paths_are_distinct(
            &config_path,
            &resolved_database_path,
            &paths,
        )?;
        None
    };
    let credentials = ProviderCredentials::from_file(&account_file)?;
    let provider_url = normalize_provider_url(&provider)?;
    let runtime = TokioRuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build Tokio runtime for provider sync")?;
    let runtime_provider_url = provider_url.clone();

    let result = runtime.block_on(async move {
        let plugin = resolve_provider_plugin(&runtime_provider_url)?;
        let client = Client::builder()
            .no_proxy()
            .build()
            .context("failed to build direct provider HTTP client")?;

        let cookies = plugin
            .login(&client, &runtime_provider_url, &credentials)
            .await?;
        let subscription_url = plugin
            .fetch_subscription_url(
                &client,
                &runtime_provider_url,
                &cookies,
                SubscriptionFormat::SingBox,
            )
            .await?;
        let subscription_json = client
            .get(subscription_url.clone())
            .header("Cookie", &cookies)
            .send()
            .await
            .context("failed to fetch sing-box subscription JSON")?
            .error_for_status()
            .context("provider rejected sing-box subscription request")?
            .text()
            .await
            .context("failed to read sing-box subscription response")?;
        Ok::<_, anyhow::Error>((plugin.key(), subscription_url, subscription_json))
    })?;

    let (plugin_key, subscription_url, subscription_json) = result;
    if let Some(path) = subscription_output {
        write_atomic(path, format!("{subscription_json}\n").as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    let (merged_path, imported_nodes) = write_provider_payload(
        &config_path,
        ProviderPayloadWrite {
            output: output.map(PathBuf::as_path),
            write,
            database_path: database_path.as_deref(),
            subscription: SubscriptionConfigRequest::without_provider(
                &subscription_json,
                replace_nodes,
                DefaultConfigOptions {
                    include_geosite_rules,
                    include_tun_mode,
                },
            ),
        },
    )?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!(ProviderSyncOutput {
            provider: provider_url.to_string(),
            provider_plugin: plugin_key.to_string(),
            singbox_subscription_url: subscription_url.to_string(),
            imported_nodes,
            merged_config_path: merged_path.display().to_string(),
            subscription_output_path: subscription_output.map(|path| path.display().to_string()),
        }))?
    );
    Ok(())
}

fn provider_merged_path(config_path: &Path, output: Option<&Path>, write: bool) -> Result<PathBuf> {
    if let Some(path) = output {
        Ok(path.to_path_buf())
    } else if write {
        Ok(config_path.to_path_buf())
    } else {
        bail!("sync requires either --output <FILE> or --write")
    }
}

struct ProviderPayloadWrite<'a> {
    output: Option<&'a Path>,
    write: bool,
    database_path: Option<&'a Path>,
    subscription: SubscriptionConfigRequest<'a>,
}

fn write_provider_payload(
    config_path: &PathBuf,
    options: ProviderPayloadWrite<'_>,
) -> Result<(PathBuf, usize)> {
    let ProviderPayloadWrite {
        output,
        write,
        database_path,
        subscription,
    } = options;
    let merged_path = provider_merged_path(config_path, output, write)?;
    let imported_nodes = if paths_refer_to_same_target(config_path, &merged_path)? {
        commit_subscription_payload_to_active_config(
            config_path,
            &merged_path,
            database_path.context("active provider sync requires node-quality storage")?,
            subscription,
        )?
    } else {
        let _config_guard = lock_config_mutation_for(config_path)?;
        let (config, imported_nodes) = build_full_config_from_singbox_subscription_with_options(
            config_path,
            subscription.subscription_json,
            subscription.replace_nodes,
            subscription.config_options,
        )?;
        let contents = serde_json::to_string_pretty(&config)
            .context("failed to serialize merged provider config")?;
        ensure_bypass_rule_set_file_for_config(&merged_path)?;
        write_atomic(&merged_path, format!("{contents}\n").as_bytes())
            .with_context(|| format!("failed to write {}", merged_path.display()))?;
        imported_nodes
    };
    Ok((merged_path, imported_nodes))
}

#[cfg(test)]
pub(crate) fn commit_provider_payload_to_active_config(
    config_path: &PathBuf,
    active_output: &Path,
    database_path: &Path,
    subscription_json: &str,
    replace_nodes: bool,
    include_geosite_rules: bool,
    include_tun_mode: bool,
) -> Result<usize> {
    commit_subscription_payload_to_active_config(
        config_path,
        active_output,
        database_path,
        SubscriptionConfigRequest::without_provider(
            subscription_json,
            replace_nodes,
            DefaultConfigOptions {
                include_geosite_rules,
                include_tun_mode,
            },
        ),
    )
}

#[derive(Serialize)]
struct ProviderSyncOutput {
    provider: String,
    provider_plugin: String,
    singbox_subscription_url: String,
    imported_nodes: usize,
    merged_config_path: String,
    subscription_output_path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProviderCredentials {
    username: String,
    password: String,
}

impl ProviderCredentials {
    fn from_file(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read account file {}", path.display()))?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self> {
        let lines = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect::<Vec<_>>();
        if lines.is_empty() {
            bail!("account file is empty");
        }

        if lines.iter().all(|line| line.contains('=')) {
            let mut username = None;
            let mut password = None;
            for line in lines {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                match key.trim() {
                    "email" | "username" | "user" | "account" => {
                        username = Some(value.trim().to_string())
                    }
                    "password" | "passwd" | "pass" => password = Some(value.trim().to_string()),
                    _ => {}
                }
            }

            return Ok(Self {
                username: username.context("account file is missing email/username")?,
                password: password.context("account file is missing password")?,
            });
        }

        if lines.len() < 2 {
            bail!(
                "account file must contain username/email on the first line and password on the second line"
            );
        }

        Ok(Self {
            username: lines[0].to_string(),
            password: lines[1].to_string(),
        })
    }
}

fn resolve_provider_plugin(provider_url: &Url) -> Result<&'static dyn ProviderPlugin> {
    let plugins: [&'static dyn ProviderPlugin; 1] = [&airtcp::AIRTCP_PLUGIN];
    plugins
        .into_iter()
        .find(|plugin| plugin.matches(provider_url))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unsupported provider host: {}",
                provider_url.host_str().unwrap_or_default()
            )
        })
}

fn normalize_provider_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).with_context(|| format!("invalid provider URL: {value}"))?;
    if url.path().is_empty() {
        url.set_path("/");
    }
    Ok(url)
}

fn capture_cookie_header(headers: &reqwest::header::HeaderMap) -> Result<String> {
    let cookies = headers
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if cookies.is_empty() {
        bail!("provider login response did not include any cookies");
    }
    Ok(cookies.join("; "))
}

fn extract_script_asset_urls(base_url: &Url, html: &str) -> Result<Vec<Url>> {
    let mut urls = Vec::new();
    let mut remaining = html;
    while let Some(index) = remaining.find(r#"src=""#) {
        let after_src = &remaining[index + 5..];
        let Some(end) = after_src.find('"') else {
            break;
        };
        let candidate = &after_src[..end];
        if candidate.ends_with(".js") {
            urls.push(
                base_url
                    .join(candidate)
                    .with_context(|| format!("invalid asset URL in provider HTML: {candidate}"))?,
            );
        }
        remaining = &after_src[end + 1..];
    }
    Ok(urls)
}

fn extract_subscription_url_from_text(
    base_url: &Url,
    text: &str,
    format: SubscriptionFormat,
) -> Result<Option<Url>> {
    let marker = format.marker();
    let Some(index) = text.find(marker) else {
        return Ok(None);
    };
    let bytes = text.as_bytes();
    let mut start = index;
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if matches!(ch, '"' | '\'' | ' ' | '\n' | '\r' | '\t') {
            break;
        }
        start -= 1;
    }
    let mut end = index + marker.len();
    while end < bytes.len() {
        let ch = bytes[end] as char;
        if matches!(ch, '"' | '\'' | ' ' | '\n' | '\r' | '\t') {
            break;
        }
        end += 1;
    }
    let candidate = text[start..end]
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`'))
        .replace("\\u002F", "/")
        .replace("\\/", "/");
    if candidate.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        base_url
            .join(&candidate)
            .or_else(|_| Url::parse(&candidate))
            .with_context(|| format!("invalid subscription URL candidate: {candidate}"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        ProviderCredentials, ProviderPayloadWrite, ProviderSyncOptions, SubscriptionFormat,
        extract_script_asset_urls, extract_subscription_url_from_text, normalize_provider_url,
        resolve_provider_plugin, run_provider_sync, write_provider_payload,
    };
    use crate::config::{DefaultConfigOptions, build_default_config};
    use crate::import::SubscriptionConfigRequest;
    use serde_json::json;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-{label}-{nonce}"))
    }

    #[test]
    fn provider_raw_subscription_output_cannot_alias_the_active_config() {
        let dir = temp_dir("provider-raw-active-alias");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");

        let error = run_provider_sync(ProviderSyncOptions {
            provider: "this provider must never be parsed".to_string(),
            account_file: dir.join("missing-account"),
            config_path: config_path.clone(),
            output: Some(config_path.clone()),
            subscription_output: Some(config_path),
            replace_nodes: true,
            include_geosite_rules: false,
            include_tun_mode: false,
            write: false,
        })
        .expect_err("raw provider payload output must be rejected before credentials are read");

        assert!(format!("{error:#}").contains("must not alias"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn provider_account_and_output_alias_fails_before_credentials_or_network_io() {
        let dir = temp_dir("provider-account-output-alias");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let account_and_output = dir.join("account.json");
        fs::write(&account_and_output, b"credential-canary\n").expect("write credential canary");

        let error = run_provider_sync(ProviderSyncOptions {
            provider: "this provider must never be parsed".to_string(),
            account_file: account_and_output.clone(),
            config_path,
            output: Some(account_and_output.clone()),
            subscription_output: None,
            replace_nodes: true,
            include_geosite_rules: false,
            include_tun_mode: false,
            write: false,
        })
        .expect_err("account/output alias must fail before credentials are read");

        assert!(format!("{error:#}").contains("must not alias"));
        assert_eq!(
            fs::read(&account_and_output).expect("read credential canary"),
            b"credential-canary\n"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn distinct_provider_output_takes_precedence_over_write_without_touching_quality() {
        let dir = temp_dir("provider-output-write-precedence");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("active.json");
        let output_path = dir.join("preview.json");
        let database_path = dir.join("quality.sqlite3");
        fs::write(
            &config_path,
            serde_json::to_vec_pretty(&build_default_config(vec![json!({
                "type": "trojan", "tag": "node-a", "server": "active.example",
                "server_port": 443, "password": "active-secret"
            })]))
            .expect("serialize active config"),
        )
        .expect("write active config");
        let payload = json!({
            "outbounds": [{
                "type": "trojan", "tag": "node-a", "server": "preview.example",
                "server_port": 443, "password": "preview-secret"
            }]
        })
        .to_string();

        let (merged_path, imported_nodes) = write_provider_payload(
            &config_path,
            ProviderPayloadWrite {
                output: Some(&output_path),
                write: true,
                database_path: None,
                subscription: SubscriptionConfigRequest::without_provider(
                    &payload,
                    true,
                    DefaultConfigOptions {
                        include_geosite_rules: false,
                        include_tun_mode: false,
                    },
                ),
            },
        )
        .expect("provider preview succeeds");

        assert_eq!(merged_path, output_path);
        assert_eq!(imported_nodes, 1);
        assert!(
            fs::read_to_string(&config_path)
                .expect("read active config")
                .contains("active.example")
        );
        assert!(
            fs::read_to_string(&merged_path)
                .expect("read preview config")
                .contains("preview.example")
        );
        assert!(
            !database_path.exists(),
            "a distinct --output remains preview-only even when --write is also present"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolves_airtcp_plugin_from_host() {
        let provider_url = normalize_provider_url("https://3.airtcp.me").expect("provider URL");
        let plugin = resolve_provider_plugin(&provider_url).expect("plugin resolves");

        assert_eq!(plugin.key(), "airtcp");
    }

    #[test]
    fn parses_key_value_account_file() {
        let credentials =
            ProviderCredentials::parse("email=your-email@example.com\npassword=your-password\n")
                .expect("account file parses");

        assert_eq!(credentials.username, "your-email@example.com");
        assert_eq!(credentials.password, "your-password");
    }

    #[test]
    fn parses_two_line_account_file() {
        let credentials = ProviderCredentials::parse("your-email@example.com\nyour-password\n")
            .expect("account file parses");

        assert_eq!(credentials.username, "your-email@example.com");
        assert_eq!(credentials.password, "your-password");
    }

    #[test]
    fn extracts_script_assets_from_html() {
        let base = normalize_provider_url("https://3.airtcp.me").expect("base URL");
        let urls = extract_script_asset_urls(
            &base,
            r#"<script type="module" src="/assets/user-abc.js"></script><script src="/assets/index-def.js"></script>"#,
        )
        .expect("asset extraction succeeds");

        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].as_str(), "https://3.airtcp.me/assets/user-abc.js");
        assert_eq!(urls[1].as_str(), "https://3.airtcp.me/assets/index-def.js");
    }

    #[test]
    fn extracts_absolute_singbox_subscription_url_from_script() {
        let base = normalize_provider_url("https://3.airtcp.me").expect("base URL");
        let url = extract_subscription_url_from_text(
            &base,
            r#"const s="https://spring.mailrelay.us/link/abc?singbox=1";"#,
            SubscriptionFormat::SingBox,
        )
        .expect("subscription extraction succeeds")
        .expect("subscription URL exists");

        assert_eq!(
            url.as_str(),
            "https://spring.mailrelay.us/link/abc?singbox=1"
        );
    }

    #[test]
    fn extracts_absolute_clash_subscription_url_from_script() {
        let base = normalize_provider_url("https://3.airtcp.me").expect("base URL");
        let url = extract_subscription_url_from_text(
            &base,
            r#"const s="https://spring.mailrelay.us/link/abc?mihomo=1";"#,
            SubscriptionFormat::Clash,
        )
        .expect("subscription extraction succeeds")
        .expect("subscription URL exists");

        assert_eq!(
            url.as_str(),
            "https://spring.mailrelay.us/link/abc?mihomo=1"
        );
    }

    #[test]
    fn extracts_escaped_relative_subscription_url_from_script() {
        let base = normalize_provider_url("https://3.airtcp.me").expect("base URL");
        let url = extract_subscription_url_from_text(
            &base,
            r#"const s="\/api\/subscription?singbox=1";"#,
            SubscriptionFormat::SingBox,
        )
        .expect("subscription extraction succeeds")
        .expect("subscription URL exists");

        assert_eq!(
            url.as_str(),
            "https://3.airtcp.me/api/subscription?singbox=1"
        );
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Url;
use reqwest::header::USER_AGENT;
use serde::{Deserialize, Serialize};
use tokio::runtime::Builder as TokioRuntimeBuilder;

use crate::config::{ProviderNodeSet, build_full_config_with_provider_node_sets};
use crate::import::extract_mergeable_outbounds_from_singbox_subscription;

pub(crate) const DEFAULT_SUBSCRIPTION_SOURCE_PATH: &str = ".suburl";
pub(crate) const DEFAULT_SUBSCRIPTION_CACHE_PATH: &str = ".suburl.cache.json";
pub(crate) const DEFAULT_SUBSCRIPTION_INTERVAL_DAYS: u64 = 1;
pub(crate) const SUBSCRIPTION_CONFIG_BACKUP_SUFFIX: &str = "sing-box-tui-subscription-backup";

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

#[derive(Clone, Debug)]
pub(crate) struct SubscriptionRefreshRequest {
    pub(crate) input: PathBuf,
    pub(crate) cache_path: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) merged_path: PathBuf,
    pub(crate) replace_nodes: bool,
    pub(crate) force: bool,
    pub(crate) interval_days: u64,
}

pub(crate) fn run_subscription_refresh(
    input: &Path,
    cache_path: &Path,
    config_path: &Path,
    output: Option<&PathBuf>,
    replace_nodes: bool,
    write: bool,
    force: bool,
    interval_days: u64,
) -> Result<()> {
    let config_path_buf = config_path.to_path_buf();
    let merged_path = if let Some(path) = output.cloned() {
        path
    } else if write {
        config_path_buf.clone()
    } else {
        bail!("subscriptions requires either --output <FILE> or --write");
    };
    let report = refresh_subscriptions(&SubscriptionRefreshRequest {
        input: input.to_path_buf(),
        cache_path: cache_path.to_path_buf(),
        config_path: config_path_buf,
        merged_path,
        replace_nodes,
        force,
        interval_days,
    })?;

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

pub(crate) fn refresh_subscriptions(
    request: &SubscriptionRefreshRequest,
) -> Result<SubscriptionRefreshOutput> {
    if request.interval_days == 0 {
        bail!("--interval-days must be greater than 0");
    }
    let sources = read_subscription_sources(&request.input)?;
    if sources.is_empty() {
        bail!(
            "{} did not contain any subscription URLs",
            request.input.display()
        );
    }

    let cache_store = SubscriptionCacheStore::new(&request.cache_path);
    let mut cache = cache_store.load()?;
    let now_unix = unix_now()?;
    let refresh_interval =
        Duration::from_secs(request.interval_days.saturating_mul(SECONDS_PER_DAY));
    let runtime = TokioRuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build Tokio runtime for subscription refresh")?;

    let resolved = runtime.block_on(resolve_subscriptions(
        &sources,
        &cache,
        request.force,
        refresh_interval,
        now_unix,
    ))?;

    let mut provider_node_sets = Vec::new();
    let mut summaries = Vec::new();
    let mut cache_changed = false;
    for item in resolved {
        let nodes = extract_mergeable_outbounds_from_singbox_subscription(&item.subscription_json)
            .with_context(|| {
                format!(
                    "failed to parse sing-box subscription JSON for {}",
                    item.source.provider_name
                )
            })?;
        if matches!(item.status, SubscriptionFetchStatus::Fetched) {
            cache.entries.insert(
                item.source.provider_name.clone(),
                CachedSubscription {
                    url_hash: hash_url(item.source.url.as_str()),
                    fetched_at_unix: item.fetched_at_unix,
                    subscription_json: item.subscription_json.clone(),
                },
            );
            cache_changed = true;
        }

        let mut warning = item.warning;
        if nodes.is_empty() {
            warning = Some(match warning {
                Some(existing) => format!("{existing}; no mergeable nodes found"),
                None => "no mergeable nodes found".to_string(),
            });
        }

        summaries.push(ProviderRefreshSummary {
            provider: item.source.provider_name.clone(),
            subscription_url: redact_url(item.source.url.as_str()),
            status: item.status.as_str().to_string(),
            imported_nodes: nodes.len(),
            fetched_at_unix: item.fetched_at_unix,
            warning,
        });
        provider_node_sets.push(ProviderNodeSet {
            provider_name: item.source.provider_name,
            nodes,
        });
    }

    let merged_config = build_full_config_with_provider_node_sets(
        &request.config_path,
        provider_node_sets,
        request.replace_nodes,
    )?;

    if cache_changed {
        cache_store.save(&cache)?;
    }
    let backup_path = backup_existing_config(&request.merged_path)?;
    fs::write(
        &request.merged_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&merged_config)
                .context("failed to serialize merged subscription config")?
        ),
    )
    .with_context(|| format!("failed to write {}", request.merged_path.display()))?;

    Ok(SubscriptionRefreshOutput {
        input_path: request.input.display().to_string(),
        cache_path: request.cache_path.display().to_string(),
        interval_days: request.interval_days,
        merged_config_path: request.merged_path.display().to_string(),
        backup_config_path: backup_path.map(|path| path.display().to_string()),
        providers: summaries,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SubscriptionSource {
    provider_name: String,
    url: Url,
}

#[derive(Clone, Debug)]
struct ResolvedSubscription {
    source: SubscriptionSource,
    subscription_json: String,
    fetched_at_unix: u64,
    status: SubscriptionFetchStatus,
    warning: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubscriptionFetchStatus {
    Fetched,
    Cached,
    StaleCache,
}

impl SubscriptionFetchStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fetched => "fetched",
            Self::Cached => "cached",
            Self::StaleCache => "stale-cache",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct SubscriptionCache {
    #[serde(default)]
    entries: BTreeMap<String, CachedSubscription>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedSubscription {
    url_hash: String,
    fetched_at_unix: u64,
    subscription_json: String,
}

#[derive(Clone, Debug)]
struct SubscriptionCacheStore {
    path: PathBuf,
}

impl SubscriptionCacheStore {
    fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    fn load(&self) -> Result<SubscriptionCache> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(SubscriptionCache::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", self.path.display()));
            }
        };
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", self.path.display()))
    }

    fn save(&self, cache: &SubscriptionCache) -> Result<()> {
        let text =
            serde_json::to_string_pretty(cache).context("failed to encode subscription cache")?;
        fs::write(&self.path, format!("{text}\n"))
            .with_context(|| format!("failed to write {}", self.path.display()))
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SubscriptionRefreshOutput {
    pub(crate) input_path: String,
    pub(crate) cache_path: String,
    pub(crate) interval_days: u64,
    pub(crate) merged_config_path: String,
    pub(crate) backup_config_path: Option<String>,
    pub(crate) providers: Vec<ProviderRefreshSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProviderRefreshSummary {
    pub(crate) provider: String,
    pub(crate) subscription_url: String,
    pub(crate) status: String,
    pub(crate) imported_nodes: usize,
    pub(crate) fetched_at_unix: u64,
    pub(crate) warning: Option<String>,
}

fn read_subscription_sources(path: &Path) -> Result<Vec<SubscriptionSource>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read subscription URL file {}", path.display()))?;
    parse_subscription_sources(&text)
}

fn backup_existing_config(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }

    let backup_path = subscription_config_backup_path(path);
    match fs::remove_file(&backup_path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to remove old backup {}", backup_path.display()));
        }
    }
    fs::copy(path, &backup_path).with_context(|| {
        format!(
            "failed to back up {} to {}",
            path.display(),
            backup_path.display()
        )
    })?;
    Ok(Some(backup_path))
}

fn subscription_config_backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    path.with_file_name(format!("{file_name}.{SUBSCRIPTION_CONFIG_BACKUP_SUFFIX}"))
}

fn parse_subscription_sources(text: &str) -> Result<Vec<SubscriptionSource>> {
    let mut sources = Vec::new();
    let mut seen_providers = BTreeSet::new();
    for (line_index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (provider_name, url_text) =
            if let Some((provider_name, url_text)) = line.split_once('=') {
                (provider_name.trim().to_string(), url_text.trim())
            } else {
                (format!("subscription-{}", sources.len() + 1), line)
            };
        if provider_name.is_empty() {
            bail!(
                "subscription line {} has an empty provider name",
                line_index + 1
            );
        }
        if url_text.is_empty() {
            bail!("subscription line {} has an empty URL", line_index + 1);
        }
        if !seen_providers.insert(provider_name.clone()) {
            bail!("duplicate subscription provider name: {provider_name}");
        }

        let url = Url::parse(url_text).map_err(|_| {
            anyhow!(
                "subscription line {} has an invalid URL for {}",
                line_index + 1,
                provider_name
            )
        })?;
        sources.push(SubscriptionSource { provider_name, url });
    }
    Ok(sources)
}

async fn resolve_subscriptions(
    sources: &[SubscriptionSource],
    cache: &SubscriptionCache,
    force: bool,
    refresh_interval: Duration,
    now_unix: u64,
) -> Result<Vec<ResolvedSubscription>> {
    let client = reqwest::Client::builder()
        .build()
        .context("failed to build subscription HTTP client")?;
    let direct_client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("failed to build direct subscription HTTP client")?;
    let mut resolved = Vec::with_capacity(sources.len());
    for source in sources {
        let client = if subscription_source_requires_direct_fetch(source) {
            &direct_client
        } else {
            &client
        };
        resolved.push(
            resolve_subscription(client, source, cache, force, refresh_interval, now_unix).await?,
        );
    }
    Ok(resolved)
}

async fn resolve_subscription(
    client: &reqwest::Client,
    source: &SubscriptionSource,
    cache: &SubscriptionCache,
    force: bool,
    refresh_interval: Duration,
    now_unix: u64,
) -> Result<ResolvedSubscription> {
    let url_hash = hash_url(source.url.as_str());
    let cached = cache
        .entries
        .get(&source.provider_name)
        .filter(|entry| entry.url_hash == url_hash);
    if let Some(cached) = cached
        && !force
        && cache_entry_is_fresh(cached, now_unix, refresh_interval)
    {
        return Ok(ResolvedSubscription {
            source: source.clone(),
            subscription_json: cached.subscription_json.clone(),
            fetched_at_unix: cached.fetched_at_unix,
            status: SubscriptionFetchStatus::Cached,
            warning: None,
        });
    }

    match fetch_subscription_text(client, source).await {
        Ok(subscription_json) => Ok(ResolvedSubscription {
            source: source.clone(),
            subscription_json,
            fetched_at_unix: now_unix,
            status: SubscriptionFetchStatus::Fetched,
            warning: None,
        }),
        Err(error) => {
            if let Some(cached) = cached {
                Ok(ResolvedSubscription {
                    source: source.clone(),
                    subscription_json: cached.subscription_json.clone(),
                    fetched_at_unix: cached.fetched_at_unix,
                    status: SubscriptionFetchStatus::StaleCache,
                    warning: Some(format!("{error}; using cached subscription JSON")),
                })
            } else {
                Err(error)
            }
        }
    }
}

async fn fetch_subscription_text(
    client: &reqwest::Client,
    source: &SubscriptionSource,
) -> Result<String> {
    let response = client
        .get(source.url.clone())
        .header(USER_AGENT, "sing-box")
        .send()
        .await
        .map_err(|_| anyhow!("failed to fetch {} subscription", source.provider_name))?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "{} subscription server returned HTTP {}",
            source.provider_name,
            status
        );
    }
    let text = response.text().await.map_err(|_| {
        anyhow!(
            "failed to read {} subscription response",
            source.provider_name
        )
    })?;
    if text.trim().is_empty() {
        bail!("{} subscription response was empty", source.provider_name);
    }
    Ok(text)
}

fn subscription_source_requires_direct_fetch(source: &SubscriptionSource) -> bool {
    let provider = source.provider_name.to_ascii_lowercase();
    let host = source
        .url
        .host_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    provider.contains("airtcp") || host.contains("airtcp") || host.contains("mailrelay")
}

fn cache_entry_is_fresh(
    entry: &CachedSubscription,
    now_unix: u64,
    refresh_interval: Duration,
) -> bool {
    now_unix.saturating_sub(entry.fetched_at_unix) < refresh_interval.as_secs()
        && !entry.subscription_json.trim().is_empty()
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

fn hash_url(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn redact_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value) else {
        return value.to_string();
    };

    if !url.query_pairs().next().is_none() {
        let pairs = url
            .query_pairs()
            .map(|(key, value)| {
                if is_secret_query_key(&key) {
                    (key.into_owned(), "REDACTED".to_string())
                } else {
                    (key.into_owned(), value.into_owned())
                }
            })
            .collect::<Vec<_>>();
        url.set_query(None);
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in pairs {
                query.append_pair(&key, &value);
            }
        }
    }

    let path = url.path().to_string();
    if let Some(index) = path.find("/link/") {
        let prefix_len = index + "/link/".len();
        let after_token = path[prefix_len..]
            .find('/')
            .map(|relative| prefix_len + relative)
            .unwrap_or(path.len());
        let redacted_path = format!("{}REDACTED{}", &path[..prefix_len], &path[after_token..]);
        url.set_path(&redacted_path);
    }
    url.to_string()
}

fn is_secret_query_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "token" | "access_token" | "sub_token" | "key" | "auth"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CachedSubscription, backup_existing_config, cache_entry_is_fresh,
        parse_subscription_sources, redact_url, subscription_config_backup_path,
        subscription_source_requires_direct_fetch,
    };
    use std::fs;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_provider_named_subscription_urls() {
        let sources = parse_subscription_sources(
            r#"
            # local secrets file
            baobeiyun = https://example.com/api/subscribe?token=secret
            airtcp = https://spring.mailrelay.us/link/secret?singbox=1
            "#,
        )
        .expect("sources parse");

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].provider_name, "baobeiyun");
        assert_eq!(sources[1].provider_name, "airtcp");
        assert_eq!(
            sources[1].url.as_str(),
            "https://spring.mailrelay.us/link/secret?singbox=1"
        );
    }

    #[test]
    fn airtcp_subscription_sources_use_direct_fetch() {
        let sources = parse_subscription_sources(
            r#"
            airtcp = https://spring.mailrelay.us/link/secret?singbox=1
            other = https://example.com/api/subscribe?token=secret
            "#,
        )
        .expect("sources parse");

        assert!(subscription_source_requires_direct_fetch(&sources[0]));
        assert!(!subscription_source_requires_direct_fetch(&sources[1]));
    }

    #[test]
    fn rejects_duplicate_provider_names() {
        let error = parse_subscription_sources(
            r#"
            airtcp = https://example.com/a
            airtcp = https://example.com/b
            "#,
        )
        .expect_err("duplicate providers should fail");

        assert!(
            error
                .to_string()
                .contains("duplicate subscription provider name")
        );
    }

    #[test]
    fn fresh_cache_entry_skips_daily_fetch() {
        let entry = CachedSubscription {
            url_hash: "hash".to_string(),
            fetched_at_unix: 10_000,
            subscription_json: "{}".to_string(),
        };

        assert!(cache_entry_is_fresh(
            &entry,
            10_000 + 60 * 60,
            Duration::from_secs(24 * 60 * 60)
        ));
        assert!(!cache_entry_is_fresh(
            &entry,
            10_000 + 24 * 60 * 60,
            Duration::from_secs(24 * 60 * 60)
        ));
    }

    #[test]
    fn redacts_subscription_tokens() {
        assert_eq!(
            redact_url("https://example.com/api/subscribe?token=secret&singbox=1"),
            "https://example.com/api/subscribe?token=REDACTED&singbox=1"
        );
        assert_eq!(
            redact_url("https://spring.mailrelay.us/link/secret?singbox=1"),
            "https://spring.mailrelay.us/link/REDACTED?singbox=1"
        );
    }

    #[test]
    fn backup_path_uses_special_single_sidecar_name() {
        let path = std::path::PathBuf::from("/tmp/config.json");

        assert_eq!(
            subscription_config_backup_path(&path),
            std::path::PathBuf::from("/tmp/config.json.sing-box-tui-subscription-backup")
        );
    }

    #[test]
    fn backup_existing_config_replaces_prior_backup() {
        let dir = temp_dir("subscription-backup");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config = dir.join("config.json");
        let backup = subscription_config_backup_path(&config);
        fs::write(&config, "old config\n").expect("write config");
        fs::write(&backup, "stale backup\n").expect("write stale backup");

        let backup_path = backup_existing_config(&config)
            .expect("backup succeeds")
            .expect("backup path");

        assert_eq!(backup_path, backup);
        assert_eq!(
            fs::read_to_string(&backup).expect("read backup"),
            "old config\n"
        );

        fs::write(&config, "new config\n").expect("write updated config");
        let backup_path = backup_existing_config(&config)
            .expect("second backup succeeds")
            .expect("backup path");

        assert_eq!(backup_path, backup);
        assert_eq!(
            fs::read_to_string(&backup).expect("read backup"),
            "new config\n"
        );
        assert_eq!(
            fs::read_dir(&dir)
                .expect("read temp dir")
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("backup"))
                .count(),
            1
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backup_existing_config_returns_none_when_config_is_absent() {
        let dir = temp_dir("subscription-no-backup");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config = dir.join("config.json");

        assert!(
            backup_existing_config(&config)
                .expect("missing config backup succeeds")
                .is_none()
        );
        assert!(!subscription_config_backup_path(&config).exists());

        let _ = fs::remove_dir_all(dir);
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-{label}-{nanos}"))
    }
}

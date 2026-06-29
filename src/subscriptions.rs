use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Url;
use reqwest::header::USER_AGENT;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::runtime::Builder as TokioRuntimeBuilder;

use crate::config::{
    DefaultConfigOptions, ProviderNodeSet, build_full_config_with_provider_node_sets_and_options,
    ensure_bypass_rule_set_file_for_config,
};
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
    pub(crate) include_geosite_rules: bool,
    pub(crate) include_tun_mode: bool,
    pub(crate) force: bool,
    pub(crate) interval_days: u64,
}

pub(crate) fn run_subscription_refresh(
    input: &Path,
    cache_path: &Path,
    config_path: &Path,
    output: Option<&PathBuf>,
    replace_nodes: bool,
    include_geosite_rules: bool,
    include_tun_mode: bool,
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
        include_geosite_rules,
        include_tun_mode,
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

    let mut summaries = Vec::new();
    let mut cache_changed = false;
    let mut provider_node_sets = Vec::new();
    for item in resolved {
        let mut nodes =
            extract_mergeable_outbounds_from_singbox_subscription(&item.subscription_json)
                .with_context(|| {
                    format!(
                        "failed to parse sing-box subscription JSON for {}",
                        item.source.provider_name
                    )
                })?;
        if subscription_source_strips_flag_emoji(&item.source) {
            strip_flag_emoji_from_node_tags(&mut nodes);
        }
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
        provider_node_sets.push(ProviderNodeRefresh {
            provider_name: item.source.provider_name,
            nodes,
        });
    }

    let refreshed_config = if request.config_path.exists() {
        let mut config = read_existing_config(&request.config_path)?;
        refresh_provider_node_outbounds_only(
            &mut config,
            provider_node_sets,
            request.replace_nodes,
        )?;
        config
    } else {
        build_full_config_with_provider_node_sets_and_options(
            &request.config_path,
            provider_node_sets
                .into_iter()
                .map(|provider| ProviderNodeSet {
                    provider_name: provider.provider_name,
                    nodes: provider.nodes,
                })
                .collect(),
            request.replace_nodes,
            DefaultConfigOptions {
                include_geosite_rules: request.include_geosite_rules,
                include_tun_mode: request.include_tun_mode,
            },
        )?
    };

    if cache_changed {
        cache_store.save(&cache)?;
    }
    let backup_path = backup_existing_config(&request.merged_path)?;
    fs::write(
        &request.merged_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&refreshed_config)
                .context("failed to serialize refreshed subscription config")?
        ),
    )
    .with_context(|| format!("failed to write {}", request.merged_path.display()))?;
    ensure_bypass_rule_set_file_for_config(&request.merged_path)?;

    Ok(SubscriptionRefreshOutput {
        input_path: request.input.display().to_string(),
        cache_path: request.cache_path.display().to_string(),
        interval_days: request.interval_days,
        merged_config_path: request.merged_path.display().to_string(),
        backup_config_path: backup_path.map(|path| path.display().to_string()),
        providers: summaries,
    })
}

fn read_existing_config(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read existing config {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

#[cfg(test)]
fn refresh_node_outbounds_only(
    config: &mut Value,
    refreshed_nodes: Vec<Value>,
    replace_nodes: bool,
) -> Result<()> {
    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    let outbounds = root
        .get_mut("outbounds")
        .and_then(Value::as_array_mut)
        .context("existing config outbounds must be an array")?;

    if replace_nodes {
        outbounds.retain(|outbound| !is_refreshable_node_outbound(outbound));
    }

    for node in refreshed_nodes {
        let tag = node
            .get("tag")
            .and_then(Value::as_str)
            .context("refreshed node outbound is missing a tag")?
            .to_string();
        if let Some(existing) = outbounds
            .iter_mut()
            .find(|outbound| outbound.get("tag").and_then(Value::as_str) == Some(tag.as_str()))
        {
            if is_refreshable_node_outbound(existing) {
                *existing = node;
            }
        } else {
            outbounds.push(node);
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ProviderNodeRefresh {
    provider_name: String,
    nodes: Vec<Value>,
}

fn refresh_provider_node_outbounds_only(
    config: &mut Value,
    provider_node_sets: Vec<ProviderNodeRefresh>,
    replace_nodes: bool,
) -> Result<()> {
    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    let outbounds = root
        .get_mut("outbounds")
        .and_then(Value::as_array_mut)
        .context("existing config outbounds must be an array")?;

    for provider in provider_node_sets {
        let node_tags = collect_node_tags(&provider.nodes)?;
        let old_provider_node_tags = provider_selector_members(outbounds, &provider.provider_name);
        let node_tag_set = node_tags
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        outbounds.retain(|outbound| {
            let Some(tag) = outbound.get("tag").and_then(Value::as_str) else {
                return true;
            };
            if !is_refreshable_node_outbound(outbound) || !old_provider_node_tags.contains(tag) {
                return true;
            }
            !replace_nodes && node_tag_set.contains(tag)
        });
        update_root_selector_for_provider(
            outbounds,
            &provider.provider_name,
            &old_provider_node_tags,
            &node_tags,
        );
        upsert_node_outbounds(outbounds, provider.nodes)?;
        upsert_provider_selector(outbounds, &provider.provider_name, &node_tags);
    }
    Ok(())
}

fn is_refreshable_node_outbound(outbound: &Value) -> bool {
    let Some(outbound_type) = outbound.get("type").and_then(Value::as_str) else {
        return false;
    };
    !matches!(
        outbound_type,
        "selector" | "urltest" | "direct" | "block" | "dns"
    )
}

fn collect_node_tags(nodes: &[Value]) -> Result<Vec<String>> {
    nodes
        .iter()
        .map(|node| {
            node.get("tag")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .context("refreshed node outbound is missing a tag")
        })
        .collect()
}

fn subscription_source_strips_flag_emoji(source: &SubscriptionSource) -> bool {
    let provider = source.provider_name.to_ascii_lowercase();
    source.provider_name.contains("白嫖")
        || provider.contains("baipiao")
        || source
            .url
            .host_str()
            .is_some_and(|host| host.contains("xn--mesv7f5toqlp"))
}

fn strip_flag_emoji_from_node_tags(nodes: &mut [Value]) {
    for node in nodes {
        let Some(object) = node.as_object_mut() else {
            continue;
        };
        let Some(tag) = object.get("tag").and_then(Value::as_str) else {
            continue;
        };
        let stripped = strip_regional_indicator_symbols(tag);
        if stripped != tag {
            object.insert("tag".to_string(), Value::String(stripped));
        }
    }
}

fn strip_regional_indicator_symbols(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !('\u{1F1E6}'..='\u{1F1FF}').contains(ch))
        .collect::<String>()
        .trim()
        .to_string()
}

fn upsert_node_outbounds(outbounds: &mut Vec<Value>, nodes: Vec<Value>) -> Result<()> {
    for node in nodes {
        let tag = node
            .get("tag")
            .and_then(Value::as_str)
            .context("refreshed node outbound is missing a tag")?
            .to_string();
        if let Some(existing) = outbounds
            .iter_mut()
            .find(|outbound| outbound.get("tag").and_then(Value::as_str) == Some(tag.as_str()))
        {
            if is_refreshable_node_outbound(existing) {
                *existing = node;
            }
        } else {
            outbounds.push(node);
        }
    }
    Ok(())
}

fn provider_selector_members(outbounds: &[Value], provider_name: &str) -> BTreeSet<String> {
    outbounds
        .iter()
        .find(|outbound| {
            outbound.get("type").and_then(Value::as_str) == Some("selector")
                && outbound.get("tag").and_then(Value::as_str) == Some(provider_name)
        })
        .and_then(|outbound| outbound.get("outbounds").and_then(Value::as_array))
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn upsert_provider_selector(outbounds: &mut Vec<Value>, provider_name: &str, node_tags: &[String]) {
    if let Some(selector) = outbounds.iter_mut().find(|outbound| {
        outbound.get("type").and_then(Value::as_str) == Some("selector")
            && outbound.get("tag").and_then(Value::as_str) == Some(provider_name)
    }) {
        set_selector_members(selector, node_tags);
        return;
    };

    outbounds.push(json!({
        "type": "selector",
        "tag": provider_name,
        "outbounds": node_tags,
        "default": node_tags.first().cloned().unwrap_or_default(),
        "interrupt_exist_connections": true
    }));
}

fn update_root_selector_for_provider(
    outbounds: &mut [Value],
    provider_name: &str,
    old_provider_node_tags: &BTreeSet<String>,
    node_tags: &[String],
) {
    let Some(root_index) = find_root_selector_index(outbounds) else {
        return;
    };
    let selector_tags = outbounds
        .iter()
        .filter(|outbound| outbound.get("type").and_then(Value::as_str) == Some("selector"))
        .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    let Some(selector) = outbounds.get_mut(root_index) else {
        return;
    };
    let Some(object) = selector.as_object_mut() else {
        return;
    };
    let Some(members) = object.get_mut("outbounds").and_then(Value::as_array_mut) else {
        return;
    };

    let new_node_tags = node_tags
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut insert_index = None;
    let mut next_members = Vec::new();
    for member in members.iter() {
        let Some(tag) = member.as_str() else {
            next_members.push(member.clone());
            continue;
        };
        if tag == provider_name
            || old_provider_node_tags.contains(tag)
            || new_node_tags.contains(tag)
        {
            insert_index.get_or_insert(next_members.len());
            continue;
        }
        next_members.push(member.clone());
    }
    let index =
        insert_index.unwrap_or_else(|| provider_insert_index(&next_members, &selector_tags));
    next_members.insert(index, Value::String(provider_name.to_string()));
    *members = next_members;
}

fn find_root_selector_index(outbounds: &[Value]) -> Option<usize> {
    ["手动选择", "select"].iter().find_map(|tag| {
        outbounds.iter().position(|outbound| {
            outbound.get("type").and_then(Value::as_str) == Some("selector")
                && outbound.get("tag").and_then(Value::as_str) == Some(*tag)
        })
    })
}

fn provider_insert_index(members: &[Value], selector_tags: &BTreeSet<String>) -> usize {
    members
        .iter()
        .position(|member| {
            let Some(tag) = member.as_str() else {
                return true;
            };
            !matches!(tag, "自动选择" | "auto" | "国内直连" | "direct")
                && !selector_tags.contains(tag)
        })
        .unwrap_or(members.len())
}

fn set_selector_members(selector: &mut Value, node_tags: &[String]) {
    let Some(object) = selector.as_object_mut() else {
        return;
    };
    object.insert(
        "outbounds".to_string(),
        Value::Array(node_tags.iter().cloned().map(Value::String).collect()),
    );
    if let Some(default) = object.get("default").and_then(Value::as_str)
        && node_tags.iter().any(|tag| tag == default)
    {
        return;
    }
    if let Some(first) = node_tags.first() {
        object.insert("default".to_string(), Value::String(first.clone()));
    } else {
        object.remove("default");
    }
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
        CachedSubscription, ProviderNodeRefresh, backup_existing_config, cache_entry_is_fresh,
        parse_subscription_sources, redact_url, refresh_node_outbounds_only,
        refresh_provider_node_outbounds_only, strip_flag_emoji_from_node_tags,
        subscription_config_backup_path, subscription_source_requires_direct_fetch,
        subscription_source_strips_flag_emoji,
    };
    use crate::defaults::default_clash_api_external_controller;
    use std::fs;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn subscription_refresh_updates_only_server_node_outbounds() {
        let original = serde_json::json!({
            "log": {
                "level": "debug"
            },
            "dns": {
                "servers": [{"tag": "local", "address": "https://dns.example/dns-query"}]
            },
            "inbounds": [{
                "type": "tun",
                "tag": "local-tun",
                "sniff": true
            }],
            "outbounds": [{
                "type": "selector",
                "tag": "手动选择",
                "outbounds": ["自动选择", "国内直连", "node-a"],
                "default": "自动选择",
                "interrupt_exist_connections": true
            }, {
                "type": "urltest",
                "tag": "自动选择",
                "outbounds": ["node-a"],
                "interval": "5m"
            }, {
                "type": "direct",
                "tag": "国内直连"
            }, {
                "type": "trojan",
                "tag": "node-a",
                "server": "old.example",
                "server_port": 443,
                "password": "old-secret"
            }],
            "route": {
                "rules": [{
                    "domain_suffix": ["local.example"],
                    "outbound": "国内直连"
                }]
            },
            "experimental": {
                "cache_file": {
                    "enabled": false
                },
                "clash_api": {
                    "external_controller": default_clash_api_external_controller()
                }
            }
        });
        let mut config = original.clone();

        refresh_node_outbounds_only(
            &mut config,
            vec![
                serde_json::json!({
                    "type": "trojan",
                    "tag": "node-a",
                    "server": "new.example",
                    "server_port": 8443,
                    "password": "new-secret"
                }),
                serde_json::json!({
                    "type": "selector",
                    "tag": "手动选择",
                    "outbounds": ["should-not-replace-selector"]
                }),
                serde_json::json!({
                    "type": "vless",
                    "tag": "node-b",
                    "server": "added.example",
                    "server_port": 443,
                    "uuid": "abc"
                }),
            ],
            false,
        )
        .expect("node-only refresh succeeds");

        assert_eq!(config["log"], original["log"]);
        assert_eq!(config["dns"], original["dns"]);
        assert_eq!(config["inbounds"], original["inbounds"]);
        assert_eq!(config["route"], original["route"]);
        assert_eq!(config["experimental"], original["experimental"]);
        assert_eq!(config["outbounds"][0], original["outbounds"][0]);
        assert_eq!(config["outbounds"][1], original["outbounds"][1]);
        assert_eq!(config["outbounds"][2], original["outbounds"][2]);
        assert_eq!(
            config["outbounds"][3],
            serde_json::json!({
                "type": "trojan",
                "tag": "node-a",
                "server": "new.example",
                "server_port": 8443,
                "password": "new-secret"
            })
        );
        assert_eq!(
            config["outbounds"][4],
            serde_json::json!({
                "type": "vless",
                "tag": "node-b",
                "server": "added.example",
                "server_port": 443,
                "uuid": "abc"
            })
        );
    }

    #[test]
    fn subscription_refresh_updates_nodes_by_provider_selector() {
        let original = serde_json::json!({
            "dns": {
                "servers": [{"tag": "local", "address": "https://dns.example/dns-query"}]
            },
            "outbounds": [{
                "type": "selector",
                "tag": "手动选择",
                "outbounds": ["宝贝云", "白嫖机场", "local-node"],
                "default": "宝贝云"
            }, {
                "type": "selector",
                "tag": "宝贝云",
                "outbounds": ["bby-old", "bby-stale"],
                "default": "bby-stale"
            }, {
                "type": "selector",
                "tag": "白嫖机场",
                "outbounds": ["bp-old"],
                "default": "bp-old"
            }, {
                "type": "trojan",
                "tag": "bby-old",
                "server": "old-bby.example",
                "server_port": 443,
                "password": "old"
            }, {
                "type": "trojan",
                "tag": "bby-stale",
                "server": "stale-bby.example",
                "server_port": 443,
                "password": "stale"
            }, {
                "type": "trojan",
                "tag": "bp-old",
                "server": "old-bp.example",
                "server_port": 443,
                "password": "bp"
            }, {
                "type": "trojan",
                "tag": "local-node",
                "server": "local.example",
                "server_port": 443,
                "password": "local"
            }],
            "route": {
                "rules": [{"domain_suffix": ["local.example"], "outbound": "local-node"}]
            }
        });
        let mut config = original.clone();

        refresh_provider_node_outbounds_only(
            &mut config,
            vec![ProviderNodeRefresh {
                provider_name: "宝贝云".to_string(),
                nodes: vec![
                    serde_json::json!({
                        "type": "trojan",
                        "tag": "bby-old",
                        "server": "new-bby.example",
                        "server_port": 8443,
                        "password": "new"
                    }),
                    serde_json::json!({
                        "type": "vless",
                        "tag": "bby-new",
                        "server": "new-node.example",
                        "server_port": 443,
                        "uuid": "abc"
                    }),
                ],
            }],
            false,
        )
        .expect("provider refresh succeeds");

        assert_eq!(config["dns"], original["dns"]);
        assert_eq!(config["route"], original["route"]);
        assert_eq!(config["outbounds"][0], original["outbounds"][0]);
        assert_eq!(config["outbounds"][2], original["outbounds"][2]);
        assert_eq!(
            config["outbounds"][1],
            serde_json::json!({
                "type": "selector",
                "tag": "宝贝云",
                "outbounds": ["bby-old", "bby-new"],
                "default": "bby-old"
            })
        );
        assert!(
            config["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .all(|outbound| outbound["tag"] != "bby-stale")
        );
        assert!(
            config["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .any(|outbound| outbound
                    == &serde_json::json!({
                        "type": "trojan",
                        "tag": "bby-old",
                        "server": "new-bby.example",
                        "server_port": 8443,
                        "password": "new"
                    }))
        );
        assert_eq!(
            config["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .find(|outbound| outbound["tag"] == "bp-old")
                .expect("other provider node is preserved"),
            &original["outbounds"][5]
        );
        assert_eq!(
            config["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .find(|outbound| outbound["tag"] == "local-node")
                .expect("local node is preserved"),
            &original["outbounds"][6]
        );
    }

    #[test]
    fn subscription_refresh_creates_provider_selector_from_flat_nodes() {
        let original = serde_json::json!({
            "dns": {
                "servers": [{"tag": "local", "address": "https://dns.example/dns-query"}]
            },
            "outbounds": [{
                "type": "selector",
                "tag": "手动选择",
                "outbounds": ["自动选择", "bby-old", "bp-old", "local-node"],
                "default": "自动选择"
            }, {
                "type": "urltest",
                "tag": "自动选择",
                "outbounds": ["bby-old", "bp-old", "local-node"]
            }, {
                "type": "trojan",
                "tag": "bby-old",
                "server": "old-bby.example",
                "server_port": 443,
                "password": "old"
            }, {
                "type": "trojan",
                "tag": "bp-old",
                "server": "old-bp.example",
                "server_port": 443,
                "password": "bp"
            }, {
                "type": "trojan",
                "tag": "local-node",
                "server": "local.example",
                "server_port": 443,
                "password": "local"
            }]
        });
        let mut config = original.clone();

        refresh_provider_node_outbounds_only(
            &mut config,
            vec![ProviderNodeRefresh {
                provider_name: "宝贝云".to_string(),
                nodes: vec![
                    serde_json::json!({
                        "type": "trojan",
                        "tag": "bby-old",
                        "server": "new-bby.example",
                        "server_port": 8443,
                        "password": "new"
                    }),
                    serde_json::json!({
                        "type": "vless",
                        "tag": "bby-new",
                        "server": "new-node.example",
                        "server_port": 443,
                        "uuid": "abc"
                    }),
                ],
            }],
            false,
        )
        .expect("provider refresh succeeds");

        assert_eq!(config["dns"], original["dns"]);
        assert_eq!(
            config["outbounds"][0],
            serde_json::json!({
                "type": "selector",
                "tag": "手动选择",
                "outbounds": ["自动选择", "宝贝云", "bp-old", "local-node"],
                "default": "自动选择"
            })
        );
        assert_eq!(config["outbounds"][1], original["outbounds"][1]);
        assert_eq!(
            config["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .find(|outbound| outbound["tag"] == "宝贝云")
                .expect("provider selector is created"),
            &serde_json::json!({
                "type": "selector",
                "tag": "宝贝云",
                "outbounds": ["bby-old", "bby-new"],
                "default": "bby-old",
                "interrupt_exist_connections": true
            })
        );
        assert_eq!(
            config["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .find(|outbound| outbound["tag"] == "bby-old")
                .expect("provider node updated"),
            &serde_json::json!({
                "type": "trojan",
                "tag": "bby-old",
                "server": "new-bby.example",
                "server_port": 8443,
                "password": "new"
            })
        );
        assert_eq!(
            config["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .find(|outbound| outbound["tag"] == "bp-old")
                .expect("other provider flat node is preserved"),
            &original["outbounds"][3]
        );
    }

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
    fn baipiao_subscription_sources_strip_flag_emoji() {
        let sources = parse_subscription_sources(
            r#"
            白嫖机场 = https://yes.xn--mesv7f5toqlp.biz/api/subscribe?token=secret
            other = https://example.com/api/subscribe?token=secret
            "#,
        )
        .expect("sources parse");

        assert!(subscription_source_strips_flag_emoji(&sources[0]));
        assert!(!subscription_source_strips_flag_emoji(&sources[1]));
    }

    #[test]
    fn strips_country_flag_emoji_from_baipiao_node_tags() {
        let mut nodes = vec![
            serde_json::json!({
                "type": "trojan",
                "tag": "🇺🇸美国HY1轻量",
                "server": "us.example",
                "server_port": 443,
                "password": "secret"
            }),
            serde_json::json!({
                "type": "hysteria2",
                "tag": "🇯🇵日本Trojan2",
                "server": "jp.example",
                "server_port": 443,
                "password": "secret"
            }),
        ];

        strip_flag_emoji_from_node_tags(&mut nodes);

        assert_eq!(nodes[0]["tag"], "美国HY1轻量");
        assert_eq!(nodes[1]["tag"], "日本Trojan2");
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

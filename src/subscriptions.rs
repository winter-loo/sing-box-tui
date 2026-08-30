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

use crate::atomic_file::write_atomic;
use crate::config::{
    DefaultConfigOptions, ProviderNodeSet, build_full_config_with_provider_node_sets_and_options,
    ensure_bypass_rule_set_file_for_config, resolved_bypass_rule_set_path_for_config,
};
use crate::config_mutation::{
    commit_active_node_config, lock_config_mutation_for, paths_refer_to_same_target,
};
use crate::import::extract_mergeable_outbounds_from_singbox_subscription;
use crate::node_quality_path::{
    default_benchmark_db_path_for_config, ensure_active_config_paths_are_distinct,
};

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
    pub(crate) node_quality_db_path: PathBuf,
    pub(crate) replace_nodes: bool,
    pub(crate) include_geosite_rules: bool,
    pub(crate) include_tun_mode: bool,
    pub(crate) force: bool,
    pub(crate) interval_days: u64,
}

pub(crate) struct SubscriptionRefreshOptions {
    pub(crate) input: PathBuf,
    pub(crate) cache_path: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) output: Option<PathBuf>,
    pub(crate) replace_nodes: bool,
    pub(crate) include_geosite_rules: bool,
    pub(crate) include_tun_mode: bool,
    pub(crate) write: bool,
    pub(crate) force: bool,
    pub(crate) interval_days: u64,
}

pub(crate) fn run_subscription_refresh(options: SubscriptionRefreshOptions) -> Result<()> {
    let SubscriptionRefreshOptions {
        input,
        cache_path,
        config_path,
        output,
        replace_nodes,
        include_geosite_rules,
        include_tun_mode,
        write,
        force,
        interval_days,
    } = options;
    let config_path_buf = config_path;
    let merged_path = if let Some(path) = output {
        path
    } else if write {
        config_path_buf.clone()
    } else {
        bail!("subscriptions requires either --output <FILE> or --write");
    };
    let node_quality_db_path = default_benchmark_db_path_for_config(&config_path_buf)?;
    let report = refresh_subscriptions(&SubscriptionRefreshRequest {
        input,
        cache_path,
        config_path: config_path_buf,
        merged_path,
        node_quality_db_path,
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
    validate_subscription_refresh_paths(request)?;
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

    let no_provider_fetch_failed = resolved
        .iter()
        .all(|item| !matches!(item.status, SubscriptionFetchStatus::StaleCache));
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

    if cache_changed {
        cache_store.save(&cache)?;
    }
    let commit = commit_subscription_config_and_quality(
        request,
        provider_node_sets,
        no_provider_fetch_failed,
    )?;

    Ok(SubscriptionRefreshOutput {
        input_path: request.input.display().to_string(),
        cache_path: request.cache_path.display().to_string(),
        interval_days: request.interval_days,
        merged_config_path: request.merged_path.display().to_string(),
        backup_config_path: commit.backup_path.map(|path| path.display().to_string()),
        config_updated: commit.config_updated,
        node_history_reconciled: commit.node_history_reconciled,
        node_history_changed: commit.node_history_changed,
        node_quality_generation: commit.node_quality_generation,
        providers: summaries,
    })
}

pub(crate) fn validate_subscription_refresh_paths(
    request: &SubscriptionRefreshRequest,
) -> Result<()> {
    let writes_active_config =
        paths_refer_to_same_target(&request.config_path, &request.merged_path)?;
    if writes_active_config {
        // The writer intentionally preserves an active-config symlink by replacing the path
        // named by `merged_path`, so its backup and prerequisite live beside that actual entry.
        let backup_path = subscription_config_backup_path(&request.merged_path);
        let bypass_path = resolved_bypass_rule_set_path_for_config(&request.merged_path)?;
        ensure_active_config_paths_are_distinct(
            &request.config_path,
            &request.node_quality_db_path,
            &[
                ("subscription source", request.input.as_path()),
                ("subscription cache", request.cache_path.as_path()),
                ("subscription backup", backup_path.as_path()),
                ("bypass rule-set", bypass_path.as_path()),
            ],
        )
    } else {
        let backup_path = subscription_config_backup_path(&request.merged_path);
        let bypass_path = resolved_bypass_rule_set_path_for_config(&request.merged_path)?;
        ensure_active_config_paths_are_distinct(
            &request.config_path,
            &request.node_quality_db_path,
            &[
                ("subscription source", request.input.as_path()),
                ("subscription cache", request.cache_path.as_path()),
                ("merged subscription output", request.merged_path.as_path()),
                ("subscription output backup", backup_path.as_path()),
                ("bypass rule-set", bypass_path.as_path()),
            ],
        )
    }
}

struct SubscriptionConfigCommit {
    backup_path: Option<PathBuf>,
    config_updated: bool,
    node_history_reconciled: bool,
    node_history_changed: bool,
    node_quality_generation: Option<u64>,
}

impl std::fmt::Debug for SubscriptionConfigCommit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubscriptionConfigCommit")
            .field("backup_path", &self.backup_path)
            .field("config_updated", &self.config_updated)
            .field("node_history_reconciled", &self.node_history_reconciled)
            .field("node_history_changed", &self.node_history_changed)
            .field("node_quality_generation", &self.node_quality_generation)
            .finish()
    }
}

fn commit_subscription_config_and_quality(
    request: &SubscriptionRefreshRequest,
    provider_node_sets: Vec<ProviderNodeRefresh>,
    no_provider_fetch_failed: bool,
) -> Result<SubscriptionConfigCommit> {
    let writes_active_config =
        paths_refer_to_same_target(&request.config_path, &request.merged_path)?;
    if !no_provider_fetch_failed && writes_active_config {
        return Ok(unchanged_subscription_commit());
    }
    if !writes_active_config {
        return merge_and_commit_subscription_config(request, provider_node_sets);
    }

    let active = commit_active_node_config(
        &request.merged_path,
        &request.node_quality_db_path,
        || build_subscription_config(request, provider_node_sets),
        || ensure_bypass_rule_set_file_for_config(&request.merged_path).map(|_| ()),
        || backup_existing_config(&request.merged_path),
    )?;
    Ok(subscription_commit_from_active(active))
}

#[cfg(test)]
fn commit_subscription_config_as_independent_process(
    request: &SubscriptionRefreshRequest,
    provider_node_sets: Vec<ProviderNodeRefresh>,
    before_reconcile: impl FnOnce(),
    after_commit_before_marker_cleanup: impl FnOnce(),
) -> Result<SubscriptionConfigCommit> {
    let active = crate::config_mutation::commit_active_node_config_as_independent_process(
        &request.merged_path,
        &request.node_quality_db_path,
        || build_subscription_config(request, provider_node_sets),
        || ensure_bypass_rule_set_file_for_config(&request.merged_path).map(|_| ()),
        || backup_existing_config(&request.merged_path),
        before_reconcile,
        after_commit_before_marker_cleanup,
    )?;
    Ok(subscription_commit_from_active(active))
}

fn unchanged_subscription_commit() -> SubscriptionConfigCommit {
    SubscriptionConfigCommit {
        backup_path: None,
        config_updated: false,
        node_history_reconciled: false,
        node_history_changed: false,
        node_quality_generation: None,
    }
}

fn subscription_commit_from_active(
    active: crate::config_mutation::ActiveNodeConfigCommit,
) -> SubscriptionConfigCommit {
    SubscriptionConfigCommit {
        backup_path: active.backup_path,
        config_updated: true,
        node_history_reconciled: true,
        node_history_changed: active.reconciliation.identities_changed,
        node_quality_generation: Some(active.reconciliation.generation),
    }
}

fn merge_and_commit_subscription_config(
    request: &SubscriptionRefreshRequest,
    provider_node_sets: Vec<ProviderNodeRefresh>,
) -> Result<SubscriptionConfigCommit> {
    let _config_guard = lock_config_mutation_for(&request.config_path)?;
    ensure_bypass_rule_set_file_for_config(&request.merged_path)?;
    merge_and_commit_subscription_config_unlocked(request, provider_node_sets)
}

fn merge_and_commit_subscription_config_unlocked(
    request: &SubscriptionRefreshRequest,
    provider_node_sets: Vec<ProviderNodeRefresh>,
) -> Result<SubscriptionConfigCommit> {
    let refreshed_config = build_subscription_config(request, provider_node_sets)?;

    let contents = serde_json::to_string_pretty(&refreshed_config)
        .context("failed to serialize refreshed subscription config")?;
    let backup_path = backup_existing_config(&request.merged_path)?;
    write_atomic(&request.merged_path, format!("{contents}\n").as_bytes())
        .with_context(|| format!("failed to write {}", request.merged_path.display()))?;
    Ok(SubscriptionConfigCommit {
        backup_path,
        config_updated: true,
        node_history_reconciled: false,
        node_history_changed: false,
        node_quality_generation: None,
    })
}

fn build_subscription_config(
    request: &SubscriptionRefreshRequest,
    provider_node_sets: Vec<ProviderNodeRefresh>,
) -> Result<Value> {
    if request.config_path.exists() {
        let mut config = read_existing_config(&request.config_path)?;
        refresh_provider_node_outbounds_only(
            &mut config,
            provider_node_sets,
            request.replace_nodes,
        )?;
        Ok(config)
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
        )
    }
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
        if node_tags.is_empty() {
            remove_provider_selector(outbounds, &provider.provider_name);
        } else {
            upsert_provider_selector(outbounds, &provider.provider_name, &node_tags);
        }
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

fn remove_provider_selector(outbounds: &mut Vec<Value>, provider_name: &str) {
    outbounds.retain(|outbound| {
        !(outbound.get("type").and_then(Value::as_str) == Some("selector")
            && outbound.get("tag").and_then(Value::as_str) == Some(provider_name))
    });
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
    if node_tags.is_empty() {
        *members = next_members;
        ensure_selector_default_is_member(selector);
        return;
    }
    let index =
        insert_index.unwrap_or_else(|| provider_insert_index(&next_members, &selector_tags));
    next_members.insert(index, Value::String(provider_name.to_string()));
    *members = next_members;
    ensure_selector_default_is_member(selector);
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

fn ensure_selector_default_is_member(selector: &mut Value) {
    let Some(object) = selector.as_object_mut() else {
        return;
    };
    let member_tags = object
        .get("outbounds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    if let Some(default) = object.get("default").and_then(Value::as_str)
        && member_tags.iter().any(|tag| tag == default)
    {
        return;
    }
    if let Some(first) = member_tags.first() {
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
    pub(crate) config_updated: bool,
    pub(crate) node_history_reconciled: bool,
    pub(crate) node_history_changed: bool,
    pub(crate) node_quality_generation: Option<u64>,
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
    let mut file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("config.json"))
        .to_os_string();
    file_name.push(format!(".{SUBSCRIPTION_CONFIG_BACKUP_SUFFIX}"));
    path.with_file_name(file_name)
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

    if url.query_pairs().next().is_some() {
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
        CachedSubscription, ProviderNodeRefresh, SubscriptionCache, SubscriptionCacheStore,
        SubscriptionRefreshRequest, backup_existing_config, cache_entry_is_fresh,
        commit_subscription_config_and_quality, commit_subscription_config_as_independent_process,
        hash_url, merge_and_commit_subscription_config, parse_subscription_sources, redact_url,
        refresh_node_outbounds_only, refresh_provider_node_outbounds_only, refresh_subscriptions,
        strip_flag_emoji_from_node_tags, subscription_config_backup_path,
        subscription_source_requires_direct_fetch, subscription_source_strips_flag_emoji, unix_now,
        validate_subscription_refresh_paths,
    };
    use crate::atomic_file::DurableAtomicWriteError;
    use crate::benchmark_workflow::BenchmarkWorkflow;
    use crate::config::{set_internet_tun_mode, set_internet_tun_mode_after_read_for_test};
    use crate::config_mutation::{
        commit_active_node_config, commit_active_node_config_with_writer_for_test,
        lock_config_mutation_for, paths_refer_to_same_target,
    };
    use crate::controller::{NodeReachabilityAssessment, ProbeOutcome};
    use crate::defaults::{DEFAULT_BYPASS_RULE_SET_PATH, default_clash_api_external_controller};
    use crate::storage::BenchmarkStore;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn provider_config(nodes: Vec<Value>) -> Value {
        let tags = nodes
            .iter()
            .map(|node| node["tag"].as_str().expect("node tag").to_string())
            .collect::<Vec<_>>();
        let mut outbounds = vec![
            serde_json::json!({
                "type": "selector",
                "tag": "select",
                "outbounds": ["provider-a", "direct"],
                "default": "provider-a"
            }),
            serde_json::json!({
                "type": "selector",
                "tag": "provider-a",
                "outbounds": tags,
                "default": tags.first().cloned()
            }),
            serde_json::json!({"type": "direct", "tag": "direct"}),
        ];
        outbounds.extend(nodes);
        serde_json::json!({"outbounds": outbounds})
    }

    fn request_for(
        dir: &std::path::Path,
        config_path: std::path::PathBuf,
        merged_path: std::path::PathBuf,
    ) -> SubscriptionRefreshRequest {
        SubscriptionRefreshRequest {
            input: dir.join("subscriptions.txt"),
            cache_path: dir.join("cache.json"),
            config_path,
            merged_path,
            node_quality_db_path: dir.join("quality.sqlite3"),
            replace_nodes: false,
            include_geosite_rules: false,
            include_tun_mode: false,
            force: true,
            interval_days: 1,
        }
    }

    fn seed_quality_history(db_path: &std::path::Path, config: &Value, nodes: &[&str]) {
        let store = BenchmarkStore::open(db_path).expect("open quality store");
        store
            .reconcile_node_history(config)
            .expect("seed node identities");
        for (index, node) in nodes.iter().enumerate() {
            store
                .record_reachability_assessment(
                    "select",
                    &NodeReachabilityAssessment::from_attempts(
                        (*node).to_string(),
                        vec![
                            ProbeOutcome::Reachable {
                                delay_ms: 40 + index as u64,
                            },
                            ProbeOutcome::Reachable {
                                delay_ms: 41 + index as u64,
                            },
                            ProbeOutcome::Reachable {
                                delay_ms: 42 + index as u64,
                            },
                        ],
                    ),
                )
                .expect("seed node history");
        }
    }

    fn history_nodes(db_path: &std::path::Path) -> Vec<String> {
        BenchmarkStore::open(db_path)
            .expect("reopen quality store")
            .latest_reachability_assessments()
            .expect("read quality history")
            .into_iter()
            .map(|(_, assessment)| assessment.name)
            .collect()
    }

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
    fn subscription_refresh_drops_empty_provider_selector() {
        let mut config = serde_json::json!({
            "outbounds": [{
                "type": "selector",
                "tag": "手动选择",
                "outbounds": ["自动选择", "airtcp", "local-node"],
                "default": "手ZZ动选择"
            }, {
                "type": "urltest",
                "tag": "自动选择",
                "outbounds": ["airtcp-old", "local-node"]
            }, {
                "type": "selector",
                "tag": "airtcp",
                "outbounds": ["airtcp-old"],
                "default": "airtcp-old"
            }, {
                "type": "trojan",
                "tag": "airtcp-old",
                "server": "old-airtcp.example",
                "server_port": 443,
                "password": "old"
            }, {
                "type": "trojan",
                "tag": "local-node",
                "server": "local.example",
                "server_port": 443,
                "password": "local"
            }]
        });

        refresh_provider_node_outbounds_only(
            &mut config,
            vec![ProviderNodeRefresh {
                provider_name: "airtcp".to_string(),
                nodes: Vec::new(),
            }],
            false,
        )
        .expect("provider refresh succeeds");

        let outbounds = config["outbounds"].as_array().expect("outbounds");
        assert!(!outbounds.iter().any(|outbound| outbound["tag"] == "airtcp"));
        assert!(
            !outbounds
                .iter()
                .any(|outbound| outbound["tag"] == "airtcp-old")
        );
        assert!(
            outbounds
                .iter()
                .any(|outbound| outbound["tag"] == "local-node")
        );

        let root = outbounds
            .iter()
            .find(|outbound| outbound["tag"] == "手动选择")
            .expect("root selector");
        let root_members = root["outbounds"].as_array().expect("root members");
        assert!(!root_members.contains(&Value::String("airtcp".to_string())));
        assert!(!root_members.contains(&Value::String("airtcp-old".to_string())));
        assert_eq!(root["default"], "自动选择");
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

    #[cfg(unix)]
    #[test]
    fn non_utf8_config_names_keep_distinct_backup_sidecars() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let dir = temp_dir("subscription-non-utf8-backups");
        fs::create_dir_all(&dir).expect("create temp dir");
        let first = dir.join(std::ffi::OsString::from_vec(vec![b'a', 0xff]));
        let second = dir.join(std::ffi::OsString::from_vec(vec![b'b', 0xfe]));
        let first_backup = subscription_config_backup_path(&first);
        let second_backup = subscription_config_backup_path(&second);

        assert_ne!(first_backup, second_backup);
        assert!(
            first_backup
                .file_name()
                .expect("first backup name")
                .as_bytes()
                .starts_with(&[b'a', 0xff])
        );
        assert!(
            second_backup
                .file_name()
                .expect("second backup name")
                .as_bytes()
                .starts_with(&[b'b', 0xfe])
        );
        let _ = fs::remove_dir_all(dir);
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

    #[test]
    fn subscription_path_aliases_fail_before_source_cache_or_output_io() {
        for role in ["input-cache", "cache", "merged", "backup", "bypass"] {
            let dir = temp_dir(&format!("subscription-path-preflight-{role}"));
            fs::create_dir_all(&dir).expect("create temp dir");
            let config = dir.join("config.json");
            fs::write(&config, b"{}\n").expect("write active config");
            let mut request = request_for(&dir, config.clone(), config.clone());
            let canary = match role {
                "input-cache" => {
                    request.input = dir.join("source-and-cache.txt");
                    request.cache_path = request.input.clone();
                    request.input.clone()
                }
                "cache" => {
                    request.cache_path = dir.join("quality.sqlite3-shm");
                    request.cache_path.clone()
                }
                "merged" => {
                    request.merged_path = dir.join("quality.sqlite3-wal");
                    request.merged_path.clone()
                }
                "backup" => {
                    request.merged_path = dir.join("preview.json");
                    request.node_quality_db_path =
                        subscription_config_backup_path(&request.merged_path);
                    request.node_quality_db_path.clone()
                }
                "bypass" => {
                    request.node_quality_db_path = dir.join(DEFAULT_BYPASS_RULE_SET_PATH);
                    request.node_quality_db_path.clone()
                }
                _ => unreachable!(),
            };
            fs::write(&request.input, b"https://127.0.0.1:9/must-not-fetch\n")
                .expect("write source canary");
            if canary != request.input {
                fs::write(&canary, b"protected canary\n").expect("write path canary");
            }
            let before = fs::read(&canary).expect("read canary before validation");

            let error = refresh_subscriptions(&request)
                .expect_err("path alias must fail before subscription I/O");
            assert!(
                format!("{error:#}").contains("must not alias"),
                "{role}: {error:#}"
            );
            assert_eq!(fs::read(&canary).expect("read preserved canary"), before);
            assert!(validate_subscription_refresh_paths(&request).is_err());
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn active_cross_directory_alias_validates_the_actual_backup_and_resolved_bypass() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("subscription-cross-directory-preflight");
        let real_dir = root.join("real");
        let alias_dir = root.join("alias");
        fs::create_dir_all(&real_dir).expect("create real config directory");
        fs::create_dir_all(&alias_dir).expect("create alias config directory");
        let config = real_dir.join("config.json");
        let alias = alias_dir.join("config.json");
        fs::write(&config, b"{}\n").expect("write active config");
        symlink("../real/config.json", &alias).expect("create active config alias");

        let backup = subscription_config_backup_path(&alias);
        let bypass = crate::config::resolved_bypass_rule_set_path_for_config(&alias)
            .expect("resolve actual bypass prerequisite");
        fs::write(&backup, b"backup-canary\n").expect("write backup canary");
        fs::write(&bypass, b"bypass-canary\n").expect("write bypass canary");
        let request = SubscriptionRefreshRequest {
            input: backup.clone(),
            cache_path: bypass.clone(),
            config_path: config,
            merged_path: alias,
            node_quality_db_path: real_dir.join("quality.sqlite3"),
            replace_nodes: false,
            include_geosite_rules: false,
            include_tun_mode: false,
            force: true,
            interval_days: 7,
        };

        let error = refresh_subscriptions(&request)
            .expect_err("actual writer aliases must fail before source or cache I/O");
        assert!(format!("{error:#}").contains("must not alias"));
        assert_eq!(
            fs::read(&backup).expect("read backup canary"),
            b"backup-canary\n"
        );
        assert_eq!(
            fs::read(&bypass).expect("read bypass canary"),
            b"bypass-canary\n"
        );
        assert!(!alias_dir.join(DEFAULT_BYPASS_RULE_SET_PATH).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn successful_active_refresh_reconciles_history_after_the_config_commit() {
        let dir = temp_dir("subscription-history-success");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![
            serde_json::json!({
                "type":"trojan", "tag":"unchanged", "server":"same.example",
                "server_port":443, "password":"secret-a"
            }),
            serde_json::json!({
                "type":"trojan", "tag":"removed", "server":"old.example",
                "server_port":443, "password":"secret-b"
            }),
        ]);
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&old_config).expect("serialize old config"),
        )
        .expect("write old config");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        seed_quality_history(
            &request.node_quality_db_path,
            &old_config,
            &["unchanged", "removed"],
        );

        let commit = commit_subscription_config_and_quality(
            &request,
            vec![ProviderNodeRefresh {
                provider_name: "provider-a".to_string(),
                nodes: vec![
                    serde_json::json!({
                        "password":"secret-a", "server_port":443, "server":"same.example",
                        "tag":"unchanged", "type":"trojan"
                    }),
                    serde_json::json!({
                        "type":"vless", "tag":"added", "server":"new.example",
                        "server_port":443, "uuid":"secret-c"
                    }),
                ],
            }],
            true,
        )
        .expect("active refresh commits and reconciles");

        let diagnostic = format!("{commit:?}");
        assert!(!diagnostic.contains("secret-a"));
        assert!(!diagnostic.contains("secret-c"));
        assert!(commit.node_history_reconciled);
        assert!(commit.node_history_changed);
        assert!(commit.node_quality_generation.is_some());
        let committed: Value =
            serde_json::from_str(&fs::read_to_string(&config_path).expect("read committed config"))
                .expect("parse committed config");
        // #16 gates every quality read behind the same runtime fence as writes. The unchanged
        // row is retained in SQLite, but it must not become visible until the new config has been
        // observed by the managed runtime.
        assert!(history_nodes(&request.node_quality_db_path).is_empty());
        BenchmarkStore::open(&request.node_quality_db_path)
            .expect("reopen fenced quality store")
            .reconcile_node_history(&committed)
            .expect("simulate managed runtime loading the committed config");
        assert_eq!(
            history_nodes(&request.node_quality_db_path),
            vec!["unchanged"]
        );
        assert!(
            committed["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .any(|outbound| outbound["tag"] == "added")
        );
        assert!(
            !committed["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .any(|outbound| outbound["tag"] == "removed")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn startup_binding_preserves_new_facts_across_an_unchanged_active_refresh() {
        let dir = temp_dir("subscription-startup-binding-unchanged");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"same.example",
            "server_port":443, "password":"same-secret"
        })]);
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("serialize active config"),
        )
        .expect("write active config");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        let mut workflow = BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".to_string(),
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("test client"),
            &config_path,
            &request.node_quality_db_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        )
        .expect("startup binds committed active config");
        workflow
            .confirm_managed_runtime_reload(&config_path, &request.node_quality_db_path, || {
                Ok(crate::benchmark_workflow::ManagedRuntimeObservation::new(
                    (),
                    &config_path,
                    "http://127.0.0.1:9992",
                    Some(std::process::id()),
                ))
            })
            .expect("observed startup runtime enables quality persistence");
        assert_eq!(
            workflow
                .persist_reachability_for_test("node-a")
                .expect("write startup benchmark"),
            Some(true)
        );
        let startup_store =
            BenchmarkStore::open(&request.node_quality_db_path).expect("open startup-bound store");
        let generation = startup_store.quality_generation();
        assert!(
            startup_store
                .record_reachability_assessment(
                    "select",
                    &NodeReachabilityAssessment::from_attempts(
                        "node-a".to_string(),
                        vec![
                            ProbeOutcome::Reachable { delay_ms: 40 },
                            ProbeOutcome::Reachable { delay_ms: 41 },
                            ProbeOutcome::Reachable { delay_ms: 42 },
                        ],
                    ),
                )
                .expect("write startup reachability fact")
        );
        drop(startup_store);

        let commit = commit_subscription_config_and_quality(
            &request,
            vec![ProviderNodeRefresh {
                provider_name: "provider-a".to_string(),
                nodes: vec![serde_json::json!({
                    "password":"same-secret", "server_port":443,
                    "server":"same.example", "tag":"node-a", "type":"trojan"
                })],
            }],
            true,
        )
        .expect("commit unchanged active refresh");

        assert!(commit.node_history_reconciled);
        assert!(!commit.node_history_changed);
        assert_eq!(commit.node_quality_generation, Some(generation));
        assert_eq!(history_nodes(&request.node_quality_db_path), vec!["node-a"]);
        assert_eq!(
            workflow
                .persist_reachability_for_test("node-a")
                .expect("unchanged generation remains writable"),
            Some(true)
        );

        drop(workflow);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn startup_binding_and_independent_subscription_refresh_share_the_config_lock() {
        let dir = temp_dir("subscription-startup-binding-concurrency");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&old_config).expect("serialize old config"),
        )
        .expect("write old config");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        let loaded_runtime_store = BenchmarkStore::open(&request.node_quality_db_path)
            .expect("open quality store for the already loaded runtime");
        // This fixture represents a controller already serving `old_config`; the startup under
        // test therefore rebinds an unchanged generation and may install its store immediately.
        loaded_runtime_store
            .reconcile_node_history(&old_config)
            .expect("bind identities for the already loaded runtime");
        drop(loaded_runtime_store);

        let startup_config = config_path.clone();
        let startup_database = request.node_quality_db_path.clone();
        let (startup_locked_tx, startup_locked_rx) = mpsc::channel();
        let (startup_release_tx, startup_release_rx) = mpsc::channel();
        let (startup_opened_tx, startup_opened_rx) = mpsc::channel();
        let (attempt_write_tx, attempt_write_rx) = mpsc::channel();
        let (old_write_tx, old_write_rx) = mpsc::channel();
        let startup = thread::spawn(move || {
            let workflow = BenchmarkWorkflow::open_with_binding_hook_for_test(
                "http://127.0.0.1:9992".to_string(),
                reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .expect("test client"),
                &startup_config,
                &startup_database,
                crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
                || {
                    startup_locked_tx
                        .send(())
                        .expect("report startup config snapshot");
                    startup_release_rx
                        .recv()
                        .expect("wait before binding startup snapshot");
                },
            )
            .expect("bind startup snapshot");
            startup_opened_tx.send(()).expect("report startup open");
            attempt_write_rx
                .recv()
                .expect("wait until subscription refresh commits");
            old_write_tx
                .send(workflow.persist_reachability_for_test("node-a"))
                .expect("report old-generation write");
        });
        startup_locked_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("startup holds canonical config lock");

        let refresh_request = request.clone();
        let (refresh_done_tx, refresh_done_rx) = mpsc::channel();
        let refresh = thread::spawn(move || {
            refresh_done_tx
                .send(commit_subscription_config_as_independent_process(
                    &refresh_request,
                    vec![ProviderNodeRefresh {
                        provider_name: "provider-a".to_string(),
                        nodes: vec![serde_json::json!({
                            "type":"trojan", "tag":"node-a", "server":"new.example",
                            "server_port":443, "password":"new-secret"
                        })],
                    }],
                    || {},
                    || {},
                ))
                .expect("report independent refresh");
        });
        assert!(
            matches!(
                refresh_done_rx.recv_timeout(Duration::from_millis(100)),
                Err(RecvTimeoutError::Timeout)
            ),
            "independent refresh must wait until startup identity binding releases the config lock"
        );

        startup_release_tx.send(()).expect("release startup binder");
        startup_opened_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("startup binding finishes");
        let refresh_commit = refresh_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("refresh resumes after startup binding")
            .expect("independent refresh commits");
        assert!(refresh_commit.node_history_changed);
        attempt_write_tx
            .send(())
            .expect("attempt stale startup writer");
        assert_eq!(
            old_write_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("old writer returns")
                .expect("old writer checks generation"),
            Some(false)
        );
        startup.join().expect("startup worker exits");
        refresh.join().expect("refresh worker exits");

        let committed: Value =
            serde_json::from_str(&fs::read_to_string(&config_path).expect("read final config"))
                .expect("parse final config");
        assert!(
            committed["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .any(|outbound| {
                    outbound["tag"] == "node-a" && outbound["server"] == "new.example"
                })
        );
        let identities = BenchmarkStore::open(&request.node_quality_db_path)
            .expect("reopen final store")
            .stored_node_identities()
            .expect("read final identities");
        let expected_path = dir.join("expected-identities.sqlite3");
        let expected_store =
            BenchmarkStore::open(&expected_path).expect("open expected identity store");
        expected_store
            .reconcile_node_history(&committed)
            .expect("fingerprint final committed config");
        assert_eq!(
            identities,
            expected_store
                .stored_node_identities()
                .expect("read expected final identities"),
            "the quality DB must bind the exact config snapshot committed by the later writer"
        );
        drop(expected_store);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn output_only_refresh_does_not_reconcile_active_history() {
        let dir = temp_dir("subscription-history-output-only");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("active.json");
        let merged_path = dir.join("preview.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"secret-a"
        })]);
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&old_config).expect("serialize old config"),
        )
        .expect("write active config");
        let request = request_for(&dir, config_path.clone(), merged_path.clone());
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);

        commit_subscription_config_and_quality(
            &request,
            vec![ProviderNodeRefresh {
                provider_name: "provider-a".to_string(),
                nodes: vec![serde_json::json!({
                    "type":"trojan", "tag":"node-a", "server":"new.example",
                    "server_port":443, "password":"secret-b"
                })],
            }],
            true,
        )
        .expect("preview config writes");

        assert_eq!(history_nodes(&request.node_quality_db_path), vec!["node-a"]);
        let active_after_preview = serde_json::from_str::<Value>(
            &fs::read_to_string(&config_path).expect("read active config"),
        )
        .expect("parse active config");
        assert!(
            active_after_preview == old_config,
            "writing a preview must not mutate the active config"
        );
        let preview: Value =
            serde_json::from_str(&fs::read_to_string(&merged_path).expect("read preview config"))
                .expect("parse preview config");
        assert!(preview.to_string().contains("new.example"));

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_output_reconciles_when_it_resolves_to_the_active_config() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("subscription-history-same-symlink-target");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("active.json");
        let merged_path = dir.join("active-link.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"secret-a"
        })]);
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&old_config).expect("serialize old config"),
        )
        .expect("write active config");
        symlink(
            config_path.file_name().expect("active config file name"),
            &merged_path,
        )
        .expect("link alternate output path to active config");
        let request = request_for(&dir, config_path.clone(), merged_path.clone());
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);

        let commit = commit_subscription_config_and_quality(
            &request,
            vec![ProviderNodeRefresh {
                provider_name: "provider-a".to_string(),
                nodes: vec![serde_json::json!({
                    "type":"trojan", "tag":"node-a", "server":"new.example",
                    "server_port":443, "password":"secret-b"
                })],
            }],
            true,
        )
        .expect("symlinked active refresh succeeds");

        assert!(commit.node_history_reconciled);
        assert!(history_nodes(&request.node_quality_db_path).is_empty());
        assert!(
            fs::symlink_metadata(&merged_path)
                .expect("inspect output alias")
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::read_to_string(&config_path)
                .expect("read active config")
                .contains("new.example")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_cache_fallback_after_download_failure_preserves_all_history() {
        let dir = temp_dir("subscription-history-stale-cache");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-old", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        let old_text = serde_json::to_string_pretty(&old_config).expect("serialize old config");
        fs::write(&config_path, &old_text).expect("write active config");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-old"]);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve local port");
        let subscription_url = format!(
            "http://{}/subscription?token=download-secret",
            listener.local_addr().expect("read local address")
        );
        drop(listener);
        fs::write(&request.input, format!("provider-a = {subscription_url}\n"))
            .expect("write subscription source");
        SubscriptionCacheStore::new(&request.cache_path)
            .save(&SubscriptionCache {
                entries: BTreeMap::from([(
                    "provider-a".to_string(),
                    CachedSubscription {
                        url_hash: hash_url(&subscription_url),
                        fetched_at_unix: 1,
                        subscription_json: serde_json::json!({
                            "outbounds": [{
                                "type":"trojan", "tag":"node-new", "server":"cached.example",
                                "server_port":443, "password":"cached-secret"
                            }]
                        })
                        .to_string(),
                    },
                )]),
            })
            .expect("write stale cache");

        let report = refresh_subscriptions(&request).expect("stale cache fallback remains usable");

        assert_eq!(report.providers[0].status, "stale-cache");
        assert!(!report.config_updated);
        assert_eq!(
            history_nodes(&request.node_quality_db_path),
            vec!["node-old"]
        );
        assert!(
            fs::read_to_string(&config_path).expect("read active config") == old_text,
            "a stale-cache fallback must leave an existing active config byte-for-byte unchanged"
        );
        let report_text = serde_json::to_string(&report).expect("serialize report");
        assert!(!report_text.contains("download-secret"));
        assert!(!report_text.contains("cached-secret"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_cache_failure_does_not_create_a_missing_active_config_or_rebind_old_facts() {
        let dir = temp_dir("subscription-history-stale-cache-missing-active");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);
        let old_identities = BenchmarkStore::open(&request.node_quality_db_path)
            .expect("open old quality store")
            .stored_node_identities()
            .expect("read old identities");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve local port");
        let subscription_url = format!(
            "http://{}/subscription?token=download-secret",
            listener.local_addr().expect("read local address")
        );
        drop(listener);
        fs::write(&request.input, format!("provider-a = {subscription_url}\n"))
            .expect("write subscription source");
        SubscriptionCacheStore::new(&request.cache_path)
            .save(&SubscriptionCache {
                entries: BTreeMap::from([(
                    "provider-a".to_string(),
                    CachedSubscription {
                        url_hash: hash_url(&subscription_url),
                        fetched_at_unix: 1,
                        subscription_json: serde_json::json!({
                            "outbounds": [{
                                "type":"trojan", "tag":"node-a", "server":"cached-new.example",
                                "server_port":443, "password":"cached-secret"
                            }]
                        })
                        .to_string(),
                    },
                )]),
            })
            .expect("write stale cache");

        let report = refresh_subscriptions(&request).expect("stale cache report remains available");

        assert_eq!(report.providers[0].status, "stale-cache");
        assert!(!report.config_updated);
        assert!(
            !config_path.exists(),
            "a failed download must not create an active config from stale cache"
        );
        assert_eq!(history_nodes(&request.node_quality_db_path), vec!["node-a"]);
        assert_eq!(
            BenchmarkStore::open(&request.node_quality_db_path)
                .expect("reopen old quality store")
                .stored_node_identities()
                .expect("read preserved identities"),
            old_identities
        );
        let report_text = serde_json::to_string(&report).expect("serialize report");
        assert!(!report_text.contains("download-secret"));
        assert!(!report_text.contains("cached-secret"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fresh_cache_refresh_reconciles_config_drift() {
        let dir = temp_dir("subscription-history-fresh-cache");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"manually-drifted.example",
            "server_port":443, "password":"old-secret"
        })]);
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&old_config).expect("serialize old config"),
        )
        .expect("write active config");
        let mut request = request_for(&dir, config_path.clone(), config_path.clone());
        request.force = false;
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);
        let subscription_url = "https://example.invalid/subscription?token=cache-secret";
        fs::write(&request.input, format!("provider-a = {subscription_url}\n"))
            .expect("write subscription source");
        SubscriptionCacheStore::new(&request.cache_path)
            .save(&SubscriptionCache {
                entries: BTreeMap::from([(
                    "provider-a".to_string(),
                    CachedSubscription {
                        url_hash: hash_url(subscription_url),
                        fetched_at_unix: unix_now().expect("read current time"),
                        subscription_json: serde_json::json!({
                            "outbounds": [{
                                "type":"trojan", "tag":"node-a", "server":"cached.example",
                                "server_port":443, "password":"cached-secret"
                            }]
                        })
                        .to_string(),
                    },
                )]),
            })
            .expect("write fresh cache");

        let report = refresh_subscriptions(&request).expect("fresh cache refresh succeeds");

        assert_eq!(report.providers[0].status, "cached");
        assert!(report.config_updated);
        assert!(report.node_history_reconciled);
        assert!(report.node_history_changed);
        assert!(history_nodes(&request.node_quality_db_path).is_empty());
        let committed = fs::read_to_string(&config_path).expect("read refreshed config");
        assert!(committed.contains("cached.example"));
        assert!(
            !serde_json::to_string(&report)
                .expect("serialize report")
                .contains("cache-secret")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn subscription_parse_failure_preserves_config_and_history() {
        let dir = temp_dir("subscription-history-parse-failure");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        let old_text = serde_json::to_string_pretty(&old_config).expect("serialize old config");
        fs::write(&config_path, &old_text).expect("write active config");
        let mut request = request_for(&dir, config_path.clone(), config_path.clone());
        request.force = false;
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);
        let subscription_url = "https://example.invalid/subscription?token=parse-secret";
        fs::write(&request.input, format!("provider-a = {subscription_url}\n"))
            .expect("write subscription source");
        SubscriptionCacheStore::new(&request.cache_path)
            .save(&SubscriptionCache {
                entries: BTreeMap::from([(
                    "provider-a".to_string(),
                    CachedSubscription {
                        url_hash: hash_url(subscription_url),
                        fetched_at_unix: unix_now().expect("read current time"),
                        subscription_json: "{not valid subscription json".to_string(),
                    },
                )]),
            })
            .expect("write invalid cached subscription");

        let error = refresh_subscriptions(&request).expect_err("subscription parse must fail");

        assert!(!format!("{error:#}").contains("parse-secret"));
        assert!(
            fs::read_to_string(&config_path).expect("read config") == old_text,
            "parse failure must leave the active config byte-for-byte unchanged"
        );
        assert_eq!(history_nodes(&request.node_quality_db_path), vec!["node-a"]);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn active_pre_rename_write_failure_preserves_config_identities_and_all_facts() {
        let dir = temp_dir("subscription-history-write-failure");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        let old_bytes = serde_json::to_vec_pretty(&old_config).expect("serialize old config");
        fs::write(&config_path, &old_bytes).expect("write active config");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);
        let before =
            BenchmarkStore::open(&request.node_quality_db_path).expect("open seeded quality store");
        let old_identities = before
            .stored_node_identities()
            .expect("read seeded identities");
        let old_assessments = before
            .latest_reachability_assessments()
            .expect("read seeded reachability assessments");
        drop(before);
        let new_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"new.example",
            "server_port":443, "password":"new-secret"
        })]);

        let error = commit_active_node_config_with_writer_for_test(
            &config_path,
            &request.node_quality_db_path,
            || Ok(new_config),
            || Ok(()),
            || Ok(None),
            |_, _| {
                Err(DurableAtomicWriteError::DestinationUnchanged(
                    anyhow::anyhow!("injected pre-rename write failure"),
                ))
            },
        )
        .expect_err("pre-rename failure must abort the active transaction");

        assert!(!format!("{error:#}").contains("new-secret"));
        assert!(format!("{error:#}").contains("injected pre-rename write failure"));
        assert_eq!(
            fs::read(&config_path).expect("read preserved config"),
            old_bytes
        );
        let after = BenchmarkStore::open(&request.node_quality_db_path)
            .expect("reopen preserved quality store");
        assert_eq!(
            after
                .stored_node_identities()
                .expect("read preserved identities"),
            old_identities
        );
        assert_eq!(
            after
                .latest_reachability_assessments()
                .expect("read preserved reachability assessments"),
            old_assessments
        );
        drop(after);
        assert_eq!(history_nodes(&request.node_quality_db_path), vec!["node-a"]);
        let mut marker = request.node_quality_db_path.as_os_str().to_os_string();
        marker.push(".node-quality-writes-blocked");
        assert!(!std::path::PathBuf::from(marker).exists());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backup_failure_preserves_config_and_history() {
        let dir = temp_dir("subscription-history-backup-failure");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        let old_text = serde_json::to_string_pretty(&old_config).expect("serialize old config");
        fs::write(&config_path, &old_text).expect("write active config");
        fs::create_dir(subscription_config_backup_path(&config_path))
            .expect("occupy backup path with a directory");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);

        merge_and_commit_subscription_config(
            &request,
            vec![ProviderNodeRefresh {
                provider_name: "provider-a".to_string(),
                nodes: vec![serde_json::json!({
                    "type":"trojan", "tag":"node-a", "server":"new.example",
                    "server_port":443, "password":"new-secret"
                })],
            }],
        )
        .expect_err("backup path collision must fail before the config write");

        assert!(
            fs::read_to_string(&config_path).expect("read config") == old_text,
            "backup failure must leave the active config byte-for-byte unchanged"
        );
        assert_eq!(history_nodes(&request.node_quality_db_path), vec!["node-a"]);

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn required_rule_set_failure_preserves_config_and_history() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("subscription-history-rule-set-failure");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        let old_text = serde_json::to_string_pretty(&old_config).expect("serialize old config");
        fs::write(&config_path, &old_text).expect("write active config");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);
        symlink(
            dir.join("missing-directory").join("rules.json"),
            dir.join(DEFAULT_BYPASS_RULE_SET_PATH),
        )
        .expect("create dangling rule-set symlink");

        let error = commit_subscription_config_and_quality(
            &request,
            vec![ProviderNodeRefresh {
                provider_name: "provider-a".to_string(),
                nodes: vec![serde_json::json!({
                    "type":"trojan", "tag":"node-a", "server":"new.example",
                    "server_port":443, "password":"new-secret"
                })],
            }],
            true,
        )
        .expect_err("required rule-set write must fail");

        assert!(!format!("{error:#}").contains("new-secret"));
        assert!(
            fs::read_to_string(&config_path).expect("read active config") == old_text,
            "a required rule-set failure must leave the active config byte-for-byte unchanged"
        );
        assert_eq!(history_nodes(&request.node_quality_db_path), vec!["node-a"]);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reconciliation_failure_restores_config_and_rolls_back_identity_and_history() {
        let dir = temp_dir("subscription-history-reconcile-rollback");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        let old_text = serde_json::to_string_pretty(&old_config).expect("serialize old config");
        fs::write(&config_path, &old_text).expect("write active config");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);
        let before_identities = BenchmarkStore::open(&request.node_quality_db_path)
            .expect("open quality store")
            .stored_node_identities()
            .expect("read old identities");
        let invalid_config = provider_config(vec![
            serde_json::json!({
                "type":"trojan", "tag":"node-a", "server":"new.example",
                "server_port":443, "password":"new-secret"
            }),
            serde_json::json!({
                "type":"trojan", "tag":"node-a", "server":"duplicate.example",
                "server_port":443, "password":"duplicate-secret"
            }),
        ]);
        let error = commit_active_node_config(
            &config_path,
            &request.node_quality_db_path,
            || Ok(invalid_config),
            || Ok(()),
            || Ok(None),
        )
        .expect_err("duplicate identity must fail after the config write");

        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("duplicate node tag node-a"));
        assert!(!diagnostic.contains("new-secret"));
        assert!(!diagnostic.contains("duplicate-secret"));
        assert!(
            fs::read_to_string(&config_path).expect("read restored config") == old_text,
            "reconciliation failure must atomically restore the active config"
        );
        assert_eq!(history_nodes(&request.node_quality_db_path), vec!["node-a"]);
        let recovered_store =
            BenchmarkStore::open(&request.node_quality_db_path).expect("reopen quality store");
        assert_eq!(
            recovered_store
                .stored_node_identities()
                .expect("read restored identities"),
            before_identities
        );
        assert!(
            recovered_store
                .record_reachability_assessment(
                    "select",
                    &NodeReachabilityAssessment::from_attempts(
                        "node-a".to_string(),
                        vec![
                            ProbeOutcome::Reachable { delay_ms: 31 },
                            ProbeOutcome::Reachable { delay_ms: 32 },
                            ProbeOutcome::Reachable { delay_ms: 33 },
                        ],
                    ),
                )
                .expect("write after complete rollback"),
            "a complete config and DB rollback must clear this attempt's write block"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reconciliation_failure_removes_a_config_created_by_this_attempt() {
        let dir = temp_dir("subscription-new-config-reconcile-rollback");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        let old_identity = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-old", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        seed_quality_history(&request.node_quality_db_path, &old_identity, &["node-old"]);
        let invalid_config = provider_config(vec![
            serde_json::json!({
                "type":"trojan", "tag":"node-new", "server":"new.example",
                "server_port":443, "password":"new-secret"
            }),
            serde_json::json!({
                "type":"trojan", "tag":"node-new", "server":"duplicate.example",
                "server_port":443, "password":"duplicate-secret"
            }),
        ]);
        let error = commit_active_node_config(
            &config_path,
            &request.node_quality_db_path,
            || Ok(invalid_config),
            || Ok(()),
            || Ok(None),
        )
        .expect_err("duplicate identity must fail after creating the config");

        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("duplicate node tag node-new"));
        assert!(!diagnostic.contains("new-secret"));
        assert!(!diagnostic.contains("duplicate-secret"));
        assert!(
            !config_path.exists(),
            "a failed reconciliation must remove the config created by this attempt"
        );
        #[cfg(unix)]
        assert_eq!(
            history_nodes(&request.node_quality_db_path),
            vec!["node-old"]
        );
        #[cfg(not(unix))]
        {
            assert!(diagnostic.contains("quality reads and writes remain blocked"));
            assert!(history_nodes(&request.node_quality_db_path).is_empty());
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn quality_store_open_failure_preserves_active_config() {
        let dir = temp_dir("subscription-history-quality-open-failure");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        let old_text = serde_json::to_string_pretty(&old_config).expect("serialize old config");
        fs::write(&config_path, &old_text).expect("write active config");
        let mut request = request_for(&dir, config_path.clone(), config_path.clone());
        request.node_quality_db_path = dir.join("invalid-quality.sqlite3");
        fs::create_dir(&request.node_quality_db_path)
            .expect("occupy quality path with a directory");

        let error = commit_subscription_config_and_quality(
            &request,
            vec![ProviderNodeRefresh {
                provider_name: "provider-a".to_string(),
                nodes: vec![serde_json::json!({
                    "type":"trojan", "tag":"node-a", "server":"new.example",
                    "server_port":443, "password":"new-secret"
                })],
            }],
            true,
        )
        .expect_err("invalid quality store must fail before the active config write");

        assert!(!format!("{error:#}").contains("new-secret"));
        assert!(
            fs::read_to_string(&config_path).expect("read active config") == old_text,
            "a quality-store open failure must leave the active config byte-for-byte unchanged"
        );
        assert!(
            request.node_quality_db_path.is_dir(),
            "a failed quality-store open must preserve the existing path"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_active_target_is_compared_by_its_resolved_parent_and_file_name() {
        let dir = temp_dir("subscription-missing-target-identity");
        fs::create_dir_all(&dir).expect("create temp dir");
        let active = dir.join("new-config.json");

        assert!(paths_refer_to_same_target(&active, &active).expect("compare identical target"));
        assert!(
            !paths_refer_to_same_target(&active, &dir.join("preview.json"))
                .expect("compare distinct target")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn subscription_commit_waits_for_the_shared_config_mutation_lock() {
        let dir = temp_dir("subscription-mutation-lock");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "outbounds": [
                    {
                        "type": "selector",
                        "tag": "手动选择",
                        "outbounds": ["自动选择", "direct"]
                    },
                    { "type": "urltest", "tag": "自动选择", "outbounds": [] },
                    { "type": "direct", "tag": "direct" }
                ]
            }))
            .expect("serializes config"),
        )
        .expect("writes config");
        let request = SubscriptionRefreshRequest {
            input: dir.join("subscriptions.txt"),
            cache_path: dir.join("cache.json"),
            config_path: config_path.clone(),
            merged_path: config_path.clone(),
            node_quality_db_path: dir.join("quality.sqlite3"),
            replace_nodes: false,
            include_geosite_rules: false,
            include_tun_mode: false,
            force: true,
            interval_days: 1,
        };
        let providers = vec![ProviderNodeRefresh {
            provider_name: "provider-a".to_string(),
            nodes: vec![serde_json::json!({
                "type": "trojan",
                "tag": "node-a",
                "server": "node.example",
                "server_port": 443,
                "password": "secret"
            })],
        }];

        let guard = lock_config_mutation_for(&config_path).expect("lock active config");
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).expect("signals worker start");
            let result = merge_and_commit_subscription_config(&request, providers);
            finished_tx.send(result).expect("reports worker result");
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker starts");
        assert!(
            matches!(
                finished_rx.recv_timeout(Duration::from_millis(100)),
                Err(RecvTimeoutError::Timeout)
            ),
            "subscription config replacement must wait for an active config mutation"
        );
        drop(guard);

        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker resumes after releasing the mutation lock")
            .expect("subscription config commit succeeds");
        worker.join().expect("worker exits cleanly");
        let updated: Value = serde_json::from_str(
            &fs::read_to_string(&config_path).expect("reads committed config"),
        )
        .expect("parses committed config");
        assert!(
            updated["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .any(|outbound| outbound["tag"] == "node-a")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn route_writer_waits_for_reconciliation_and_preserves_refreshed_nodes() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("subscription-atomic-config-quality-commit");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&old_config).expect("serialize old config"),
        )
        .expect("write old config");
        let config_alias = dir.join("active-config-alias.json");
        symlink(
            config_path.file_name().expect("config file name"),
            &config_alias,
        )
        .expect("create route-writer alias");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);

        let (reconcile_reached_tx, reconcile_reached_rx) = mpsc::channel();
        let (reconcile_release_tx, reconcile_release_rx) = mpsc::channel();
        let (commit_tx, commit_rx) = mpsc::channel();
        let commit_worker = thread::spawn(move || {
            commit_tx
                .send(commit_subscription_config_as_independent_process(
                    &request,
                    vec![ProviderNodeRefresh {
                        provider_name: "provider-a".to_string(),
                        nodes: vec![serde_json::json!({
                            "type":"trojan", "tag":"node-a", "server":"new.example",
                            "server_port":443, "password":"new-secret"
                        })],
                    }],
                    || {
                        reconcile_reached_tx
                            .send(())
                            .expect("report reconciliation barrier");
                        reconcile_release_rx
                            .recv()
                            .expect("wait for reconciliation release");
                    },
                    || {},
                ))
                .expect("report subscription commit");
        });

        reconcile_reached_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("subscription writer reaches reconciliation barrier");
        assert!(
            fs::read_to_string(&config_path)
                .expect("read committed config")
                .contains("new.example"),
            "the subscription writer must reach the SQLite reconciliation barrier"
        );

        let (editor_tx, editor_rx) = mpsc::channel();
        let editor_path = config_alias.clone();
        let editor = thread::spawn(move || {
            editor_tx
                .send(set_internet_tun_mode(&editor_path, true, None))
                .expect("report route editor outcome");
        });
        assert!(
            matches!(
                editor_rx.recv_timeout(Duration::from_millis(100)),
                Err(RecvTimeoutError::Timeout)
            ),
            "the route writer must remain blocked until node reconciliation commits"
        );

        reconcile_release_tx
            .send(())
            .expect("release reconciliation barrier");
        commit_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("subscription commit completes after quality lock release")
            .expect("subscription commit succeeds");
        let route_update = editor_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("route writer enters after reconciliation")
            .expect("route writer succeeds");
        assert!(route_update.changed);
        commit_worker.join().expect("subscription writer exits");
        editor.join().expect("second editor exits");

        let final_config: Value =
            serde_json::from_str(&fs::read_to_string(&config_path).expect("read final config"))
                .expect("parse final config");
        assert!(
            final_config["outbounds"]
                .as_array()
                .expect("outbounds array")
                .iter()
                .any(|outbound| {
                    outbound["tag"] == "node-a" && outbound["server"] == "new.example"
                }),
            "the route writer must derive its edit from the subscription's committed nodes"
        );
        assert!(
            final_config["inbounds"]
                .as_array()
                .expect("inbounds array")
                .iter()
                .any(|inbound| inbound["tag"] == "tun-in")
        );
        assert!(
            fs::symlink_metadata(&config_alias)
                .expect("inspect route writer alias")
                .file_type()
                .is_symlink()
        );
        assert!(history_nodes(&dir.join("quality.sqlite3")).is_empty());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn subscription_waits_for_config_only_writer_and_reconciles_its_committed_snapshot() {
        let dir = temp_dir("config-writer-before-subscription");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&old_config).expect("serialize old config"),
        )
        .expect("write old config");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);

        let (route_read_tx, route_read_rx) = mpsc::channel();
        let (route_release_tx, route_release_rx) = mpsc::channel();
        let (route_done_tx, route_done_rx) = mpsc::channel();
        let route_path = config_path.clone();
        let route_writer = thread::spawn(move || {
            route_done_tx
                .send(set_internet_tun_mode_after_read_for_test(
                    &route_path,
                    true,
                    None,
                    || {
                        route_read_tx.send(()).expect("report route snapshot read");
                        route_release_rx.recv().expect("wait before route commit");
                    },
                ))
                .expect("report route writer outcome");
        });
        route_read_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("route writer reads the old config while holding its guard");

        let (subscription_done_tx, subscription_done_rx) = mpsc::channel();
        let subscription_writer = thread::spawn(move || {
            subscription_done_tx
                .send(commit_subscription_config_and_quality(
                    &request,
                    vec![ProviderNodeRefresh {
                        provider_name: "provider-a".to_string(),
                        nodes: vec![serde_json::json!({
                            "type":"trojan", "tag":"node-a", "server":"new.example",
                            "server_port":443, "password":"new-secret"
                        })],
                    }],
                    true,
                ))
                .expect("report subscription outcome");
        });
        assert!(
            matches!(
                subscription_done_rx.recv_timeout(Duration::from_millis(100)),
                Err(RecvTimeoutError::Timeout)
            ),
            "the subscription build must wait until the config-only writer commits its snapshot"
        );

        route_release_tx.send(()).expect("release route writer");
        route_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("route writer completes")
            .expect("route writer succeeds");
        subscription_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("subscription resumes after route commit")
            .expect("subscription commit succeeds");
        route_writer.join().expect("route writer exits");
        subscription_writer
            .join()
            .expect("subscription writer exits");

        let final_config: Value =
            serde_json::from_str(&fs::read_to_string(&config_path).expect("read final config"))
                .expect("parse final config");
        assert!(
            final_config["inbounds"]
                .as_array()
                .expect("inbounds array")
                .iter()
                .any(|inbound| inbound["tag"] == "tun-in")
        );
        assert!(
            final_config["outbounds"]
                .as_array()
                .expect("outbounds array")
                .iter()
                .any(|outbound| {
                    outbound["tag"] == "node-a" && outbound["server"] == "new.example"
                })
        );
        assert!(history_nodes(&dir.join("quality.sqlite3")).is_empty());
        let actual_identities = BenchmarkStore::open(dir.join("quality.sqlite3"))
            .expect("open reconciled quality store")
            .stored_node_identities()
            .expect("read reconciled identities");
        let expected_store = BenchmarkStore::open(dir.join("expected.sqlite3"))
            .expect("open expected identity store");
        expected_store
            .reconcile_node_history(&final_config)
            .expect("fingerprint final config");
        assert_eq!(
            actual_identities,
            expected_store
                .stored_node_identities()
                .expect("read expected identities"),
            "quality identity must fingerprint the config snapshot committed after the route edit"
        );

        drop(expected_store);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reconciliation_file_lock_spans_db_commit_and_marker_cleanup() {
        let dir = temp_dir("subscription-cross-process-serialization");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let old_config = provider_config(vec![serde_json::json!({
            "type":"trojan", "tag":"node-a", "server":"old.example",
            "server_port":443, "password":"old-secret"
        })]);
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&old_config).expect("serialize old config"),
        )
        .expect("write old config");
        let request = request_for(&dir, config_path.clone(), config_path.clone());
        seed_quality_history(&request.node_quality_db_path, &old_config, &["node-a"]);

        let first_request = request.clone();
        let (first_reached_tx, first_reached_rx) = mpsc::channel();
        let (first_release_tx, first_release_rx) = mpsc::channel();
        let (first_done_tx, first_done_rx) = mpsc::channel();
        let first = thread::spawn(move || {
            let result = commit_subscription_config_as_independent_process(
                &first_request,
                vec![ProviderNodeRefresh {
                    provider_name: "provider-a".to_string(),
                    nodes: vec![serde_json::json!({
                        "type":"trojan", "tag":"node-a", "server":"writer-a.example",
                        "server_port":443, "password":"writer-a-secret"
                    })],
                }],
                || {},
                || {
                    first_reached_tx
                        .send(())
                        .expect("report first post-commit cleanup barrier");
                    first_release_rx
                        .recv()
                        .expect("wait for first marker cleanup release");
                },
            );
            first_done_tx.send(result).expect("report first result");
        });

        first_reached_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first writer commits DB and pauses before marker cleanup");
        assert!(
            fs::read_to_string(&config_path)
                .expect("read first writer config")
                .contains("writer-a.example")
        );

        let second_request = request.clone();
        let (second_started_tx, second_started_rx) = mpsc::channel();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second = thread::spawn(move || {
            second_started_tx
                .send(())
                .expect("report second process attempt");
            let result = commit_subscription_config_as_independent_process(
                &second_request,
                vec![ProviderNodeRefresh {
                    provider_name: "provider-a".to_string(),
                    nodes: vec![serde_json::json!({
                        "type":"trojan", "tag":"node-a", "server":"writer-b.example",
                        "server_port":443, "password":"writer-b-secret"
                    })],
                }],
                || {},
                || {},
            );
            second_done_tx.send(result).expect("report second result");
        });
        second_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second process reaches reconciliation call");
        assert!(
            matches!(
                second_done_rx.recv_timeout(Duration::from_millis(100)),
                Err(RecvTimeoutError::Timeout)
            ),
            "the second process must not acquire reconciliation ownership before marker cleanup"
        );
        assert!(
            fs::read_to_string(&config_path)
                .expect("read config while first writer is blocked")
                .contains("writer-a.example")
        );

        first_release_tx
            .send(())
            .expect("release first marker cleanup");
        first_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first writer finishes")
            .expect("first writer succeeds");
        second_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second writer finishes")
            .expect("second writer succeeds");
        first.join().expect("first writer exits");
        second.join().expect("second writer exits");

        let final_config: Value =
            serde_json::from_str(&fs::read_to_string(&config_path).expect("read final config"))
                .expect("parse final config");
        let final_text = final_config.to_string();
        assert!(final_text.contains("writer-b.example"));
        assert!(!final_text.contains("writer-a.example"));
        let actual_store =
            BenchmarkStore::open(&request.node_quality_db_path).expect("open final quality store");
        let expected_store = BenchmarkStore::open(dir.join("expected.sqlite3"))
            .expect("open expected quality store");
        expected_store
            .reconcile_node_history(&final_config)
            .expect("fingerprint final config");
        assert_eq!(
            actual_store
                .stored_node_identities()
                .expect("read final identities"),
            expected_store
                .stored_node_identities()
                .expect("read expected identities")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn subscription_commit_preserves_a_symlinked_config_path() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("subscription-symlink");
        fs::create_dir_all(&dir).expect("create temp dir");
        let target = dir.join("real-config.json");
        let link = dir.join("config.json");
        fs::write(
            &target,
            serde_json::to_string_pretty(&serde_json::json!({
                "outbounds": [
                    {
                        "type": "selector",
                        "tag": "手动选择",
                        "outbounds": ["自动选择", "direct"]
                    },
                    { "type": "urltest", "tag": "自动选择", "outbounds": [] },
                    { "type": "direct", "tag": "direct" }
                ]
            }))
            .expect("serializes config"),
        )
        .expect("writes target config");
        symlink(target.file_name().expect("target has file name"), &link)
            .expect("creates config symlink");
        let request = SubscriptionRefreshRequest {
            input: dir.join("subscriptions.txt"),
            cache_path: dir.join("cache.json"),
            config_path: link.clone(),
            merged_path: link.clone(),
            node_quality_db_path: dir.join("quality.sqlite3"),
            replace_nodes: false,
            include_geosite_rules: false,
            include_tun_mode: false,
            force: true,
            interval_days: 1,
        };

        merge_and_commit_subscription_config(
            &request,
            vec![ProviderNodeRefresh {
                provider_name: "provider-a".to_string(),
                nodes: vec![serde_json::json!({
                    "type": "trojan",
                    "tag": "node-a",
                    "server": "node.example",
                    "server_port": 443,
                    "password": "secret"
                })],
            }],
        )
        .expect("subscription commit succeeds");

        assert!(
            fs::symlink_metadata(&link)
                .expect("reads link metadata")
                .file_type()
                .is_symlink()
        );
        let updated: Value =
            serde_json::from_str(&fs::read_to_string(&target).expect("reads target config"))
                .expect("parses target config");
        assert!(
            updated["outbounds"]
                .as_array()
                .expect("outbounds")
                .iter()
                .any(|outbound| outbound["tag"] == "node-a")
        );

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

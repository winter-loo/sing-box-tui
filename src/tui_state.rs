use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::defaults::DEFAULT_BYPASS_RULE_SET_PATH;

const DEFAULT_TUI_STATE_PATH: &str = "sing-box-tui.json";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TuiRuntimeState {
    #[serde(default)]
    pub(crate) benchmark_filter: String,
    #[serde(default)]
    pub(crate) auto_pick_enabled: bool,
    #[serde(default)]
    pub(crate) current_selected_nodes: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) bypass_entries: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct TuiStateStore {
    path: PathBuf,
}

impl TuiStateStore {
    pub(crate) fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub(crate) fn load(&self) -> Result<TuiRuntimeState> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(TuiRuntimeState::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", self.path.display()));
            }
        };
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", self.path.display()))
    }

    pub(crate) fn save(&self, state: &TuiRuntimeState) -> Result<()> {
        let text = serde_json::to_string_pretty(state).context("failed to encode TUI state")?;
        fs::write(&self.path, format!("{text}\n"))
            .with_context(|| format!("failed to write {}", self.path.display()))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BypassRuleSetStore {
    path: PathBuf,
}

impl BypassRuleSetStore {
    pub(crate) fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub(crate) fn save(&self, entries: &[String]) -> Result<()> {
        let value = bypass_rule_set_value(entries);
        let text =
            serde_json::to_string_pretty(&value).context("failed to encode bypass rule-set")?;
        fs::write(&self.path, format!("{text}\n"))
            .with_context(|| format!("failed to write {}", self.path.display()))
    }
}

pub(crate) fn default_tui_state_path() -> PathBuf {
    env::var("SING_BOX_TUI_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_TUI_STATE_PATH))
}

pub(crate) fn default_bypass_rule_set_path() -> PathBuf {
    env::var("SING_BOX_TUI_BYPASS_RULE_SET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_BYPASS_RULE_SET_PATH))
}

pub(crate) fn parse_bypass_entries(input: &str) -> Vec<String> {
    let mut entries = Vec::new();
    for item in input.split([',', '，', '\n', '\r', '\t', ' ']) {
        let Some(entry) = normalize_bypass_entry(item) else {
            continue;
        };
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries
}

fn bypass_rule_set_value(entries: &[String]) -> Value {
    let mut domains = Vec::new();
    let mut ip_cidrs = Vec::new();
    for entry in entries {
        if is_ip_entry(entry) {
            ip_cidrs.push(entry.clone());
        } else {
            domains.push(normalize_domain_suffix(entry));
        }
    }

    let mut rules = Vec::new();
    if !domains.is_empty() {
        rules.push(json!({ "domain_suffix": domains }));
    }
    if !ip_cidrs.is_empty() {
        rules.push(json!({ "ip_cidr": ip_cidrs }));
    }
    json!({
        "version": 1,
        "rules": rules,
    })
}

fn normalize_bypass_entry(value: &str) -> Option<String> {
    let mut value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut is_url = false;
    if let Some(rest) = value.strip_prefix("http://") {
        value = rest;
        is_url = true;
    } else if let Some(rest) = value.strip_prefix("https://") {
        value = rest;
        is_url = true;
    }
    if is_url {
        value = value.split('/').next().unwrap_or(value);
    }
    value = value.trim_start_matches("*.");
    Some(value.trim_start_matches('.').to_ascii_lowercase())
}

fn normalize_domain_suffix(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .to_ascii_lowercase()
}

fn is_ip_entry(value: &str) -> bool {
    value.contains('/') || IpAddr::from_str(value).is_ok()
}

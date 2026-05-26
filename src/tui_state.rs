use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_TUI_STATE_PATH: &str = "sing-box-tui.json";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TuiRuntimeState {
    #[serde(default)]
    pub(crate) benchmark_filter: String,
    #[serde(default)]
    pub(crate) auto_pick_enabled: bool,
    #[serde(default)]
    pub(crate) current_selected_nodes: BTreeMap<String, String>,
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

pub(crate) fn default_tui_state_path() -> PathBuf {
    env::var("SING_BOX_TUI_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_TUI_STATE_PATH))
}

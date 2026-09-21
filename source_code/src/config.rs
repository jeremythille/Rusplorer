//! Persistent configuration (`rusplorer.config.json`) and session snapshots
//! (`.rsess` files).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::types::{SortColumn, TabState};

// ---------------------------------------------------------------------------
// Serde helpers
// ---------------------------------------------------------------------------

pub(crate) fn default_sort_column() -> SortColumn {
    SortColumn::Name
}

pub(crate) fn default_sort_ascending() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Config {
    pub(crate) last_path: String,
    pub(crate) show_date_columns: HashMap<String, bool>,
    #[serde(default)]
    pub(crate) thumb_view: HashMap<String, bool>,
    #[serde(default = "default_sort_column")]
    pub(crate) sort_column: SortColumn,
    #[serde(default = "default_sort_ascending")]
    pub(crate) sort_ascending: bool,
    #[serde(default)]
    pub(crate) favorites: Vec<String>,
    /// Auto-saved tabs on exit, restored on next launch.
    #[serde(default)]
    pub(crate) tabs: Option<Vec<TabState>>,
    #[serde(default)]
    pub(crate) active_tab: Option<usize>,
    /// Last known window position [x, y], persisted across restarts.
    #[serde(default)]
    pub(crate) window_pos: Option<[f32; 2]>,
    /// Last known window inner size [w, h], persisted across restarts.
    #[serde(default)]
    pub(crate) window_size: Option<[f32; 2]>,
}

impl Config {
    pub(crate) fn path() -> PathBuf {
        let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rusplorer.exe"));
        let mut config_path = exe_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        config_path.push("rusplorer.config.json");
        config_path
    }

    pub(crate) fn load() -> Self {
        if let Ok(content) = std::fs::read_to_string(Self::path()) {
            if let Ok(config) = serde_json::from_str(&content) {
                return config;
            }
        }
        Config {
            last_path: "C:\\".to_string(),
            show_date_columns: HashMap::new(),
            thumb_view: HashMap::new(),
            sort_column: SortColumn::Name,
            sort_ascending: true,
            favorites: Vec::new(),
            tabs: None,
            active_tab: None,
            window_pos: None,
            window_size: None,
        }
    }

    pub(crate) fn save(&self) {
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), content);
        }
    }
}

// ---------------------------------------------------------------------------
// Session snapshot
// ---------------------------------------------------------------------------

/// Snapshot of the current browsing session that can be saved to a `.rsess`
/// file and restored by passing the file as a CLI argument.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct SessionData {
    pub(crate) tabs: Vec<TabState>,
    pub(crate) active_tab: usize,
    #[serde(default)]
    pub(crate) window_pos: Option<[f32; 2]>,
    #[serde(default)]
    pub(crate) window_size: Option<[f32; 2]>,
}

impl SessionData {
    pub(crate) fn save_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| e.to_string())
            .and_then(|content| std::fs::write(path, content).map_err(|e| e.to_string()))
    }

    pub(crate) fn load_from_file(path: &std::path::Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }
}

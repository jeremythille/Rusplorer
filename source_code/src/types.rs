//! Core data types shared across the application.
//! Kept in one place so the compiler can cache them independently.

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) enum SortColumn {
    Name,
    Size,
    Date,
}

// ---------------------------------------------------------------------------
// File actions
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) enum FileAction {
    OpenDir(PathBuf),
    GoToParent,
}

// ---------------------------------------------------------------------------
// Clipboard mode
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Copy, PartialEq)]
pub(crate) enum ClipboardMode {
    Copy,
    Cut,
}

// ---------------------------------------------------------------------------
// Drive types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Copy, PartialEq)]
pub(crate) enum DriveKind {
    Ssd,
    Hdd,
    Removable, // USB / SD card
    Network,
    CdRom,
    Unknown,
}

impl DriveKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            DriveKind::Ssd      => "SSD",
            DriveKind::Hdd      => "HDD",
            DriveKind::Removable => "USB",
            DriveKind::Network  => "Network",
            DriveKind::CdRom    => "CD-ROM",
            DriveKind::Unknown  => "Unknown",
        }
    }

    pub(crate) fn color(self) -> egui::Color32 {
        match self {
            DriveKind::Ssd      => egui::Color32::from_rgb(0, 130, 0),
            DriveKind::Hdd      => egui::Color32::from_gray(130),
            DriveKind::Removable => egui::Color32::from_rgb(220, 150, 170),
            DriveKind::Network  => egui::Color32::from_rgb(100, 160, 220),
            DriveKind::CdRom    => egui::Color32::from_rgb(200, 160, 80),
            DriveKind::Unknown  => egui::Color32::TRANSPARENT,
        }
    }
}

#[derive(Clone)]
pub(crate) struct DriveInfo {
    pub(crate) drive: String, // e.g. "C:\\"
    pub(crate) kind: DriveKind,
    pub(crate) free_bytes: u64,
    pub(crate) total_bytes: u64,
}

// ---------------------------------------------------------------------------
// File entry
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct FileEntry {
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    #[allow(dead_code)]
    pub(crate) size: u64,
    pub(crate) modified: Option<SystemTime>,
}

// ---------------------------------------------------------------------------
// Tab state
// ---------------------------------------------------------------------------

/// Per-tab browsing state.  Lightweight: only stores what needs to be
/// preserved across tab switches.  Everything else (computed sizes, watcher,
/// selection, etc.) is rebuilt on switch via `refresh_contents()`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct TabState {
    pub(crate) path: PathBuf,
    pub(crate) back_history: Vec<PathBuf>,
    pub(crate) forward_history: Vec<PathBuf>,
    pub(crate) filter: String,
    pub(crate) sort_column: SortColumn,
    pub(crate) sort_ascending: bool,
}

impl TabState {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            back_history: Vec::new(),
            forward_history: Vec::new(),
            filter: String::new(),
            sort_column: SortColumn::Name,
            sort_ascending: true,
        }
    }

    /// Short display label: last path component, or drive letter.
    pub(crate) fn label(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.to_string_lossy().to_string())
    }
}

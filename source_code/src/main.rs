#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use arboard::Clipboard;
use eframe::egui;
use eframe::egui_wgpu;
use egui_extras::{Column, TableBuilder};
use notify::recommended_watcher;
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::SystemTime;

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use winapi::um::shellapi::{FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, SHFILEOPSTRUCTW, SHFileOperationW};
#[cfg(windows)]
use winapi::um::winuser::GetAsyncKeyState;

mod clipboard;
mod fs_ops;
mod ole;
mod shortcuts;
mod tree;

#[cfg(windows)]
use clipboard::{copy_files_to_clipboard, read_clipboard_drop_effect_is_cut, read_files_from_clipboard};
#[cfg(windows)]
use fs_ops::{calculate_dir_size_progressive, read_dir_children};
use fs_ops::{CopyJobState, ConflictChoice, ConflictInfo, spawn_copy_job};
#[cfg(windows)]
use ole::{find_own_hwnd, ole_drag_files_out, register_ole_drop_target, try_move_to_rusplorer_desktop};
#[cfg(windows)]
use shortcuts::{create_lnk_shortcut, resolve_lnk};
use tree::render_tree_node;

fn main() -> Result<(), eframe::Error> {
    // In release mode the console is hidden (windows_subsystem = "windows").
    // Catch panics and eframe errors and write them to a log file next to the
    // exe so startup failures are diagnosable on restricted corporate machines.
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("PANIC: {}\n", info);
        let log_path = std::env::current_exe()
            .ok()
            .map(|p| p.with_file_name("rusplorer_error.log"))
            .unwrap_or_else(|| std::path::PathBuf::from("rusplorer_error.log"));
        let _ = std::fs::write(&log_path, &msg);
        // Also show a message box so the user knows something went wrong
        #[cfg(windows)]
        unsafe {
            use std::ffi::CString;
            let title = CString::new("Rusplorer crashed").unwrap_or_default();
            let body  = CString::new(format!("Rusplorer encountered a fatal error.\nDetails written to:\n{}", log_path.display())).unwrap_or_default();
            winapi::um::winuser::MessageBoxA(
                std::ptr::null_mut(),
                body.as_ptr(),
                title.as_ptr(),
                winapi::um::winuser::MB_OK | winapi::um::winuser::MB_ICONERROR,
            );
        }
    }));

    let result = run_app();
    if let Err(ref e) = result {
        let msg = format!("eframe error: {:?}\n", e);
        let log_path = std::env::current_exe()
            .ok()
            .map(|p| p.with_file_name("rusplorer_error.log"))
            .unwrap_or_else(|| std::path::PathBuf::from("rusplorer_error.log"));
        let _ = std::fs::write(&log_path, &msg);
        #[cfg(windows)]
        unsafe {
            use std::ffi::CString;
            let title = CString::new("Rusplorer failed to start").unwrap_or_default();
            let body  = CString::new(format!("Rusplorer could not initialise the graphics window.\nDetails written to:\n{}", log_path.display())).unwrap_or_default();
            winapi::um::winuser::MessageBoxA(
                std::ptr::null_mut(),
                body.as_ptr(),
                title.as_ptr(),
                winapi::um::winuser::MB_OK | winapi::um::winuser::MB_ICONERROR,
            );
        }
    }
    result
}

fn run_app() -> Result<(), eframe::Error> {
    // Initialise OLE on the main thread so DoDragDrop works
    #[cfg(windows)]
    unsafe {
        let _ = windows::Win32::System::Ole::OleInitialize(None);
    }

    // Parse optional session file from CLI: rusplorer.exe [session.rsess]
    let session: Option<SessionData> = std::env::args()
        .nth(1)
        .and_then(|arg| SessionData::load_from_file(std::path::Path::new(&arg)));

    let is_dev = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_lowercase()))
        .map(|name| name.contains("dev"))
        .unwrap_or(false);
    let window_title: &'static str = if is_dev {
        concat!("Rusplorer (dev) (", env!("GIT_COMMIT_DATE"), ")")
    } else {
        concat!("Rusplorer (", env!("GIT_COMMIT_DATE"), ")")
    };

    // 1. Glow (OpenGL) — fastest on machines that support it.
    let result = launch(eframe::Renderer::Glow, None, session.clone(), window_title);

    // 2. If OpenGL 2.0 is unavailable, try wgpu with primary backends (DX12 / Vulkan).
    let result = match result {
        Err(ref e) if format!("{:?}", e).to_lowercase().contains("opengl") => {
            launch(eframe::Renderer::Wgpu, None, session.clone(), window_title)
        }
        other => return other,
    };

    // 3. Last resort: use the GL (ANGLE) backend.
    //    On Windows, ANGLE implements OpenGL ES on top of D3D11's software path,
    //    which works on AWS WorkSpaces, Hyper-V guests, and any environment where
    //    there is no GPU and DX12/Vulkan are unavailable.
    //    Do NOT set WGPU_ADAPTER_NAME — wgpu panics with unwrap() if that env var
    //    is set but no adapter name matches.
    match result {
        Err(ref e) if format!("{:?}", e).contains("NoSuitableAdapterFound") => {
            let mut wgpu_config = egui_wgpu::WgpuConfiguration::default();
            wgpu_config.supported_backends = eframe::wgpu::Backends::GL;
            wgpu_config.power_preference = eframe::wgpu::PowerPreference::None;
            launch(eframe::Renderer::Wgpu, Some(wgpu_config), session, window_title)
        }
        other => other,
    }
}

fn launch(
    renderer: eframe::Renderer,
    wgpu_config_override: Option<egui_wgpu::WgpuConfiguration>,
    session: Option<SessionData>,
    window_title: &'static str,
) -> Result<(), eframe::Error> {
    let mut options = eframe::NativeOptions::default();
    options.renderer = renderer;
    if let Some(wgpu_config) = wgpu_config_override {
        options.wgpu_options = wgpu_config;
    }
    // Disable multisampling — required on some corporate/VM environments
    // where the GPU driver does not expose MSAA sample counts.
    options.multisampling = 0;
    options.viewport.inner_size = session
        .as_ref()
        .and_then(|s| s.window_size)
        .map(|[w, h]| egui::vec2(w, h))
        .or(Some(egui::vec2(700.0, 600.0)));
    options.viewport.position = session
        .as_ref()
        .and_then(|s| s.window_pos)
        .map(|[x, y]| egui::pos2(x, y));
    options.viewport.icon = {
        let icon_bytes = include_bytes!("../logo/rusplorer_logo_512.png");
        let image = image::load_from_memory(icon_bytes).expect("Failed to load icon");
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        Some(std::sync::Arc::new(egui::IconData {
            rgba: rgba.into_raw(),
            width,
            height,
        }))
    };

    eframe::run_native(
        window_title,
        options,
        Box::new(|cc| {
            // Embed Iosevka Aile Regular + Bold (subsetted) at compile time
            let mut fonts = egui::FontDefinitions::default();

            fonts.font_data.insert(
                "IosevkaAile-Regular".to_owned(),
                egui::FontData::from_static(include_bytes!("fonts/IosevkaAile-Regular.ttf")),
            );
            fonts.font_data.insert(
                "IosevkaAile-Bold".to_owned(),
                egui::FontData::from_static(include_bytes!("fonts/IosevkaAile-Bold.ttf")),
            );
            // Replace the default proportional font with Iosevka Aile Regular
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "IosevkaAile-Regular".to_owned());
            // Register Bold under a named family used in the tree
            fonts
                .families
                .entry(egui::FontFamily::Name("Bold".into()))
                .or_default()
                .insert(0, "IosevkaAile-Bold".to_owned());

            cc.egui_ctx.set_fonts(fonts);

            let mut style = (*cc.egui_ctx.style()).clone();
            // Set 11pt font size for all text styles
            for (_, font_id) in &mut style.text_styles {
                font_id.size = 11.0;
            }
            style.spacing.button_padding = egui::vec2(2.0, 0.0);
            style.visuals.widgets.hovered.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.hovered.bg_fill =
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 10);
            style.visuals.widgets.active.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
            style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
            cc.egui_ctx.set_style(style);
            Box::new(RusplorerApp::new(session))
        }),
    )
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
enum SortColumn {
    Name,
    Size,
    Date,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Config {
    last_path: String,
    show_date_columns: HashMap<String, bool>,
    #[serde(default)]
    thumb_view: HashMap<String, bool>,
    #[serde(default = "default_sort_column")]
    sort_column: SortColumn,
    #[serde(default = "default_sort_ascending")]
    sort_ascending: bool,
    #[serde(default)]
    favorites: Vec<String>,
    /// Auto-saved tabs on exit, restored on next launch.
    #[serde(default)]
    tabs: Option<Vec<TabState>>,
    #[serde(default)]
    active_tab: Option<usize>,
}

fn default_sort_column() -> SortColumn {
    SortColumn::Name
}
fn default_sort_ascending() -> bool {
    true
}

impl Config {
    fn path() -> PathBuf {
        let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rusplorer.exe"));
        let mut config_path = exe_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        config_path.push("rusplorer.config.json");
        config_path
    }

    fn load() -> Self {
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
        }
    }

    fn save(&self) {
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), content);
        }
    }
}

/// Snapshot of the current browsing session that can be saved to a `.rsess`
/// file and restored by passing the file as a CLI argument.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct SessionData {
    tabs: Vec<TabState>,
    active_tab: usize,
    #[serde(default)]
    window_pos: Option<[f32; 2]>,
    #[serde(default)]
    window_size: Option<[f32; 2]>,
}

impl SessionData {
    fn save_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| e.to_string())
            .and_then(|content| std::fs::write(path, content).map_err(|e| e.to_string()))
    }

    fn load_from_file(path: &std::path::Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }
}

struct RusplorerApp {
    current_path: PathBuf,
    contents: Vec<FileEntry>,
    selected_action: Option<FileAction>,
    back_history: Vec<PathBuf>,
    forward_history: Vec<PathBuf>,
    available_drives: Vec<String>,
    file_sizes: HashMap<PathBuf, u64>,
    size_receiver: Option<Receiver<(PathBuf, u64)>>,
    cancel_token: Arc<AtomicBool>,
    pause_token: Arc<AtomicBool>,
    dragged_files: Vec<PathBuf>,
    show_drop_menu: bool,
    #[allow(dead_code)]
    drop_menu_position: egui::Pos2,
    is_right_click_drag: bool,
    config: Config,
    max_file_size: u64,
    is_focused: bool,
    filter: String,
    #[allow(dead_code)]
    file_watcher: Option<notify::RecommendedWatcher>,
    watch_receiver: Option<Receiver<PathBuf>>,
    stop_watcher: Option<Sender<()>>,
    show_context_menu: bool,
    context_menu_entry: Option<FileEntry>,
    /// When the context menu was opened from the tree panel, holds the full path
    /// (overrides current_path + name for path resolution).
    context_menu_tree_path: Option<PathBuf>,
    /// Path of the tree node being right-clicked (for visual highlight).
    context_menu_tree_highlight: Option<PathBuf>,
    context_menu_position: egui::Pos2,
    context_menu_size: egui::Vec2,
    /// Snapshot of the selected files at the moment the context menu was opened.
    /// Used by menu actions so that a click-through to the table can't clobber the selection.
    context_menu_selection: Vec<PathBuf>,
    show_rename_dialog: bool,
    rename_buffer: String,
    /// The file extension (e.g. ".txt") stored separately while renaming.
    rename_ext: String,
    /// Whether the extension is shown in the rename text field.
    rename_show_ext: bool,
    // New folder / new file dialogs (triggered from background right-click menu)
    show_new_item_dialog: bool,
    new_item_is_dir: bool,
    new_item_name_buffer: String,
    show_bg_context_menu: bool,
    bg_context_position: egui::Pos2,
    bg_context_menu_size: egui::Vec2,
    selected_entries: HashSet<String>,
    /// Anchor entry name for shift-click range selection.
    last_clicked_entry: Option<String>,
    /// Paths of the files/folders deleted in the last delete operation (for undo).
    last_deleted_paths: Vec<PathBuf>,
    show_archive_dialog: bool,
    archive_type: usize,      // 0 = 7z, 1 = zip
    compression_level: usize, // 0 = store, 1 = medium, 2 = high
    archive_name_buffer: String,
    files_to_archive: Vec<PathBuf>,
    archive_done_receiver: Option<Receiver<String>>,
    show_extract_dialog: bool,
    extract_archive_path: PathBuf,
    extract_done_receiver: Option<Receiver<()>>,
    clipboard_files: Vec<PathBuf>,
    clipboard_mode: Option<ClipboardMode>,
    prev_ctrl_c_down: bool,
    prev_ctrl_v_down: bool,
    prev_ctrl_x_down: bool,
    prev_del_down: bool,
    selection_drag_start: Option<egui::Pos2>,
    selection_drag_current: Option<egui::Pos2>,
    entry_rects: HashMap<String, egui::Rect>,
    is_dragging_selection: bool,
    selection_before_drag: HashSet<String>,
    any_button_hovered: bool,
    filter_edit_rect: egui::Rect,    // rect of the filter TextEdit — excluded from rubber-band
    // Internal drag-and-drop
    dnd_active: bool,
    dnd_sources: Vec<PathBuf>,
    dnd_label: String,
    dnd_start_pos: Option<egui::Pos2>,
    dnd_drag_entry: Option<String>,  // entry name when pointer was pressed (raw tracking)
    dnd_drop_target: Option<PathBuf>,
    dnd_drop_target_prev: Option<PathBuf>, // previous frame's value, used for color display
    dnd_is_right_click: bool,
    dnd_suppress: u8, // frame counter: suppresses new drag/context-menu detection while > 0
    // Pending right-click drop menu: (sources, destination, screen position)
    dnd_right_drop_menu: Option<(Vec<PathBuf>, PathBuf, egui::Pos2)>,
    dirs_done: HashSet<PathBuf>,
    dirs_done_receiver: Option<Receiver<PathBuf>>,
    show_date_columns: HashMap<PathBuf, bool>,
    sort_column: SortColumn,
    sort_ascending: bool,
    // Left panel
    favorites: Vec<PathBuf>,
    tree_expanded: HashSet<PathBuf>,
    tree_children_cache: HashMap<PathBuf, Vec<PathBuf>>,
    left_panel_width: f32,
    right_panel_width: f32,
    prev_left_panel_width: f32,
    // Tabs
    tabs: Vec<TabState>,
    active_tab: usize,
    // Virtual desktop placement on startup
    startup_vd_done: bool,
    startup_vd_attempts: u8,
    // OLE drop-in channel: Explorer → Rusplorer
    ole_drop_receiver: Option<std::sync::mpsc::Receiver<Vec<PathBuf>>>,
    ole_drop_sender: Option<std::sync::mpsc::Sender<Vec<PathBuf>>>,
    ole_rclick_drop_receiver: Option<std::sync::mpsc::Receiver<Vec<PathBuf>>>,
    ole_rclick_drop_sender: Option<std::sync::mpsc::Sender<Vec<PathBuf>>>,
    drop_target_registered: bool,
    /// True while an OLE drag-in from another app (e.g. Explorer) is in progress.
    /// Prevents the internal DnD system from activating on the same pointer press.
    ole_drag_in_active: Arc<AtomicBool>,
    // Keep the COM IDropTarget alive for the lifetime of the app
    #[cfg(windows)]
    _ole_drop_target: Option<windows::Win32::System::Ole::IDropTarget>,
    #[cfg(not(windows))]
    _ole_drop_target: Option<()>,
    // Save-session dialog
    show_save_session_dialog: bool,
    save_session_filename: String,
    save_session_status: Option<String>,
    // Tab bar drag-reorder
    tab_drag_index: Option<usize>,
    tab_drag_start_x: f32,
    tab_scroll_to_active: bool,
    tab_scroll_offset: f32,
    tab_scroll_target: f32,
    tab_bar_rect: egui::Rect,
    // DnD over tab bar: persisted rects + hover-to-switch tracking
    dnd_tab_rects: Vec<egui::Rect>,
    dnd_tab_hover: Option<(usize, std::time::Instant)>,
    /// Cached drive kind per drive root (e.g. "C:\\").
    drive_types: HashMap<String, DriveKind>,
    /// Detailed info cached for the Drives overview page.
    drives_info: Vec<DriveInfo>,
    show_drives_page: bool,
    /// Set while asynchronously waiting for a slow drive (HDD/USB/Network) to spin up.
    /// Holds the target path being navigated to.
    loading_path: Option<PathBuf>,
    /// Receives a signal from the background spin-up probe thread.
    /// `true`  = path is accessible and ready; `false` = path not accessible.
    dir_load_receiver: Option<std::sync::mpsc::Receiver<bool>>,
    /// Timestamp of the last drive-list refresh (for hotplug detection).
    last_drive_check: std::time::Instant,
    /// Per-folder thumbnail view toggle.
    thumb_view: HashMap<PathBuf, bool>,
    /// Cached egui textures for image thumbnails.
    thumb_cache: HashMap<PathBuf, egui::TextureHandle>,
    /// Paths currently being loaded in the background (prevents duplicate spawns).
    thumb_loading: HashSet<PathBuf>,
    /// Send half of the thumbnail loader channel, cloned into each loader thread.
    thumb_loader_tx: std::sync::mpsc::Sender<(PathBuf, egui::ColorImage)>,
    /// Receive half of the thumbnail loader channel.
    thumb_loader_rx: std::sync::mpsc::Receiver<(PathBuf, egui::ColorImage)>,
    /// Active background copy/move jobs.
    copy_jobs: Vec<Arc<CopyJobState>>,
}

#[derive(Clone)]
enum FileAction {
    OpenDir(PathBuf),
    GoToParent,
}

#[derive(Clone, Debug, Copy, PartialEq)]
enum ClipboardMode {
    Copy,
    Cut,
}

#[derive(Clone, Debug, Copy, PartialEq)]
enum DriveKind {
    Ssd,
    Hdd,
    Removable,  // USB / SD card
    Network,
    CdRom,
    Unknown,
}

impl DriveKind {
    fn label(self) -> &'static str {
        match self {
            DriveKind::Ssd      => "SSD",
            DriveKind::Hdd      => "HDD",
            DriveKind::Removable => "USB",
            DriveKind::Network  => "Network",
            DriveKind::CdRom    => "CD-ROM",
            DriveKind::Unknown  => "Unknown",
        }
    }
    fn color(self) -> egui::Color32 {
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
struct DriveInfo {
    drive: String,   // e.g. "C:\\"
    kind: DriveKind,
    free_bytes: u64,
    total_bytes: u64,
}

#[derive(Clone)]
struct FileEntry {
    name: String,
    is_dir: bool,
    #[allow(dead_code)]
    size: u64,
    modified: Option<SystemTime>,
}

/// Per-tab browsing state.  Lightweight: only stores what needs to be
/// preserved across tab switches.  Everything else (computed sizes, watcher,
/// selection, etc.) is rebuilt on switch via `refresh_contents()`.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct TabState {
    path: PathBuf,
    back_history: Vec<PathBuf>,
    forward_history: Vec<PathBuf>,
    filter: String,
    sort_column: SortColumn,
    sort_ascending: bool,
}

impl TabState {
    fn new(path: PathBuf) -> Self {
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
    fn label(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.to_string_lossy().to_string())
    }
}

impl Default for RusplorerApp {
    fn default() -> Self {
        Self::new(None)
    }
}

impl RusplorerApp {
    fn new(session: Option<SessionData>) -> Self {
        let available_drives = Self::list_drives();
        let config = Config::load();
        let start_path = PathBuf::from(&config.last_path);
        let current_path = if start_path.exists() {
            start_path
        } else {
            PathBuf::from("C:\\")
        };
        let show_date_columns: HashMap<PathBuf, bool> = config
            .show_date_columns
            .iter()
            .map(|(k, v)| (PathBuf::from(k), *v))
            .collect();
        let thumb_view: HashMap<PathBuf, bool> = config
            .thumb_view
            .iter()
            .map(|(k, v)| (PathBuf::from(k), *v))
            .collect();
        let sort_column = config.sort_column.clone();
        let (ole_tx, ole_rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        let (ole_rc_tx, ole_rc_rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        let (thumb_tx, thumb_rx) = std::sync::mpsc::channel::<(PathBuf, egui::ColorImage)>();
        let sort_ascending = config.sort_ascending;
        let mut favorites: Vec<PathBuf> = config.favorites.iter().map(PathBuf::from).collect();
        favorites.sort_by(|a, b| {
            let a_name = a.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_else(|| a.to_string_lossy().to_lowercase().into());
            let b_name = b.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_else(|| b.to_string_lossy().to_lowercase().into());
            a_name.cmp(&b_name)
        });

        // Extract saved tabs before config is moved into the app struct.
        let config_saved_tabs = config.tabs.clone();
        let config_saved_active_tab = config.active_tab;

        let mut app = Self {
            current_path,
            contents: Vec::new(),
            selected_action: None,
            back_history: Vec::new(),
            forward_history: Vec::new(),
            available_drives,
            file_sizes: HashMap::new(),
            size_receiver: None,
            cancel_token: Arc::new(AtomicBool::new(false)),
            pause_token: Arc::new(AtomicBool::new(false)),
            dragged_files: Vec::new(),
            show_drop_menu: false,
            drop_menu_position: egui::Pos2::ZERO,
            is_right_click_drag: false,
            config,
            max_file_size: 0,
            is_focused: true,
            filter: String::new(),
            file_watcher: None,
            watch_receiver: None,
            stop_watcher: None,
            show_context_menu: false,
            context_menu_entry: None,
            context_menu_tree_path: None,
            context_menu_tree_highlight: None,
            context_menu_position: egui::Pos2::ZERO,
            context_menu_size: egui::vec2(100.0, 100.0),
            context_menu_selection: Vec::new(),
            show_rename_dialog: false,
            rename_buffer: String::new(),
            rename_ext: String::new(),
            rename_show_ext: false,
            show_new_item_dialog: false,
            new_item_is_dir: false,
            new_item_name_buffer: String::new(),
            show_bg_context_menu: false,
            bg_context_position: egui::Pos2::ZERO,
            bg_context_menu_size: egui::vec2(100.0, 80.0),
            selected_entries: HashSet::new(),
            last_clicked_entry: None,
            show_archive_dialog: false,
            archive_type: 0,
            compression_level: 2,
            archive_name_buffer: String::new(),
            files_to_archive: Vec::new(),
            archive_done_receiver: None,
            show_extract_dialog: false,
            extract_archive_path: PathBuf::new(),
            extract_done_receiver: None,
            clipboard_files: Vec::new(),
            clipboard_mode: None,
            prev_ctrl_c_down: false,
            prev_ctrl_v_down: false,
            prev_ctrl_x_down: false,
            prev_del_down: false,
            selection_drag_start: None,
            selection_drag_current: None,
            entry_rects: HashMap::new(),
            is_dragging_selection: false,
            selection_before_drag: HashSet::new(),
            any_button_hovered: false,
            filter_edit_rect: egui::Rect::NOTHING,
            dnd_active: false,
            dnd_sources: Vec::new(),
            dnd_label: String::new(),
            dnd_start_pos: None,
            dnd_drag_entry: None,
            dnd_drop_target: None,
            dnd_drop_target_prev: None,
            dnd_is_right_click: false,
            dnd_suppress: 0,
            dnd_right_drop_menu: None,
            dirs_done: HashSet::new(),
            dirs_done_receiver: None,
            show_date_columns,
            sort_column,
            sort_ascending,
            favorites,
            tree_expanded: HashSet::new(),
            tree_children_cache: HashMap::new(),
            left_panel_width: 150.0,
            right_panel_width: 0.0,
            prev_left_panel_width: 0.0,
            tabs: Vec::new(), // populated below
            active_tab: 0,
            startup_vd_done: false,
            startup_vd_attempts: 0,
            ole_drop_receiver: Some(ole_rx),
            ole_drop_sender: Some(ole_tx),
            ole_rclick_drop_receiver: Some(ole_rc_rx),
            ole_rclick_drop_sender: Some(ole_rc_tx),
            drop_target_registered: false,
            ole_drag_in_active: Arc::new(AtomicBool::new(false)),
            _ole_drop_target: None,
            show_save_session_dialog: false,
            last_deleted_paths: Vec::new(),
            save_session_filename: String::new(),
            save_session_status: None,
            tab_drag_index: None,
            tab_drag_start_x: 0.0,
            tab_scroll_to_active: true,
            tab_scroll_offset: 0.0,
            tab_scroll_target: 0.0,
            tab_bar_rect: egui::Rect::NOTHING,
            dnd_tab_rects: Vec::new(),
            dnd_tab_hover: None,
            drive_types: HashMap::new(),
            drives_info: Vec::new(),
            show_drives_page: false,
            loading_path: None,
            dir_load_receiver: None,
            last_drive_check: std::time::Instant::now(),
            thumb_view,
            thumb_cache: HashMap::new(),
            thumb_loading: HashSet::new(),
            thumb_loader_tx: thumb_tx,
            thumb_loader_rx: thumb_rx,
            copy_jobs: Vec::new(),
        };

        // Initialise tabs — from session if provided, then config, then single default
        if let Some(sess) = session {
            if !sess.tabs.is_empty() {
                app.tabs = sess.tabs;
                app.active_tab = sess.active_tab.min(app.tabs.len().saturating_sub(1));
                app.current_path = app.tabs[app.active_tab].path.clone();
            } else {
                app.tabs.push(TabState::new(app.current_path.clone()));
            }
        } else if let Some(saved_tabs) = config_saved_tabs.filter(|t| !t.is_empty()) {
            app.tabs = saved_tabs;
            app.active_tab = config_saved_active_tab.unwrap_or(0).min(app.tabs.len().saturating_sub(1));
            app.current_path = app.tabs[app.active_tab].path.clone();
        } else {
            app.tabs.push(TabState::new(app.current_path.clone()));
        }

        // Expand tree to current path (single call covers all branches above)
        let path_snap = app.current_path.clone();
        app.expand_tree_to(&path_snap);

        app.refresh_contents();
        app.start_file_watcher();

        // Classify each available drive (best-effort, cached once at startup)
        for drive in app.available_drives.clone() {
            let letter = drive.chars().next().unwrap_or('C');
            let kind = Self::classify_drive(letter);
            let (free_bytes, total_bytes) = Self::get_drive_space(&drive);
            app.drive_types.insert(drive.clone(), kind);
            app.drives_info.push(DriveInfo { drive, kind, free_bytes, total_bytes });
        }

        app
    }
}

impl RusplorerApp {
    /// Snapshot the current tabs into a `SessionData` and write it to `path`.
    fn save_session_to_file(&mut self, path: &std::path::Path, ctx: &egui::Context) -> Result<(), String> {
        self.save_active_tab();
        let (window_pos, window_size) = ctx.input(|i| {
            let vp = i.viewport();
            let pos = vp.outer_rect.map(|r| [r.min.x, r.min.y]);
            let size = vp.inner_rect.map(|r| [r.width(), r.height()]);
            (pos, size)
        });
        let data = SessionData {
            tabs: self.tabs.clone(),
            active_tab: self.active_tab,
            window_pos,
            window_size,
        };
        data.save_to_file(path)
    }

    fn list_drives() -> Vec<String> {
        let mut drives = Vec::new();

        // Check drives A through Z
        for letter in b'A'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            if PathBuf::from(&drive).exists() {
                drives.push(drive);
            }
        }

        drives
    }

    /// Returns `Some(true)` = SSD, `Some(false)` = HDD, `None` = unknown.
    /// Uses IOCTL_STORAGE_QUERY_PROPERTY / StorageDeviceSeekPenaltyProperty.
    #[cfg(windows)]
    fn query_is_ssd(drive_letter: char) -> Option<bool> {
        use winapi::um::fileapi::CreateFileW;
        use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
        use winapi::um::ioapiset::DeviceIoControl;
        use winapi::shared::minwindef::{DWORD, LPVOID};

        const OPEN_EXISTING: DWORD = 3;
        const FILE_SHARE_READ: DWORD = 0x0000_0001;
        const FILE_SHARE_WRITE: DWORD = 0x0000_0002;
        const IOCTL_STORAGE_QUERY_PROPERTY: DWORD = 0x002D_1400;

        #[repr(C)]
        struct StoragePropertyQuery {
            property_id: u32,        // 7 = StorageDeviceSeekPenaltyProperty
            query_type: u32,         // 0 = PropertyStandardQuery
            additional_parameters: [u8; 1],
        }

        #[repr(C)]
        struct DeviceSeekPenaltyDescriptor {
            version: u32,
            size: u32,
            incurs_seek_penalty: u8, // 0 = SSD (no seek penalty)
        }

        let path: Vec<u16> = format!("\\\\.\\{}:", drive_letter)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return None;
        }

        let query = StoragePropertyQuery {
            property_id: 7,
            query_type: 0,
            additional_parameters: [0],
        };
        let mut desc: DeviceSeekPenaltyDescriptor = unsafe { std::mem::zeroed() };
        let mut bytes_returned: DWORD = 0;

        let result = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                &query as *const _ as LPVOID,
                std::mem::size_of::<StoragePropertyQuery>() as DWORD,
                &mut desc as *mut _ as LPVOID,
                std::mem::size_of::<DeviceSeekPenaltyDescriptor>() as DWORD,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };

        unsafe { CloseHandle(handle); }

        if result != 0 { Some(desc.incurs_seek_penalty == 0) } else { None }
    }

    #[cfg(not(windows))]
    fn query_is_ssd(_drive_letter: char) -> Option<bool> { None }

    #[cfg(windows)]
    fn classify_drive(drive_letter: char) -> DriveKind {
        use winapi::um::fileapi::GetDriveTypeW;
        const DRIVE_REMOVABLE: u32 = 2;
        const DRIVE_FIXED:     u32 = 3;
        const DRIVE_REMOTE:    u32 = 4;
        const DRIVE_CDROM:     u32 = 5;
        const BUS_TYPE_USB:    u32 = 7;
        let path: Vec<u16> = format!("{}:\\", drive_letter)
            .encode_utf16().chain(std::iter::once(0)).collect();
        let kind = unsafe { GetDriveTypeW(path.as_ptr()) };
        match kind {
            DRIVE_REMOVABLE => DriveKind::Removable,
            DRIVE_REMOTE    => DriveKind::Network,
            DRIVE_CDROM     => DriveKind::CdRom,
            DRIVE_FIXED     => {
                // Some USB drives (especially high-capacity ones) self-report as
                // DRIVE_FIXED.  Check the actual bus type to catch them.
                if Self::query_bus_type(drive_letter) == Some(BUS_TYPE_USB) {
                    return DriveKind::Removable;
                }
                match Self::query_is_ssd(drive_letter) {
                    Some(true)  => DriveKind::Ssd,
                    Some(false) => DriveKind::Hdd,
                    // Drive asleep / IOCTL failed → assume HDD so a border is shown
                    None        => DriveKind::Hdd,
                }
            },
            _               => DriveKind::Unknown,
        }
    }

    /// Returns the bus type constant from `STORAGE_DEVICE_DESCRIPTOR.BusType`.
    /// 7 = BusTypeUsb.
    #[cfg(windows)]
    fn query_bus_type(drive_letter: char) -> Option<u32> {
        use winapi::um::fileapi::CreateFileW;
        use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
        use winapi::um::ioapiset::DeviceIoControl;
        use winapi::shared::minwindef::{DWORD, LPVOID};
        const OPEN_EXISTING: DWORD = 3;
        const FILE_SHARE_READ:  DWORD = 0x0000_0001;
        const FILE_SHARE_WRITE: DWORD = 0x0000_0002;
        const IOCTL_STORAGE_QUERY_PROPERTY: DWORD = 0x002D_1400;

        #[repr(C)]
        struct StoragePropertyQuery {
            property_id: u32,        // 0 = StorageDeviceProperty
            query_type:  u32,        // 0 = PropertyStandardQuery
            additional_parameters: [u8; 1],
        }
        // Only the fields we care about; layout matches Windows SDK struct.
        #[repr(C)]
        struct StorageDeviceDescriptor {
            version:                  u32,
            size:                     u32,
            device_type:              u8,
            device_type_modifier:     u8,
            removable_media:          u8,
            command_queueing:         u8,
            vendor_id_offset:         u32,
            product_id_offset:        u32,
            product_revision_offset:  u32,
            serial_number_offset:     u32,
            bus_type:                 u32,
        }

        let path: Vec<u16> = format!("\\\\.\\{}:", drive_letter)
            .encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(path.as_ptr(), 0,
                FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null_mut(),
                OPEN_EXISTING, 0, std::ptr::null_mut())
        };
        if handle == INVALID_HANDLE_VALUE { return None; }

        let query = StoragePropertyQuery { property_id: 0, query_type: 0, additional_parameters: [0] };
        let mut desc: StorageDeviceDescriptor = unsafe { std::mem::zeroed() };
        let mut bytes: DWORD = 0;
        let ok = unsafe {
            DeviceIoControl(handle, IOCTL_STORAGE_QUERY_PROPERTY,
                &query as *const _ as LPVOID,
                std::mem::size_of::<StoragePropertyQuery>() as DWORD,
                &mut desc as *mut _ as LPVOID,
                std::mem::size_of::<StorageDeviceDescriptor>() as DWORD,
                &mut bytes, std::ptr::null_mut())
        };
        unsafe { CloseHandle(handle); }
        if ok != 0 { Some(desc.bus_type) } else { None }
    }

    #[cfg(not(windows))]
    fn classify_drive(_drive_letter: char) -> DriveKind { DriveKind::Unknown }
    #[cfg(not(windows))]
    fn query_bus_type(_: char) -> Option<u32> { None }

    /// Returns (free_bytes, total_bytes) for the given drive root (e.g. "C:\\").
    #[cfg(windows)]
    fn get_drive_space(drive: &str) -> (u64, u64) {
        use winapi::um::fileapi::GetDiskFreeSpaceExW;
        let path: Vec<u16> = drive.encode_utf16().chain(std::iter::once(0)).collect();
        // ULARGE_INTEGER is a union that is exactly 8 bytes; u64 is layout-compatible
        let mut free_caller: u64 = 0;
        let mut total: u64       = 0;
        let mut free_total: u64  = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                path.as_ptr(),
                &mut free_caller as *mut u64 as *mut _,
                &mut total       as *mut u64 as *mut _,
                &mut free_total  as *mut u64 as *mut _,
            )
        };
        if ok != 0 { (free_total, total) } else { (0, 0) }
    }
    #[cfg(not(windows))]
    fn get_drive_space(_drive: &str) -> (u64, u64) { (0, 0) }

    fn format_bytes(bytes: u64) -> String {
        const TB: u64 = 1 << 40;
        const GB: u64 = 1 << 30;
        const MB: u64 = 1 << 20;
        if bytes >= TB {
            format!("{:.2} TB", bytes as f64 / TB as f64)
        } else if bytes >= GB {
            format!("{:.1} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.0} MB", bytes as f64 / MB as f64)
        } else {
            format!("{} KB", bytes / 1024)
        }
    }

    /// Collapse the entire tree, then expand only the ancestors of `path`.
    /// This ensures unrelated drives/folders are hidden after every navigation.
    fn expand_tree_to(&mut self, path: &PathBuf) {
        self.tree_expanded.clear();
        let ancestors: Vec<PathBuf> = path.ancestors().map(|p| p.to_path_buf()).collect();
        for ancestor in ancestors.into_iter().rev() {
            if !self.tree_children_cache.contains_key(&ancestor) {
                let children = read_dir_children(&ancestor);
                self.tree_children_cache.insert(ancestor.clone(), children);
            }
            self.tree_expanded.insert(ancestor);
        }
    }

    // ── Tab helpers ────────────────────────────────────────────────────

    /// Save the current browsing state into the active tab.
    fn save_active_tab(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.path = self.current_path.clone();
            tab.back_history = self.back_history.clone();
            tab.forward_history = self.forward_history.clone();
            tab.filter = self.filter.clone();
            tab.sort_column = self.sort_column.clone();
            tab.sort_ascending = self.sort_ascending;
        }
    }

    /// Restore per-tab state from the given tab index into the app fields
    /// and refresh directory contents + watcher.
    fn restore_tab(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get(index) {
            self.current_path = tab.path.clone();
            self.back_history = tab.back_history.clone();
            self.forward_history = tab.forward_history.clone();
            self.filter = tab.filter.clone();
            self.sort_column = tab.sort_column.clone();
            self.sort_ascending = tab.sort_ascending;
            self.selected_entries.clear();

            // Collapse everything unrelated, expand only ancestors of new path
            let path_snap = self.current_path.clone();
            self.expand_tree_to(&path_snap);

            self.refresh_contents();
            self.start_file_watcher();
        }
    }

    /// Switch to a different tab index.
    fn switch_to_tab(&mut self, index: usize) {
        if index == self.active_tab || index >= self.tabs.len() {
            return;
        }
        self.save_active_tab();
        self.active_tab = index;
        self.restore_tab(index);
    }

    /// Open a new tab.  Clones the current path by default.
    fn new_tab(&mut self, path: Option<PathBuf>) {
        self.save_active_tab();
        let tab_path = path.unwrap_or_else(|| self.current_path.clone());
        self.tabs.push(TabState::new(tab_path));
        self.active_tab = self.tabs.len() - 1;
        self.restore_tab(self.active_tab);
    }

    /// Close the tab at `index`.  Won't close the last remaining tab.
    fn close_tab(&mut self, index: usize) {
        if self.tabs.len() <= 1 {
            return;
        }
        let was_active = index == self.active_tab;
        self.tabs.remove(index);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if index < self.active_tab {
            self.active_tab -= 1;
        }
        if was_active {
            self.restore_tab(self.active_tab);
        }
    }

    fn refresh_contents(&mut self) {
        // Cancel any running background computation
        self.cancel_token.store(true, Ordering::SeqCst);
        self.cancel_token = Arc::new(AtomicBool::new(false));

        self.contents.clear();
        self.file_sizes.clear();
        self.max_file_size = 0;
        self.dirs_done.clear();

        // Add parent directory option
        if let Some(parent) = self.current_path.parent() {
            if parent != self.current_path {
                self.contents.push(FileEntry {
                    name: "[..] Parent Directory".to_string(),
                    is_dir: true,
                    size: 0,
                    modified: None,
                });
            }
        }

        // List directory contents (fast - no size lookup)
        if let Ok(entries) = std::fs::read_dir(&self.current_path) {
            let mut items: Vec<_> = entries
                .filter_map(|e| e.ok())
                .map(|e| {
                    let path = e.path();
                    let name = e.file_name().to_string_lossy().to_string();
                    let is_dir = path.is_dir();
                    let modified = e.metadata().ok().and_then(|m| m.modified().ok());
                    FileEntry {
                        name,
                        is_dir,
                        size: 0,
                        modified,
                    }
                })
                .collect();

            items.sort_by(|a, b| {
                match (a.is_dir, b.is_dir) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => {
                        let ord = match self.sort_column {
                            SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                            SortColumn::Date => a.modified.cmp(&b.modified),
                            SortColumn::Size => std::cmp::Ordering::Equal, // will be re-sorted when sizes arrive
                        };
                        if self.sort_ascending {
                            ord
                        } else {
                            ord.reverse()
                        }
                    }
                }
            });

            self.contents.extend(items);
        }

        // Collect paths for background processing
        let mut file_paths: Vec<PathBuf> = Vec::new();
        let mut dir_paths: Vec<PathBuf> = Vec::new();
        for entry in &self.contents {
            if entry.name.starts_with("[..]") {
                continue;
            }
            let full_path = self.current_path.join(&entry.name);
            if entry.is_dir {
                dir_paths.push(full_path);
            } else {
                file_paths.push(full_path);
            }
        }

        // Start background thread to load file and folder sizes
        let cancel_token = self.cancel_token.clone();
        let pause_token = self.pause_token.clone();
        let (tx, rx) = channel();
        let (done_tx, done_rx) = channel::<PathBuf>();

        std::thread::spawn(move || {
            // First: send all file sizes immediately (fast)
            for path in file_paths {
                if cancel_token.load(Ordering::SeqCst) {
                    return;
                }
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let _ = tx.send((path, size));
            }

            // Then: compute directory sizes in parallel
            let num_threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(dir_paths.len().max(1));

            if !dir_paths.is_empty() {
                let work_queue = std::sync::Arc::new(std::sync::Mutex::new(dir_paths));
                let mut handles = Vec::new();

                for _ in 0..num_threads {
                    let queue = work_queue.clone();
                    let cancel = cancel_token.clone();
                    let pause = pause_token.clone();
                    let tx = tx.clone();
                    let done_tx = done_tx.clone();

                    handles.push(std::thread::spawn(move || {
                        loop {
                            let dir_path = {
                                match queue.lock() {
                                    Ok(mut dirs) => dirs.pop(),
                                    Err(_) => break,
                                }
                            };

                            let dir_path = match dir_path {
                                Some(p) => p,
                                None => break,
                            };

                            if cancel.load(Ordering::SeqCst) {
                                return;
                            }
                            while pause.load(Ordering::SeqCst) {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                                if cancel.load(Ordering::SeqCst) {
                                    return;
                                }
                            }

                            let mut accumulated = 0u64;
                            calculate_dir_size_progressive(
                                &dir_path,
                                &dir_path,
                                &cancel,
                                &pause,
                                &tx,
                                &mut accumulated,
                            );
                            // Always send final size (handles empty dirs and permission errors)
                            let _ = tx.send((dir_path.clone(), accumulated));
                            // Signal this directory is done computing
                            let _ = done_tx.send(dir_path);
                        }
                    }));
                }

                for handle in handles {
                    let _ = handle.join();
                }
            }
        });

        self.dirs_done_receiver = Some(done_rx);

        self.size_receiver = Some(rx);
    }

    fn sort_contents(&mut self) {
        let sort_column = &self.sort_column;
        let sort_ascending = self.sort_ascending;
        let file_sizes = &self.file_sizes;
        let current_path = &self.current_path;

        self.contents.sort_by(|a, b| {
            // Parent directory always first
            if a.name.starts_with("[..]") {
                return std::cmp::Ordering::Less;
            }
            if b.name.starts_with("[..]") {
                return std::cmp::Ordering::Greater;
            }

            // Dirs always before files
            match (a.is_dir, b.is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => {
                    let ord = match sort_column {
                        SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                        SortColumn::Size => {
                            let sa = file_sizes
                                .get(&current_path.join(&a.name))
                                .copied()
                                .unwrap_or(0);
                            let sb = file_sizes
                                .get(&current_path.join(&b.name))
                                .copied()
                                .unwrap_or(0);
                            sa.cmp(&sb)
                        }
                        SortColumn::Date => a.modified.cmp(&b.modified),
                    };
                    if sort_ascending { ord } else { ord.reverse() }
                }
            }
        });
    }

    /// Returns true when `path` lives on a drive kind that may need to spin up
    /// (HDD, removable USB/SD, or Network) and would therefore block the UI.
    fn is_slow_drive(&self, path: &std::path::Path) -> bool {
        // Extract the drive root component (e.g. "C:\\").
        let root = path.components().next().map(|c| {
            let mut s = c.as_os_str().to_string_lossy().to_string();
            if !s.ends_with('\\') { s.push('\\'); }
            s
        });
        if let Some(root) = root {
            matches!(
                self.drive_types.get(&root).copied().unwrap_or(DriveKind::Unknown),
                DriveKind::Hdd | DriveKind::Removable | DriveKind::Network
            )
        } else {
            false
        }
    }

    fn navigate_to(&mut self, path: PathBuf) {
        if path != self.current_path {
            self.back_history.push(self.current_path.clone());
            self.forward_history.clear();
        }
        self.commit_navigation(path);
    }

    /// Shared "point the UI at `path` and load it, respecting spin-up for slow drives".
    /// History manipulation is handled by the *callers*; this only does the load.
    fn commit_navigation(&mut self, path: PathBuf) {
        self.current_path = path.clone();
        self.config.last_path = self.current_path.to_string_lossy().to_string();
        self.config.show_date_columns = self
            .show_date_columns
            .iter()
            .map(|(k, v)| (k.to_string_lossy().to_string(), *v))
            .collect();
        self.config.save();
        let path_snap = self.current_path.clone();
        self.expand_tree_to(&path_snap);
        self.save_active_tab();

        if self.is_slow_drive(&path) {
            self.contents.clear();
            self.cancel_token.store(true, Ordering::SeqCst);
            self.loading_path = Some(path.clone());
            let (tx, rx) = std::sync::mpsc::channel::<bool>();
            self.dir_load_receiver = Some(rx);
            std::thread::spawn(move || {
                let ok = path.exists() && path.is_dir();
                let _ = tx.send(ok);
            });
        } else {
            self.loading_path = None;
            self.dir_load_receiver = None;
            self.refresh_contents();
            self.start_file_watcher();
        }
    }

    fn go_back(&mut self) {
        if let Some(previous) = self.back_history.pop() {
            self.forward_history.push(self.current_path.clone());
            self.commit_navigation(previous);
        }
    }

    fn go_forward(&mut self) {
        if let Some(next) = self.forward_history.pop() {
            self.back_history.push(self.current_path.clone());
            self.commit_navigation(next);
        }
    }

    fn get_breadcrumbs(&self) -> Vec<(PathBuf, String)> {
        let mut breadcrumbs = Vec::new();
        let mut path = self.current_path.clone();

        // Skip the drive letter, we only want the path components
        if let Some(parent) = path.parent() {
            if parent != path {
                // Get all path components except the drive
                let mut components = Vec::new();
                loop {
                    if let Some(file_name) = path.file_name() {
                        if let Some(name_str) = file_name.to_str() {
                            components.push((path.clone(), name_str.to_string()));
                        }
                    }
                    if let Some(parent) = path.parent() {
                        if parent == path {
                            break; // We've reached the root (drive letter)
                        }
                        path = parent.to_path_buf();
                    } else {
                        break;
                    }
                }
                breadcrumbs = components.into_iter().rev().collect();
            }
        }
        breadcrumbs
    }

    fn format_path_display(path: &PathBuf) -> String {
        path.to_string_lossy().replace("\\", "/")
    }

    fn is_code_file(path: &PathBuf) -> bool {
        if let Some(ext) = path.extension() {
            let ext_str = ext.to_string_lossy().to_lowercase();
            matches!(
                ext_str.as_str(),
                "rs" | "js"
                    | "ts"
                    | "jsx"
                    | "tsx"
                    | "py"
                    | "java"
                    | "c"
                    | "cpp"
                    | "h"
                    | "hpp"
                    | "cs"
                    | "go"
                    | "rb"
                    | "php"
                    | "html"
                    | "css"
                    | "scss"
                    | "json"
                    | "xml"
                    | "yaml"
                    | "yml"
                    | "toml"
                    | "md"
                    | "txt"
                    | "sh"
                    | "bat"
                    | "ps1"
                    | "sql"
                    | "vue"
                    | "svelte"
            )
        } else {
            false
        }
    }

    fn is_archive(path: &PathBuf) -> bool {
        if let Some(ext) = path.extension() {
            let ext_str = ext.to_string_lossy().to_lowercase();
            matches!(
                ext_str.as_str(),
                "7z" | "zip" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "iso"
            )
        } else {
            false
        }
    }

    /// Returns true if the file extension indicates an image that can be thumbnailed.
    fn is_image_file(name: &str) -> bool {
        matches!(
            name.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref(),
            Some("jpg" | "jpeg" | "png" | "bmp" | "gif" | "webp")
        )
    }

    /// Returns true if the file extension indicates a video.
    fn is_video_file(name: &str) -> bool {
        matches!(
            name.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref(),
            Some("mp4" | "avi" | "mkv" | "mov" | "wmv" | "flv" | "webm" | "m4v")
        )
    }

    fn format_file_size(bytes: u64) -> String {
        const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
        let mut size = bytes as f64;
        let mut unit_index = 0;

        while size >= 1024.0 && unit_index < UNITS.len() - 1 {
            size /= 1024.0;
            unit_index += 1;
        }

        if unit_index == 0 {
            format!("{} {}", bytes, UNITS[0])
        } else {
            format!("{:.1} {}", size, UNITS[unit_index])
        }
    }

    /// Format a `SystemTime` as a local-time string.
    /// `tz_bias_secs` is the UTC offset (computed once per frame, not per row).
    fn format_modified_time(time: SystemTime, tz_bias_secs: i64) -> String {
        use std::time::UNIX_EPOCH;
        let Ok(dur) = time.duration_since(UNIX_EPOCH) else {
            return String::new();
        };

        let local_secs = dur.as_secs() as i64 - tz_bias_secs;
        if local_secs < 0 { return String::new(); }
        let secs = local_secs as u64;

        let time_of_day = secs % 86400;
        let hour   = time_of_day / 3600;
        let minute = (time_of_day % 3600) / 60;

        // Euclidean algorithm for Gregorian calendar (Hinnant, public domain)
        let z = (secs / 86400) as i64 + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe/1460 + doe/36524 - doe/146096) / 365;
        let y   = yoe + era * 400;
        let doy = doe - (365*yoe + yoe/4 - yoe/100);
        let mp  = (5*doy + 2) / 153;
        let d   = doy - (153*mp + 2)/5 + 1;
        let m   = if mp < 10 { mp + 3 } else { mp - 9 };
        let y   = if m <= 2 { y + 1 } else { y };

        format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, hour, minute)
    }

    /// Returns a color for a file extension group, adapted to light vs dark mode.
    /// Returns `None` for unknown / unclassified extensions (use default text color).
    fn ext_color(ext_lower: &str, dark_mode: bool) -> Option<egui::Color32> {
        // Color pairs: (dark-mode, light-mode)
        macro_rules! col {
            ($rd:expr,$gd:expr,$bd:expr, $rl:expr,$gl:expr,$bl:expr) => {
                if dark_mode {
                    egui::Color32::from_rgb($rd, $gd, $bd)
                } else {
                    egui::Color32::from_rgb($rl, $gl, $bl)
                }
            };
        }
        match ext_lower {
            // ── Executables & scripts ──────────────────────────────────────────
            ".exe" | ".msi" | ".com" | ".appx" | ".msix" =>
                Some(col!(100, 160, 255,  30,  80, 200)),   // blue
            ".bat" | ".cmd" | ".ps1" | ".psm1" | ".psd1" | ".vbs" | ".wsf" | ".sh" =>
                Some(col!(130, 180, 255,  50, 100, 200)),   // softer blue
            // ── Libraries & system ────────────────────────────────────────────
            ".dll" | ".sys" | ".ocx" | ".drv" | ".ax" =>
                Some(col!(160, 210, 255,  60, 130, 210)),   // light blue
            // ── Videos ───────────────────────────────────────────────────────
            ".mp4" | ".mkv" | ".avi" | ".mov" | ".wmv" | ".flv"
            | ".webm" | ".m4v" | ".m2ts" | ".vob" | ".mpg" | ".mpeg" | ".f4v" | ".3gp" =>
                Some(col!( 80, 200,  90,  20, 130,  40)),   // green
            // ── Audio ────────────────────────────────────────────────────────
            ".mp3" | ".wav" | ".flac" | ".aac" | ".ogg" | ".wma" | ".m4a"
            | ".opus" | ".aiff" | ".ape" =>
                Some(col!( 50, 200, 200,   0, 130, 140)),   // teal / cyan
            // ── Images ───────────────────────────────────────────────────────
            ".jpg" | ".jpeg" | ".png" | ".bmp" | ".gif" | ".webp"
            | ".svg" | ".ico" | ".tiff" | ".tif" | ".heic" | ".avif" | ".raw"
            | ".cr2" | ".nef" | ".arw" =>
                Some(col!(255, 165,  50,  190,  90,   0)),  // amber / orange
            // ── Archives ─────────────────────────────────────────────────────
            ".zip" | ".7z" | ".rar" | ".tar" | ".gz" | ".bz2" | ".xz"
            | ".zst" | ".cab" | ".iso" | ".img" | ".dmg" | ".tgz" | ".lz4" =>
                Some(col!(200, 100, 230,  130,  30, 170)),  // purple
            // ── Documents ────────────────────────────────────────────────────
            ".pdf" =>
                Some(col!(255,  90,  80,  190,  30,  20)),  // red
            ".doc" | ".docx" | ".odt" | ".rtf" =>
                Some(col!(130, 200, 255,  30, 100, 200)),   // Word blue
            ".xls" | ".xlsx" | ".ods" | ".csv" =>
                Some(col!( 80, 210, 120,  20, 140,  60)),   // Excel green
            ".ppt" | ".pptx" | ".odp" =>
                Some(col!(255, 140,  80,  200,  70,  10)),  // PowerPoint orange
            ".txt" | ".md" | ".rst" | ".log" | ".nfo" =>
                Some(col!(180, 180, 180,  100, 100, 100)),  // gray
            // ── Code & markup ─────────────────────────────────────────────────
            ".rs" | ".c" | ".cpp" | ".cc" | ".cxx" | ".h" | ".hpp"
            | ".cs" | ".java" | ".go" | ".swift" | ".kt" | ".kts"
            | ".py" | ".rb" | ".php" | ".pl" | ".lua"
            | ".js" | ".ts" | ".jsx" | ".tsx" | ".vue" | ".svelte"
            | ".html" | ".htm" | ".css" | ".scss" | ".sass" | ".less"
            | ".sql" | ".r" | ".m" | ".f" | ".f90" | ".zig" | ".nim"
            | ".dart" | ".ex" | ".exs" | ".clj" | ".cljs" | ".scala"
            | ".elm" | ".erl" | ".hrl" | ".hs" | ".lhs" =>
                Some(col!(255, 140,  60,  160,  70,   0)),  // code orange
            // ── Config / data ─────────────────────────────────────────────────
            ".json" | ".toml" | ".yaml" | ".yml" | ".xml" | ".ini"
            | ".cfg" | ".conf" | ".env" | ".properties" | ".plist"
            | ".reg" | ".desktop" | ".service" =>
                Some(col!(160, 220, 160,  40, 130,  40)),   // muted green
            // ── Fonts ─────────────────────────────────────────────────────────
            ".ttf" | ".otf" | ".woff" | ".woff2" | ".eot" =>
                Some(col!(230, 160, 230,  150,  40, 150)),  // magenta
            // ── Everything else → no color override ───────────────────────────
            _ => None,
        }
    }

    /// Build a `LayoutJob` for a file-list entry name.
    /// For files (non-directory), the extension is rendered in **bold** with a
    /// type-based color; the stem uses the regular proportional font.
    /// Directories and the parent "[..]" entry are rendered as plain text.
    fn name_layout_job(
        name: &str,
        is_dir: bool,
        color: egui::Color32,
        italics: bool,
        dark_mode: bool,
    ) -> egui::text::LayoutJob {
        const SIZE: f32 = 11.0;
        let regular = egui::FontId::new(SIZE, egui::FontFamily::Proportional);
        let bold    = egui::FontId::new(SIZE, egui::FontFamily::Name("Bold".into()));
        let fmt_reg  = egui::text::TextFormat { font_id: regular.clone(), color, italics, ..Default::default() };

        let mut job = egui::text::LayoutJob::default();

        let is_parent = name.starts_with("[..]");
        if is_dir || is_parent {
            job.append(name, 0.0, fmt_reg);
        } else {
            // Find the last dot that is not at position 0 (hidden-file "." prefix)
            let dot_pos = name.rfind('.').filter(|&p| p > 0);
            match dot_pos {
                Some(p) => {
                    let ext_lower = name[p..].to_lowercase();
                    // Use type color only when the entry is not highlighted (selected/clipboard).
                    // When it IS highlighted the background is already colored, so use
                    // the caller-supplied color (usually WHITE) for readability.
                    let ext_color = if color == egui::Color32::WHITE {
                        color // highlighted — keep white for contrast
                    } else {
                        Self::ext_color(&ext_lower, dark_mode).unwrap_or(color)
                    };
                    let fmt_stem = egui::text::TextFormat { font_id: regular.clone(), color: ext_color, italics, ..Default::default() };
                    let fmt_bold = egui::text::TextFormat { font_id: bold, color: ext_color, italics, ..Default::default() };
                    job.append(&name[..p], 0.0, fmt_stem);
                    job.append(&name[p..], 0.0, fmt_bold);
                }
                None => {
                    job.append(name, 0.0, fmt_reg);
                }
            }
        }
        job
    }

    /// Returns a background color for the date column based on how old the file is.
    /// Violet = timestamp is in the future (invalid/dashcam clock drift).
    /// Light green (very recent) → darker green → light orange → orange (>1 week).
    fn age_color(modified: SystemTime, now: SystemTime) -> egui::Color32 {
        // Future timestamp — device clock is wrong (e.g. dashcam with bad RTC).
        if modified > now {
            return egui::Color32::from_rgb(140, 80, 200);
        }
        let age = now
            .duration_since(modified)
            .map(|d| d.as_secs_f64())
            .unwrap_or(f64::MAX);

        // (age_threshold_secs, r, g, b)
        const STOPS: &[(f64, u8, u8, u8)] = &[
            (0.0,          200, 240, 200),  // light green  — just now
            (300.0,        150, 218, 150),  // green        — 5 min
            (3_600.0,       90, 180,  90),  // medium green — 1 hour
            (86_400.0,     180, 210, 130),  // yellow-green — 1 day
            (604_800.0,    255, 200, 140),  // light orange — 1 week
        ];
        const ORANGE: egui::Color32 = egui::Color32::from_rgb(255, 175, 100);

        if age >= 604_800.0 {
            return ORANGE;
        }
        for w in STOPS.windows(2) {
            let (t0, r0, g0, b0) = w[0];
            let (t1, r1, g1, b1) = w[1];
            if age <= t1 {
                let t = ((age - t0) / (t1 - t0)) as f32;
                let lerp = |a: u8, b: u8| (a as f32 + t * (b as f32 - a as f32)).round() as u8;
                return egui::Color32::from_rgb(lerp(r0, r1), lerp(g0, g1), lerp(b0, b1));
            }
        }
        ORANGE
    }

    /// Start an async background copy/move job.  Returns immediately.
    /// `clear_clipboard` – set `true` for cut-paste so the clipboard is cleared on completion.
    fn start_copy_job(&mut self, sources: Vec<PathBuf>, dest: PathBuf, is_move: bool, clear_clipboard: bool) {
        let dest_display = dest.to_string_lossy().to_string();
        let state = Arc::new(CopyJobState::new(is_move, dest_display));
        state.clear_clipboard.store(clear_clipboard, Ordering::Relaxed);
        spawn_copy_job(sources, dest, Arc::clone(&state));
        self.copy_jobs.push(state);
    }
}

impl RusplorerApp {
    fn start_file_watcher(&mut self) {
        // Signal old watcher to stop
        if let Some(stop_tx) = self.stop_watcher.take() {
            let _ = stop_tx.send(());
        }

        let (tx, rx) = channel();
        let (stop_tx, stop_rx) = channel();
        let current_path = self.current_path.clone();

        // Create watcher in a separate thread
        let tx = std::sync::Arc::new(std::sync::Mutex::new(tx));

        std::thread::spawn(move || {
            let tx = tx.clone();
            if let Ok(mut watcher) = recommended_watcher(move |res| {
                match res {
                    Ok(notify::event::Event {
                        kind:
                            notify::event::EventKind::Modify(_)
                            | notify::event::EventKind::Create(_)
                            | notify::event::EventKind::Remove(_),
                        paths,
                        ..
                    }) => {
                        // Send the actual changed paths to invalidate cache
                        for path in paths {
                            if let Ok(tx) = tx.lock() {
                                let _ = tx.send(path);
                            }
                        }
                    }
                    _ => {}
                }
            }) {
                // Watch the directory (non-recursive to avoid flood of deep events)
                match watcher.watch(&current_path, RecursiveMode::NonRecursive) {
                    Ok(_) => {
                        // Keep watcher alive until stop signal arrives
                        let _ = stop_rx.recv();
                    }
                    Err(_) => {
                        return;
                    }
                }
            }
        });

        self.watch_receiver = Some(rx);
        self.stop_watcher = Some(stop_tx);
    }

    fn process_file_changes(&mut self) {
        let mut needs_refresh = false;

        if let Some(ref rx) = self.watch_receiver {
            while let Ok(path) = rx.try_recv() {
                // Only care about direct children of current directory
                if let Some(parent) = path.parent() {
                    if parent == self.current_path {
                        let file_name = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        let exists_in_list = self.contents.iter().any(|e| e.name == file_name);
                        let exists_on_disk = path.exists();

                        if (exists_on_disk && !exists_in_list)
                            || (!exists_on_disk && exists_in_list)
                        {
                            // Direct child created or removed - full refresh needed
                            needs_refresh = true;
                        } else if exists_on_disk && !path.is_dir() {
                            // Direct child file was modified - update its size inline
                            if let Ok(metadata) = path.metadata() {
                                let size = metadata.len();
                                self.file_sizes.insert(path, size);
                                if size > self.max_file_size {
                                    self.max_file_size = size;
                                }
                            }
                        }
                    }
                }
            }
        }

        if needs_refresh {
            self.refresh_contents();
            // Also refresh the tree cache for current_path so newly created/deleted
            // subdirectories appear (or disappear) in the left-panel tree.
            let updated_children = read_dir_children(&self.current_path.clone());
            self.tree_children_cache.insert(self.current_path.clone(), updated_children);
        }
    }

    /// Restore files from the Windows Recycle Bin by matching against their original paths.
    /// Returns `true` if all items were restored successfully.
    #[cfg(windows)]
    fn restore_from_recycle_bin(paths: &[PathBuf]) -> bool {
        let mut restored = 0usize;
        let total = paths.len();

        for original_path in paths {
            // Derive the drive root (e.g.  C:\)
            // We need both the Prefix and RootDir components to get "C:\"
            let mut drive = PathBuf::new();
            let mut comp_count = 0;
            for comp in original_path.components() {
                drive.push(comp);
                comp_count += 1;
                if comp_count >= 2 { break; } // Prefix + RootDir
            }
            if comp_count < 2 {
                continue;
            }
            let recycle_bin = drive.join("$Recycle.Bin");

            // Iterate all SID subdirectories inside $Recycle.Bin
            let sid_entries = match std::fs::read_dir(&recycle_bin) {
                Ok(e) => e,
                Err(_) => continue,
            };

            'sid_loop: for sid_entry in sid_entries.flatten() {
                let sid_path = sid_entry.path();
                if !sid_path.is_dir() {
                    continue;
                }

                let items = match std::fs::read_dir(&sid_path) {
                    Ok(i) => i,
                    Err(_) => continue,
                };

                for item in items.flatten() {
                    let item_path = item.path();
                    let file_name = item_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();

                    // Only look at $I metadata files
                    if !file_name.starts_with("$I") {
                        continue;
                    }

                    let data = match std::fs::read(&item_path) {
                        Ok(d) => d,
                        Err(_) => continue,
                    };

                    if data.len() < 28 {
                        continue;
                    }

                    // Parse the original path from the $I file.
                    // Version stored at byte 0 (i64): 1 = Vista/7 (fixed 260-char slot),
                    // 2 = Windows 10+ (length-prefixed).
                    let version = i64::from_le_bytes(
                        data[0..8].try_into().unwrap_or([0; 8]),
                    );

                    let orig_path_opt: Option<PathBuf> = if version == 1 {
                        // Fixed-size slot: up to 260 UTF-16 chars starting at offset 24
                        if data.len() >= 26 {
                            let utf16: Vec<u16> = data[24..]
                                .chunks_exact(2)
                                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                                .take_while(|&c| c != 0)
                                .collect();
                            String::from_utf16(&utf16).ok().map(PathBuf::from)
                        } else {
                            None
                        }
                    } else {
                        // Version 2: path length (i32) at offset 24, UTF-16 path at offset 28
                        let path_len = i32::from_le_bytes(
                            data[24..28].try_into().unwrap_or([0; 4]),
                        ) as usize;
                        let end = 28 + path_len * 2;
                        if data.len() >= end {
                            let utf16: Vec<u16> = data[28..end]
                                .chunks_exact(2)
                                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                                .collect();
                            String::from_utf16(&utf16)
                                .ok()
                                .map(|s| PathBuf::from(s.trim_end_matches('\0')))
                        } else {
                            None
                        }
                    };

                    if let Some(orig) = orig_path_opt {
                        if orig == *original_path {
                            // The $R file has the same random suffix as $I
                            let r_name = format!("$R{}", &file_name[2..]);
                            let r_path = sid_path.join(&r_name);

                            if r_path.exists() {
                                if let Some(parent) = original_path.parent() {
                                    let _ = std::fs::create_dir_all(parent);
                                }
                                if std::fs::rename(&r_path, original_path).is_ok() {
                                    let _ = std::fs::remove_file(&item_path);
                                    restored += 1;
                                    break 'sid_loop;
                                }
                            }
                        }
                    }
                }
            }
        }

        restored == total
    }
}

impl eframe::App for RusplorerApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Flush active tab state back into the tabs vec, then persist to config.
        self.save_active_tab();
        self.config.tabs = Some(self.tabs.clone());
        self.config.active_tab = Some(self.active_tab);
        self.config.save();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Rotate drop target: prev holds last frame's value for color display;
        // current is reset to None so tree / breadcrumbs / table can detect fresh this frame.
        if self.dnd_active {
            self.dnd_drop_target_prev = self.dnd_drop_target.clone();
            self.dnd_drop_target = None;
        } else {
            self.dnd_drop_target_prev = None;
        }

        // Decrement the suppress frame-counter.  Suppress blocks new drag
        // detection and context-menu triggers for a short window (2 frames)
        // after DoDragDrop returns or an in-window drop completes.  Using a
        // simple counter avoids the old bug where a genuine *new* button press
        // was mistaken for stale state and kept suppress alive indefinitely,
        // causing every-other-drag to fail.
        if self.dnd_suppress > 0 {
            self.dnd_suppress -= 1;
        }

        // Move own window to "Rusplorer" virtual desktop on startup (in-process: no E_ACCESSDENIED)
        if !self.startup_vd_done {
            self.startup_vd_attempts += 1;
            #[cfg(windows)]
            if try_move_to_rusplorer_desktop() || self.startup_vd_attempts >= 10 {
                self.startup_vd_done = true;
            }
            #[cfg(not(windows))]
            { self.startup_vd_done = true; }
        }

        // Register OLE IDropTarget on our HWND so Explorer can drag files in
        #[cfg(windows)]
        if !self.drop_target_registered {
            if let Some(tx) = self.ole_drop_sender.take() {
                let rc_tx = self.ole_rclick_drop_sender.take();
                if let Some(hwnd_raw) = find_own_hwnd() {
                    let hwnd_ptr = hwnd_raw as *mut _;
                    let rc = rc_tx.unwrap_or_else(|| std::sync::mpsc::channel().0);
                    let drag_in_flag = self.ole_drag_in_active.clone();
                    if let Some(target) = register_ole_drop_target(hwnd_ptr, tx, rc, drag_in_flag) {
                        self._ole_drop_target = Some(target);
                        self.drop_target_registered = true;
                    } else {
                        // Registration failed — don't retry (probably no OLE)
                        self.drop_target_registered = true;
                    }
                } else {
                    // HWND not ready yet — put senders back
                    self.ole_drop_sender = Some(tx);
                    self.ole_rclick_drop_sender = rc_tx;
                }
            }
        }

        // Check if archive compression finished
        if let Some(ref rx) = self.archive_done_receiver {
            if let Ok(archive_name) = rx.try_recv() {
                self.refresh_contents();
                self.selected_entries.clear();
                self.selected_entries.insert(archive_name);
                self.archive_done_receiver = None;
            }
        }

        // Check if extraction finished
        if let Some(ref rx) = self.extract_done_receiver {
            if rx.try_recv().is_ok() {
                self.refresh_contents();
                self.show_extract_dialog = false;
                self.extract_done_receiver = None;
            }
        }

        // Drain completed copy/move jobs
        {
            let mut need_refresh = false;
            self.copy_jobs.retain(|job| {
                if job.done.load(Ordering::SeqCst) {
                    // Harvest results
                    let names = job.pasted_names.lock().unwrap().clone();
                    if !names.is_empty() {
                        self.selected_entries.clear();
                        for name in names {
                            self.selected_entries.insert(name);
                        }
                    }
                    if job.clear_clipboard.load(Ordering::Relaxed) {
                        self.clipboard_files.clear();
                        self.clipboard_mode = None;
                    }
                    need_refresh = true;
                    false // remove from list
                } else {
                    true // keep
                }
            });
            if need_refresh {
                self.refresh_contents();
            }
        }

        // Process any file system changes detected by watcher
        self.process_file_changes();

        // Track window focus and pause/resume background work
        let is_focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if is_focused != self.is_focused {
            self.is_focused = is_focused;
            self.pause_token.store(!is_focused, Ordering::SeqCst);
        }

        // Receive OLE drops from Explorer (drag-in)  — left-click = move
        #[cfg(windows)]
        {
            let incoming: Vec<Vec<PathBuf>> = self.ole_drop_receiver
                .as_ref()
                .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
                .unwrap_or_default();
            for files in incoming {
                if !files.is_empty() {
                    // Cancel any internal DnD that may have been triggered by the
                    // same pointer-down (egui sees the button held while OLE drags in).
                    self.dnd_active = false;
                    self.dnd_sources.clear();
                    self.dnd_start_pos = None;
                    self.dnd_drag_entry = None;
                    self.dnd_suppress = 2;
                    let dest = self.current_path.clone();
                    self.start_copy_job(files, dest, true, false);
                    ctx.request_repaint();
                }
            }
        }

        // Receive OLE right-click drops — show menu
        #[cfg(windows)]
        {
            let incoming: Vec<Vec<PathBuf>> = self.ole_rclick_drop_receiver
                .as_ref()
                .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
                .unwrap_or_default();
            for files in incoming {
                if !files.is_empty() {
                    // Cancel any internal DnD that may have been triggered by the
                    // same pointer-down (egui sees the button held while OLE drags in).
                    self.dnd_active = false;
                    self.dnd_sources.clear();
                    self.dnd_start_pos = None;
                    self.dnd_drag_entry = None;
                    self.dnd_suppress = 2;
                    let dest = self.current_path.clone();
                    // Use GetCursorPos+ScreenToClient for accurate drop position;
                    // egui's hover_pos() lags by one WM_MOUSEMOVE behind the actual release.
                    let drop_pos = {
                        let mut pos = egui::pos2(300.0, 300.0);
                        #[cfg(windows)]
                        unsafe {
                            use winapi::shared::windef::POINT;
                            use winapi::um::winuser::{GetCursorPos, ScreenToClient};
                            let mut pt = POINT { x: 0, y: 0 };
                            if GetCursorPos(&mut pt) != 0 {
                                if let Some(hwnd) = crate::ole::find_own_hwnd() {
                                    ScreenToClient(hwnd, &mut pt);
                                }
                                let ppp = ctx.pixels_per_point();
                                pos = egui::pos2(pt.x as f32 / ppp, pt.y as f32 / ppp);
                            }
                        }
                        #[cfg(not(windows))]
                        if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) { pos = p; }
                        pos
                    };
                    self.dnd_right_drop_menu = Some((files, dest, drop_pos));
                    ctx.request_repaint();
                }
            }
        }

        // Handle drag and drop
        ctx.input(|i| {
            let dropped_files = &i.raw.dropped_files;
            if !dropped_files.is_empty() {
                self.dragged_files = dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect();
                if !self.dragged_files.is_empty() {
                    // Check if it's a right-click drag (we'll detect this by checking pointer events)
                    self.is_right_click_drag =
                        i.pointer.button_down(egui::PointerButton::Secondary);
                    self.show_drop_menu = self.is_right_click_drag;

                    // Left click defaults to move
                    if !self.is_right_click_drag {
                        let files = self.dragged_files.clone();
                        let dest = self.current_path.clone();
                        self.start_copy_job(files, dest, true, false);
                        self.dragged_files.clear();
                    }
                }
            }
        });

        // Receive file sizes from background thread
        let mut sizes_updated = false;
        if let Some(ref rx) = self.size_receiver {
            while let Ok((path, size)) = rx.try_recv() {
                self.file_sizes.insert(path, size);
                if size > self.max_file_size {
                    self.max_file_size = size;
                }
                sizes_updated = true;
            }
        }

        // Receive directory completion signals
        if let Some(ref rx) = self.dirs_done_receiver {
            while let Ok(path) = rx.try_recv() {
                self.dirs_done.insert(path);
            }
        }

        // Poll the spin-up probe for slow (HDD/USB/Network) drives.
        // The background thread unblocks as soon as the drive is accessible.
        let spin_done = if let Some(ref rx) = self.dir_load_receiver {
            rx.try_recv().ok()
        } else {
            None
        };
        if let Some(accessible) = spin_done {
            self.loading_path = None;
            self.dir_load_receiver = None;
            if accessible {
                // Drive is now spinning — read_dir will be fast.
                self.refresh_contents();
                self.start_file_watcher();
            }
            // If not accessible (e.g. drive removed), leave the empty listing.
            ctx.request_repaint();
        }
        // Keep repainting while we are waiting so the spinner animates.
        if self.loading_path.is_some() {
            ctx.request_repaint();
        }

        // Drain completed thumbnail loads and register them as GPU textures.
        while let Ok((path, color_image)) = self.thumb_loader_rx.try_recv() {
            self.thumb_loading.remove(&path);
            let texture = ctx.load_texture(
                path.to_string_lossy().to_string(),
                color_image,
                egui::TextureOptions::default(),
            );
            self.thumb_cache.insert(path, texture);
            ctx.request_repaint();
        }

        // Re-sort when sizes arrive and we're sorting by size
        if sizes_updated && self.sort_column == SortColumn::Size {
            self.sort_contents();
        }

        // Handle mouse buttons 4 and 5 (back/forward)
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::PointerButton {
                    button, pressed, ..
                } = event
                {
                    if *pressed {
                        match button {
                            egui::PointerButton::Extra1 => {
                                // Mouse button 4 (back)
                                self.go_back();
                            }
                            egui::PointerButton::Extra2 => {
                                // Mouse button 5 (forward)
                                self.go_forward();
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        // Handle keyboard shortcuts for back/forward
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) && ctx.input(|i| i.modifiers.alt) {
            self.go_back();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) && ctx.input(|i| i.modifiers.alt) {
            self.go_forward();
        }

        // Handle Ctrl+A to select all
        if ctx.input(|i| i.key_pressed(egui::Key::A) && i.modifiers.ctrl) {
            self.selected_entries.clear();
            for entry in &self.contents {
                if !entry.name.starts_with("[..]") {
                    self.selected_entries.insert(entry.name.clone());
                }
            }
        }

        // Handle Escape to deselect all
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selected_entries.clear();
        }

        // Handle Ctrl+C / Ctrl+X / Ctrl+V / DEL using Windows API directly (bypass egui)
        let (got_copy, got_cut, got_paste, got_delete) = {
            #[cfg(windows)]
            {
                const VK_CONTROL: i32 = 0x11;
                const VK_C: i32 = 0x43;
                const VK_V: i32 = 0x56;
                const VK_X: i32 = 0x58;
                const VK_DELETE: i32 = 0x2E;

                let ctrl_down = unsafe { GetAsyncKeyState(VK_CONTROL) } as u16 & 0x8000 != 0;
                let c_down = ctrl_down && (unsafe { GetAsyncKeyState(VK_C) } as u16 & 0x8000 != 0);
                let v_down = ctrl_down && (unsafe { GetAsyncKeyState(VK_V) } as u16 & 0x8000 != 0);
                let x_down = ctrl_down && (unsafe { GetAsyncKeyState(VK_X) } as u16 & 0x8000 != 0);
                let del_down = unsafe { GetAsyncKeyState(VK_DELETE) } as u16 & 0x8000 != 0;

                // Always update prev state to avoid a false edge-trigger when we regain focus
                let prev_c = self.prev_ctrl_c_down;
                let prev_v = self.prev_ctrl_v_down;
                let prev_x = self.prev_ctrl_x_down;
                let prev_d = self.prev_del_down;
                self.prev_ctrl_c_down = c_down;
                self.prev_ctrl_v_down = v_down;
                self.prev_ctrl_x_down = x_down;
                self.prev_del_down = del_down;

                // Only fire actions when Rusplorer actually has focus
                // and no modal text input is active (rename / new-item dialog).
                // Use GetForegroundWindow for reliable check — egui's viewport().focused
                // can return None (defaulting to true), causing false positives when
                // the user presses shortcuts in another window while GetAsyncKeyState
                // reports global key state.
                let dialog_open = self.show_rename_dialog || self.show_new_item_dialog;
                let really_focused = self.is_focused && {
                    match find_own_hwnd() {
                        Some(hwnd) => unsafe {
                            winapi::um::winuser::GetForegroundWindow() == hwnd
                        },
                        None => false,
                    }
                };
                if really_focused && !dialog_open {
                    let cut_pressed    = x_down   && !prev_x;
                    let copy_pressed   = c_down   && !prev_c && !cut_pressed;
                    let paste_pressed  = v_down   && !prev_v;
                    let delete_pressed = del_down && !prev_d;
                    (copy_pressed, cut_pressed, paste_pressed, delete_pressed)
                } else {
                    (false, false, false, false)
                }
            }
            #[cfg(not(windows))]
            {
                let c = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::C));
                let x = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::X));
                let v = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::V));
                let d = ctx.input(|i| i.key_pressed(egui::Key::Delete));
                (c, x, v, d)
            }
        };

        if got_copy && !self.selected_entries.is_empty() {
            let files: Vec<PathBuf> = self
                .selected_entries
                .iter()
                .map(|name| self.current_path.join(name))
                .collect();

            #[cfg(windows)]
            {
                let _ = copy_files_to_clipboard(&files, false); // best-effort; internal clipboard always set
                self.clipboard_files = files;
                self.clipboard_mode = Some(ClipboardMode::Copy);
            }
            #[cfg(not(windows))]
            {
                self.clipboard_files = files;
                self.clipboard_mode = Some(ClipboardMode::Copy);
            }
        }

        if got_cut && !self.selected_entries.is_empty() {
            let files: Vec<PathBuf> = self
                .selected_entries
                .iter()
                .map(|name| self.current_path.join(name))
                .collect();

            #[cfg(windows)]
            {
                let _ = copy_files_to_clipboard(&files, true); // best-effort; internal clipboard always set
                self.clipboard_files = files;
                self.clipboard_mode = Some(ClipboardMode::Cut);
            }
            #[cfg(not(windows))]
            {
                self.clipboard_files = files;
                self.clipboard_mode = Some(ClipboardMode::Cut);
            }
        }

        if got_paste {
            #[cfg(windows)]
            {
                let dest = self.current_path.clone();

                // Always try the Windows clipboard first — it may have
                // been updated by another app (Explorer, another Rusplorer
                // instance, etc.) since we last set our internal clipboard.
                let win_clipboard = read_files_from_clipboard().unwrap_or_default();
                let win_is_cut = if !win_clipboard.is_empty() {
                    read_clipboard_drop_effect_is_cut()
                } else {
                    false
                };

                // Determine which clipboard to use:
                // - If Windows clipboard has files AND they differ from our
                //   internal clipboard → prefer Windows (external copy).
                // - Otherwise use internal clipboard (preserves reliable
                //   cut/copy tracking within this instance).
                let use_internal = !self.clipboard_files.is_empty()
                    && (win_clipboard.is_empty()
                        || {
                            let mut sorted_win = win_clipboard.clone();
                            sorted_win.sort();
                            let mut sorted_int = self.clipboard_files.clone();
                            sorted_int.sort();
                            sorted_win == sorted_int
                        });

                if use_internal {
                    // Use our own internal clipboard — reliable cut/copy detection
                    let files = self.clipboard_files.clone();
                    let is_cut = self.clipboard_mode == Some(ClipboardMode::Cut);
                    self.start_copy_job(files, dest, is_cut, is_cut);
                    if is_cut {
                        self.clipboard_files.clear();
                        self.clipboard_mode = None;
                    }
                } else if !win_clipboard.is_empty() {
                    // Use Windows clipboard (files from another app)
                    self.start_copy_job(win_clipboard.clone(), dest, win_is_cut, win_is_cut);
                    // Sync internal clipboard so subsequent paste in
                    // same session (if copy) re-pastes the same files.
                    self.clipboard_files = win_clipboard;
                    self.clipboard_mode = Some(if win_is_cut {
                        ClipboardMode::Cut
                    } else {
                        ClipboardMode::Copy
                    });
                    if win_is_cut {
                        self.clipboard_files.clear();
                        self.clipboard_mode = None;
                    }
                }
            }
            #[cfg(not(windows))]
            {
                if let Some(mode) = self.clipboard_mode {
                    if !self.clipboard_files.is_empty() {
                        let files = self.clipboard_files.clone();
                        let dest = self.current_path.clone();

                        let pasted_names = match mode {
                            ClipboardMode::Copy => {
                                self.start_copy_job(files, dest, false, false);
                                Vec::new()
                            }
                            ClipboardMode::Cut => {
                                self.start_copy_job(files, dest, true, true);
                                self.clipboard_files.clear();
                                self.clipboard_mode = None;
                                Vec::new()
                            }
                        };
                        self.refresh_contents();
                        self.selected_entries.clear();
                        for name in pasted_names {
                            self.selected_entries.insert(name);
                        }
                    }
                }
            }
        }

        // Handle DEL key - send to recycle bin
        if got_delete && !self.selected_entries.is_empty() {
            let files_to_delete: Vec<PathBuf> = self
                .selected_entries
                .iter()
                .map(|name| self.current_path.join(name))
                .collect();

            #[cfg(windows)]
            {
                // Build double-null-terminated wide string list
                let mut path_buffer: Vec<u16> = Vec::new();
                for path in &files_to_delete {
                    let wide: Vec<u16> = OsStr::new(path.to_str().unwrap())
                        .encode_wide()
                        .chain(std::iter::once(0u16))
                        .collect();
                    path_buffer.extend_from_slice(&wide);
                }
                path_buffer.push(0u16); // Final null terminator

                unsafe {
                    let mut file_op = SHFILEOPSTRUCTW {
                        hwnd: std::ptr::null_mut(),
                        wFunc: FO_DELETE as u32,
                        pFrom: path_buffer.as_ptr(),
                        pTo: std::ptr::null(),
                        fFlags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION,
                        fAnyOperationsAborted: 0,
                        hNameMappings: std::ptr::null_mut(),
                        lpszProgressTitle: std::ptr::null(),
                    };

                    let result = SHFileOperationW(&mut file_op);
                    if result == 0 {
                        self.last_deleted_paths = files_to_delete.clone();
                        self.selected_entries.clear();
                        self.refresh_contents();
                    }
                }
            }
        }

        // F2 → rename the single selected entry
        if self.is_focused
            && !self.show_rename_dialog
            && ctx.input(|i| i.key_pressed(egui::Key::F2))
        {
            if self.selected_entries.len() == 1 {
                let name = self.selected_entries.iter().next().unwrap().clone();
                if let Some(entry) = self.contents.iter().find(|e| e.name == name) {
                    self.rename_ext = std::path::Path::new(&entry.name)
                        .extension()
                        .map(|e| format!(".{}", e.to_string_lossy()))
                        .unwrap_or_default();
                    self.rename_buffer = std::path::Path::new(&entry.name)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| entry.name.clone());
                    self.rename_show_ext = false;
                    self.context_menu_entry = Some(entry.clone());
                    self.context_menu_tree_path = None;
                    self.show_rename_dialog = true;
                    self.show_context_menu = false;
                }
            }
        }

        // Handle pending actions
        if let Some(action) = self.selected_action.take() {
            match action {
                FileAction::GoToParent => {
                    if let Some(parent) = self.current_path.parent() {
                        self.navigate_to(parent.to_path_buf());
                    }
                }
                FileAction::OpenDir(path) => {
                    self.navigate_to(path);
                }
            }
        }

        // ── Left panel ────────────────────────────────────────────────────
        let mut nav_from_panel: Option<PathBuf> = None;

        // Measure ideal panel width from visible content (for this frame, apply next frame)
        {
            let font_id = egui::FontId::new(11.0, egui::FontFamily::Proportional);
            let mut max_w: f32 = 80.0;
            // Measure favorites (8px indent + name + 16px for × button)
            for fav in &self.favorites {
                let name = fav.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| fav.to_string_lossy().to_string());
                let text_w = ctx.fonts(|f| f.layout_no_wrap(name, font_id.clone(), egui::Color32::WHITE).size().x);
                max_w = max_w.max(8.0 + text_w + 16.0);
            }
            // Measure tree (recursively through expanded nodes)
            fn measure_tree(
                path: &PathBuf,
                depth: usize,
                expanded: &HashSet<PathBuf>,
                cache: &HashMap<PathBuf, Vec<PathBuf>>,
                font_id: &egui::FontId,
                ctx: &egui::Context,
                max_w: &mut f32,
            ) {
                let name = path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string_lossy().to_string());
                let indent = depth as f32 * 10.0;
                let text_w = ctx.fonts(|f| f.layout_no_wrap(name, font_id.clone(), egui::Color32::WHITE).size().x);
                *max_w = max_w.max(indent + text_w + 12.0);
                if expanded.contains(path) {
                    if let Some(children) = cache.get(path) {
                        for child in children {
                            measure_tree(child, depth + 1, expanded, cache, font_id, ctx, max_w);
                        }
                    }
                }
            }
            for drive in &self.available_drives {
                let drive_path = PathBuf::from(drive);
                measure_tree(&drive_path, 0, &self.tree_expanded, &self.tree_children_cache, &font_id, ctx, &mut max_w);
            }
            self.left_panel_width = max_w.min(250.0).max(80.0);
        }

        // Compute the toolbar's required minimum width from its actual contents:
        //   "Drives▼" button + drive mini-buttons + "Filter:" label + 70px input + "🖼" button
        let right_panel_min: f32 = {
            let font11 = egui::FontId::proportional(11.0);
            let font14 = egui::FontId::proportional(14.0);
            let item_sp = 8.0_f32; // default egui item_spacing.x

            // "Drives ▲/▼" button: text width + ~10px button padding each side
            let drives_btn_label = if self.show_drives_page { "Drives ▲" } else { "Drives ▼" };
            let drives_btn_w = ctx.fonts(|f| {
                f.layout_no_wrap(drives_btn_label.to_string(), font14.clone(), egui::Color32::WHITE).size().x
            }) + 10.0 + item_sp;

            // Drive mini-buttons: custom size = text_x + pad*2 (6+6=12)
            let drive_btns_w: f32 = self.available_drives.iter().map(|d| {
                let label = d.trim_end_matches(|c: char| c == '\\' || c == '/').to_string();
                let tw = ctx.fonts(|f| {
                    f.layout_no_wrap(label, font11.clone(), egui::Color32::WHITE).size().x
                });
                tw + 12.0 + item_sp
            }).sum::<f32>();

            // "Filter:" label
            let filter_label_w = ctx.fonts(|f| {
                f.layout_no_wrap("Filter:".to_string(), font14.clone(), egui::Color32::WHITE).size().x
            }) + item_sp;

            // Filter TextEdit (fixed 70px allocation)
            let filter_edit_w = 70.0 + item_sp;

            // "🖼" selectable_label
            let thumb_w = ctx.fonts(|f| {
                f.layout_no_wrap("🖼".to_string(), font14.clone(), egui::Color32::WHITE).size().x
            }) + 10.0;

            // Add outer panel margins / frame padding
            let margin = 20.0;

            (drives_btn_w + drive_btns_w + filter_label_w + filter_edit_w + thumb_w + margin)
                .max(200.0)
        };

        // Capture right panel width on first frame, then resize window to fit left+right
        let inner_w = ctx.input(|i| i.viewport().inner_rect.map(|r| r.width())).unwrap_or(0.0);
        if self.right_panel_width == 0.0 && inner_w > 0.0 {
            // Initialise: remember right panel width, enforce toolbar-based minimum.
            self.right_panel_width = (inner_w - self.left_panel_width - 8.0).max(right_panel_min);
            self.prev_left_panel_width = self.left_panel_width;
            // If the current window is too narrow, resize now.
            let desired_w = self.left_panel_width + self.right_panel_width + 8.0;
            if desired_w > inner_w + 2.0 {
                let h = ctx.input(|i| i.viewport().inner_rect.map(|r| r.height())).unwrap_or(600.0);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(desired_w, h)));
            }
        } else if self.right_panel_width > 0.0 {
            let left_changed = (self.left_panel_width - self.prev_left_panel_width).abs() > 0.5;
            if left_changed {
                // Left panel changed — resize window to preserve right panel width
                let desired_w = self.left_panel_width + self.right_panel_width + 8.0;
                let h = ctx.input(|i| i.viewport().inner_rect.map(|r| r.height())).unwrap_or(600.0);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(desired_w, h)));
                self.prev_left_panel_width = self.left_panel_width;
            } else {
                // Left panel unchanged — if window width changed, user resized: update right_panel_width.
                // Re-enforce the toolbar minimum in case the OS hasn't delivered our resize yet.
                let expected_w = self.left_panel_width + self.right_panel_width + 8.0;
                if (inner_w - expected_w).abs() > 2.0 {
                    self.right_panel_width = (inner_w - self.left_panel_width - 8.0).max(right_panel_min);
                    let desired_w = self.left_panel_width + self.right_panel_width + 8.0;
                    if desired_w > inner_w + 2.0 {
                        let h = ctx.input(|i| i.viewport().inner_rect.map(|r| r.height())).unwrap_or(600.0);
                        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(desired_w, h)));
                    }
                }
            }
        }

        // ── Copy/Move progress panel ──────────────────────────────────────────
        if !self.copy_jobs.is_empty() {
            egui::TopBottomPanel::bottom("copy_progress_panel")
                .resizable(false)
                .show(ctx, |ui| {
                    for (job_idx, job) in self.copy_jobs.iter().enumerate() {
                        let is_move = job.is_move;
                        let op = if is_move { "Moving" } else { "Copying" };
                        let files_done = job.files_done.load(Ordering::Relaxed);
                        let files_total = job.files_total.load(Ordering::Relaxed);
                        let bytes_done = job.bytes_copied.load(Ordering::Relaxed);
                        let bytes_total = job.total_bytes.load(Ordering::Relaxed);
                        let current_file = job.current_file.lock().unwrap().clone();

                        ui.horizontal(|ui| {
                            // Title: "Copying 3 of 12 files to D:\..."
                            let title = if files_total > 0 {
                                format!(
                                    "{} {} of {} files to {}",
                                    op,
                                    files_done + 1,
                                    files_total,
                                    job.dest_display
                                )
                            } else {
                                format!("{} … scanning …", op)
                            };
                            ui.label(egui::RichText::new(title).small());

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                // Abort button
                                if ui.small_button("✕ Abort").clicked() {
                                    job.cancelled.store(true, Ordering::SeqCst);
                                }
                                // Pause / Resume
                                let paused = job.paused.load(Ordering::Relaxed);
                                let pause_label = if paused { "▶ Resume" } else { "⏸ Pause" };
                                if ui.small_button(pause_label).clicked() {
                                    job.paused.store(!paused, Ordering::SeqCst);
                                }
                            });
                        });

                        // Current file name
                        if !current_file.is_empty() {
                            ui.label(
                                egui::RichText::new(&current_file)
                                    .small()
                                    .color(egui::Color32::GRAY),
                            );
                        }

                        // Per-file + overall progress bars
                        if bytes_total > 0 {
                            let fraction = bytes_done as f32 / bytes_total as f32;
                            let bar_text = format!(
                                "{} / {}",
                                Self::format_bytes(bytes_done),
                                Self::format_bytes(bytes_total),
                            );
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .text(bar_text)
                                    .desired_width(ui.available_width()),
                            );
                        } else {
                            // Indeterminate (scanning)
                            ui.spinner();
                        }

                        // Skipped-identical notifications (non-intrusive, gray)
                        let skipped = job.skipped_identical.lock().unwrap().clone();
                        for name in &skipped {
                            ui.label(
                                egui::RichText::new(format!("Skipped identical: {}", name))
                                    .small().color(egui::Color32::GRAY)
                            );
                        }

                        // Error display
                        if let Some(err) = job.error.lock().unwrap().as_ref() {
                            ui.colored_label(egui::Color32::RED, format!("Error: {}", err));
                        }

                        if job_idx + 1 < self.copy_jobs.len() {
                            ui.separator();
                        }
                    }
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                });
        }

        // ── File-conflict dialog ─────────────────────────────────────────────
        // Check every active job for a pending conflict query; show a modal for
        // the first one found (jobs queue so we handle them one at a time).
        if let Some(job) = self.copy_jobs.iter().find(|j| {
            j.conflict_query.lock().unwrap().is_some()
        }) {
            // Clone the info so we release the lock before building the UI.
            let info: Option<ConflictInfo> = job.conflict_query.lock().unwrap().clone();
            if let Some(ci) = info {
                // Helper: rough human-readable age from SystemTime.
                let age_str = |st: std::time::SystemTime| -> String {
                    use std::time::UNIX_EPOCH;
                    let now = std::time::SystemTime::now()
                        .duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                    let file = st.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                    if file > now {
                        return "future date".to_string();
                    }
                    let age = now - file;
                    if age < 60 { format!("{}s ago", age) }
                    else if age < 3600 { format!("{}m ago", age/60) }
                    else if age < 86400 { format!("{}h ago", age/3600) }
                    else { format!("{} days ago", age/86400) }
                };

                let src_info = format!(
                    "Source:       {}{}",
                    Self::format_bytes(ci.src_size),
                    ci.src_modified.map(|t| format!("  ({})", age_str(t))).unwrap_or_default()
                );
                let dst_info = format!(
                    "Destination:  {}{}",
                    Self::format_bytes(ci.dst_size),
                    ci.dst_modified.map(|t| format!("  ({})", age_str(t))).unwrap_or_default()
                );

                let op = if job.is_move { "Moving" } else { "Copying" };
                let title = format!("{} \"{}\" — file already exists", op, ci.file_name);

                // Measure button widths
                let btn_labels = [
                    "Overwrite this file",
                    "Skip this file",
                    "Overwrite all if different",
                    "Skip all with same name",
                    "✕  Abort",
                ];
                let font_id = egui::TextStyle::Button.resolve(&ctx.style());
                let btn_w = btn_labels.iter()
                    .map(|l| ctx.fonts(|f| f.layout_no_wrap(l.to_string(), font_id.clone(), egui::Color32::WHITE).size().x))
                    .fold(0.0f32, f32::max) + 16.0; // 8px padding each side

                let mut answer: Option<ConflictChoice> = None;

                egui::Window::new("conflict_dialog")
                    .title_bar(false)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .frame(egui::Frame {
                        fill: ctx.style().visuals.window_fill(),
                        stroke: egui::Stroke::new(1.5, egui::Color32::from_rgb(100, 140, 220)),
                        inner_margin: egui::Margin::same(12.0),
                        rounding: egui::Rounding::same(6.0),
                        ..Default::default()
                    })
                    .show(ctx, |ui| {
                        ui.set_min_width(btn_w + 24.0);

                        // Title bar
                        ui.label(egui::RichText::new(&title).strong());
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);

                        // File info
                        ui.label(egui::RichText::new(&src_info).small().monospace());
                        ui.label(egui::RichText::new(&dst_info).small().monospace());
                        ui.add_space(8.0);

                        ui.style_mut().spacing.button_padding = egui::vec2(8.0, 4.0);

                        if ui.add_sized([btn_w, 0.0], egui::Button::new("Overwrite this file")).clicked() {
                            answer = Some(ConflictChoice::Overwrite);
                        }
                        if ui.add_sized([btn_w, 0.0], egui::Button::new("Skip this file")).clicked() {
                            answer = Some(ConflictChoice::Skip);
                        }
                        ui.add_space(4.0);
                        if ui.add_sized([btn_w, 0.0], egui::Button::new("Overwrite all if different")).clicked() {
                            answer = Some(ConflictChoice::OverwriteAll);
                        }
                        if ui.add_sized([btn_w, 0.0], egui::Button::new("Skip all with same name")).clicked() {
                            answer = Some(ConflictChoice::SkipAll);
                        }
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(2.0);
                        if ui.add_sized([btn_w, 0.0],
                            egui::Button::new(egui::RichText::new("✕  Abort").color(egui::Color32::from_rgb(210, 80, 80)))
                        ).clicked() {
                            answer = Some(ConflictChoice::Abort);
                        }
                    });

                if let Some(choice) = answer {
                    *job.conflict_answer.lock().unwrap() = Some(choice);
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        egui::SidePanel::left("left_panel")
            .exact_width(self.left_panel_width)
            .resizable(false)
            .show(ctx, |ui| {
                // ── Favorites ────────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⭐ Favorites").small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new("+").small().frame(false))
                            .on_hover_text("Add current folder to favorites")
                            .clicked()
                        {
                            if !self.favorites.contains(&self.current_path) {
                                self.favorites.push(self.current_path.clone());
                                self.favorites.sort_by(|a, b| {
                                    let a_name = a.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_else(|| a.to_string_lossy().to_lowercase().into());
                                    let b_name = b.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_else(|| b.to_string_lossy().to_lowercase().into());
                                    a_name.cmp(&b_name)
                                });
                                self.config.favorites = self
                                    .favorites
                                    .iter()
                                    .map(|f| f.to_string_lossy().to_string())
                                    .collect();
                                self.config.save();
                            }
                        }
                    });
                });

                let mut remove_fav: Option<usize> = None;
                for (i, fav) in self.favorites.iter().enumerate() {
                    let name = fav
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| fav.to_string_lossy().to_string());
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        let is_cur = *fav == self.current_path;
                        let label = egui::RichText::new(&name)
                            .small()
                            .color(if is_cur { egui::Color32::WHITE } else { egui::Color32::BLACK });
                        let btn = if is_cur {
                            egui::Button::new(label).fill(egui::Color32::from_rgb(100, 150, 255)).frame(false)
                        } else {
                            egui::Button::new(label).fill(egui::Color32::from_rgb(255, 245, 150)).frame(false)
                        };
                        if ui
                            .add(btn)
                            .on_hover_text(Self::format_path_display(fav))
                            .clicked()
                        {
                            nav_from_panel = Some(fav.clone());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(egui::Button::new("×").small().frame(false)).clicked() {
                                remove_fav = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = remove_fav {
                    self.favorites.remove(i);
                    self.config.favorites = self
                        .favorites
                        .iter()
                        .map(|f| f.to_string_lossy().to_string())
                        .collect();
                    self.config.save();
                }

                ui.separator();

                // ── Folder tree ──────────────────────────────────────────
                let dnd_active = self.dnd_active;
                let dnd_drop_target = self.dnd_drop_target_prev.clone(); // use prev for display
                let dnd_sources: Vec<PathBuf> = self.dnd_sources.clone();
                let mut tree_hovered_drop: Option<PathBuf> = None;

                // Use a child_ui with a strict clip rect so the tree scroll
                // area cannot paint over the favorites section above.
                let tree_available_h = ui.available_height();
                egui::ScrollArea::vertical()
                    .id_source("tree_scroll")
                    .auto_shrink([false, false])
                    .max_height(tree_available_h)
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.set_max_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let drives: Vec<PathBuf> = self
                            .available_drives
                            .iter()
                            .map(PathBuf::from)
                            .collect();
                        let mut tree_right_clicked: Option<(PathBuf, egui::Pos2)> = None;
                        for drive in &drives {
                            render_tree_node(
                                ui,
                                drive,
                                &mut self.tree_expanded,
                                &mut self.tree_children_cache,
                                &mut nav_from_panel,
                                &self.current_path.clone(),
                                0,
                                dnd_active,
                                &dnd_sources,
                                &dnd_drop_target,
                                &mut tree_hovered_drop,
                                &mut tree_right_clicked,
                                &self.context_menu_tree_highlight.clone(),
                            );
                        }
                        if let Some((rclick_path, rclick_pos)) = tree_right_clicked {
                            let name = rclick_path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| rclick_path.to_string_lossy().to_string());
                            self.show_context_menu = true;
                            self.show_bg_context_menu = false;
                            self.context_menu_entry = Some(FileEntry {
                                name,
                                is_dir: true,
                                size: 0,
                                modified: None,
                            });
                            // Override current_path so the context menu resolves the full path
                            // by joining current_path + name. Since rclick_path already IS the
                            // full path, set current_path to its parent temporarily — or better,
                            // store the full path directly.
                            self.context_menu_tree_path = Some(rclick_path.clone());
                            self.context_menu_tree_highlight = Some(rclick_path.clone());
                            self.context_menu_position = rclick_pos;
                            // Snapshot: just this one tree path
                            self.context_menu_selection = vec![rclick_path];
                        }
                    });
                if let Some(target) = tree_hovered_drop {
                    self.dnd_drop_target = Some(target);
                }
            });

        if let Some(path) = nav_from_panel {
            self.navigate_to(path);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // ── Tab bar ──────────────────────────────────────────────────
            let mut switch_to: Option<usize> = None;
            let mut close_idx: Option<usize> = None;
            let mut open_new_tab = false;
            let mut open_save_session = false;
            let mut drag_swap: Option<(usize, usize)> = None;

            // ── Scrollable tab bar ───────────────────────────────────────
            let tab_bar_id = egui::Id::new("tab_bar_scroll");

            let tab_bar_resp = ui.horizontal(|ui| {
                // Tabs in a scroll area (without + and 💾 buttons)
                let available_w = ui.available_width() - 50.0; // reserve space for + and 💾
                let scroll_output = egui::ScrollArea::horizontal()
                    .id_source(tab_bar_id)
                    .auto_shrink([false, false])
                    .max_width(available_w)
                    .max_height(24.0)
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .drag_to_scroll(false)
                    .scroll_offset(egui::vec2(self.tab_scroll_offset, 0.0))
                    .show(ui, |ui| {
                    ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;

                    // Collect tab rects for drag-reorder hit testing
                    let mut tab_rects: Vec<egui::Rect> = Vec::with_capacity(self.tabs.len());

                    for i in 0..self.tabs.len() {
                        let is_active = i == self.active_tab;
                        let label_text = self.tabs[i].label();
                        let display = if label_text.len() > 20 {
                            format!("{}…", &label_text[..19])
                        } else {
                            label_text.clone()
                        };

                        let is_being_dragged = self.tab_drag_index == Some(i);
                        let is_dnd_hover = self.dnd_active
                            && self.dnd_tab_hover.map(|(idx, _)| idx == i).unwrap_or(false);

                        let fill = if is_being_dragged {
                            egui::Color32::from_rgb(80, 80, 100)
                        } else if is_dnd_hover {
                            egui::Color32::from_rgb(50, 110, 50) // green: drop zone active
                        } else if is_active {
                            egui::Color32::from_rgb(60, 60, 60)
                        } else {
                            egui::Color32::from_rgb(40, 40, 40)
                        };

                        let frame = egui::Frame::none()
                            .fill(fill)
                            .inner_margin(egui::Margin::symmetric(6.0, 3.0))
                            .rounding(egui::Rounding { nw: 4.0, ne: 4.0, sw: 0.0, se: 0.0 });

                        let mut close_btn_rect = egui::Rect::NOTHING;
                        let resp = frame.show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                let text_color = if is_active {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::GRAY
                                };
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&display).color(text_color).small(),
                                    ).selectable(false),
                                ).on_hover_text(Self::format_path_display(&self.tabs[i].path));

                                // Close label (only when more than 1 tab) — interaction
                                // is handled below via the single tab_sense interact so
                                // there's no competing click-sense widget inside the frame.
                                if self.tabs.len() > 1 {
                                    let close_resp = ui.add(
                                        egui::Label::new(
                                            egui::RichText::new("×").color(text_color).small(),
                                        ).selectable(false),
                                    );
                                    close_btn_rect = close_resp.rect;
                                }
                            });
                        });

                        let tab_rect = resp.response.rect;
                        tab_rects.push(tab_rect);

                        // Single interact over the whole tab rect — no competing widgets inside.
                        let tab_sense = ui.interact(
                            tab_rect,
                            egui::Id::new("tab_click").with(i),
                            egui::Sense::click(),
                        );
                        if tab_sense.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if tab_sense.clicked() {
                            let pos = tab_sense.interact_pointer_pos().unwrap_or_default();
                            if close_btn_rect != egui::Rect::NOTHING && close_btn_rect.contains(pos) {
                                close_idx = Some(i);
                            } else {
                                switch_to = Some(i);
                            }
                        }

                        // Middle-click anywhere on the tab to close
                        if tab_sense.middle_clicked() && self.tabs.len() > 1 {
                            close_idx = Some(i);
                        }
                    }

                    // Drag-reorder: detect drag-start from pointer state + tab rects
                    // (done outside the per-tab loop to avoid an overlapping ui.interact()
                    //  that would steal click events from the buttons inside each tab)
                    if self.tab_drag_index.is_none() {
                        if let Some(press_origin) = ui.input(|i| i.pointer.press_origin()) {
                            if ui.input(|i| i.pointer.primary_down()) && ui.input(|i| i.pointer.is_moving()) {
                                for (i, rect) in tab_rects.iter().enumerate() {
                                    if rect.contains(press_origin) {
                                        self.tab_drag_index = Some(i);
                                        self.tab_drag_start_x = rect.center().x;
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    // Drag-reorder: detect swap while dragging
                    if let Some(drag_idx) = self.tab_drag_index {
                        if let Some(pointer_pos) = ui.input(|i| i.pointer.hover_pos()) {
                            for (j, rect) in tab_rects.iter().enumerate() {
                                if j != drag_idx && rect.contains(pointer_pos) {
                                    drag_swap = Some((drag_idx, j));
                                    break;
                                }
                            }
                        }
                        // End drag on release
                        if !ui.input(|i| i.pointer.primary_down()) {
                            self.tab_drag_index = None;
                        }
                    }
                    tab_rects  // return so we can use for scroll-to-active
                    })  // end inner ui.horizontal
                });  // end ScrollArea

                // Double-click on empty tab-bar space → new tab
                if ui.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary)) {
                    if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                        if scroll_output.inner_rect.contains(pos) {
                            let on_tab = scroll_output.inner.inner.iter().any(|r| r.contains(pos));
                            if !on_tab {
                                open_new_tab = true;
                            }
                        }
                    }
                }

                // Persist tab rects for DnD-over-tab hit-testing (screen coordinates)
                self.dnd_tab_rects = scroll_output.inner.inner.to_vec();

                // Sync actual scroll offset (egui may have clamped it)
                self.tab_scroll_offset = scroll_output.state.offset.x;

                // Cap target so we never animate past the real maximum
                let max_scroll = (scroll_output.content_size.x - scroll_output.inner_rect.width()).max(0.0);
                self.tab_scroll_target = self.tab_scroll_target.min(max_scroll);

                // Scroll-to-active: bring the active tab into the viewport
                if self.tab_scroll_to_active {
                    let tab_rects = &scroll_output.inner.inner;
                    if let Some(active_rect) = tab_rects.get(self.active_tab) {
                        let viewport_min_x = scroll_output.inner_rect.min.x;
                        let viewport_w    = scroll_output.inner_rect.width();
                        // content_x = screen_x - viewport_min_x + current_offset
                        let content_x = active_rect.min.x - viewport_min_x + self.tab_scroll_offset;
                        // center the tab in the viewport, clamped to ≥ 0
                        let target = (content_x - viewport_w / 2.0).max(0.0);
                        self.tab_scroll_target = target;
                        self.tab_scroll_offset = target; // snap immediately on tab switch
                    }
                    self.tab_scroll_to_active = false;
                    ctx.request_repaint();
                }

                // Mouse wheel on tab bar → horizontal scroll (smooth)
                {
                    let hover = ctx.input(|i| i.pointer.hover_pos().unwrap_or_default());
                    if self.tab_bar_rect.contains(hover) || scroll_output.inner_rect.contains(hover) {
                        let dy = ctx.input(|i| i.raw_scroll_delta.y);
                        if dy != 0.0 {
                            // Each wheel notch scrolls ~60px; accumulate into target
                            self.tab_scroll_target = (self.tab_scroll_target - dy * 2.0).max(0.0);
                            ctx.request_repaint();
                        }
                    }
                }

                // Smoothly animate offset toward target (lerp 25% per frame)
                if (self.tab_scroll_offset - self.tab_scroll_target).abs() > 0.5 {
                    self.tab_scroll_offset += (self.tab_scroll_target - self.tab_scroll_offset) * 0.25;
                    ctx.request_repaint();
                } else {
                    self.tab_scroll_offset = self.tab_scroll_target;
                }

                // "+" and save buttons OUTSIDE the scroll area (pinned at end)
                ui.add_space(4.0);
                if ui
                    .add(egui::Button::new(egui::RichText::new("+").small()).frame(false))
                    .on_hover_text("New tab")
                    .clicked()
                {
                    open_new_tab = true;
                }
                ui.add_space(4.0);
                if ui
                    .add(egui::Button::new(egui::RichText::new("💾").small()).frame(false))
                    .on_hover_text("Save session")
                    .clicked()
                {
                    open_save_session = true;
                }
            });  // end outer ui.horizontal
            self.tab_bar_rect = tab_bar_resp.response.rect;

            ui.add_space(1.0);

            // Process tab actions (after the borrow of self.tabs in the loop ends)
            if let Some((from, to)) = drag_swap {
                self.save_active_tab();
                self.tabs.swap(from, to);
                // Keep active_tab in sync
                if self.active_tab == from {
                    self.active_tab = to;
                } else if self.active_tab == to {
                    self.active_tab = from;
                }
                self.tab_drag_index = Some(to);
            }
            if let Some(idx) = close_idx {
                self.close_tab(idx);
            } else if let Some(idx) = switch_to {
                self.switch_to_tab(idx);
                self.tab_scroll_to_active = true;
            }
            if open_new_tab {
                self.new_tab(None);
                self.tab_scroll_to_active = true;
            }
            if open_save_session {
                self.save_session_filename = "session.rsess".to_string();
                self.save_session_status = None;
                self.show_save_session_dialog = true;
            }

            // ── Save-session dialog ──────────────────────────────────────
            if self.show_save_session_dialog {
                let mut still_open = true;
                egui::Window::new("Save Session")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .open(&mut still_open)
                    .show(ctx, |ui| {
                        ui.label("Save current tabs to a session file.");
                        ui.label("You can restore it by running:");
                        ui.label("  rusplorer.exe <file>");
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label("File name:");
                            ui.text_edit_singleline(&mut self.save_session_filename);
                        });
                        if let Some(ref status) = self.save_session_status.clone() {
                            ui.colored_label(
                                if status.starts_with("Saved") {
                                    egui::Color32::from_rgb(50, 160, 50)
                                } else {
                                    egui::Color32::RED
                                },
                                status,
                            );
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                // Resolve path relative to exe directory
                                let exe_dir = std::env::current_exe()
                                    .ok()
                                    .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                                let save_path = exe_dir.join(&self.save_session_filename);
                                match self.save_session_to_file(&save_path, ctx) {
                                    Ok(()) => {
                                        self.save_session_status = Some(format!(
                                            "Saved to {}",
                                            save_path.display()
                                        ));
                                    }
                                    Err(e) => {
                                        self.save_session_status = Some(format!("Error: {e}"));
                                    }
                                }
                            }
                            if ui.button("Close").clicked() {
                                self.show_save_session_dialog = false;
                            }
                        });
                    });
                if !still_open {
                    self.show_save_session_dialog = false;
                }
            }

            // Drive selector with filter and navigation buttons
            let mut selected_drive: Option<PathBuf> = None;
            ui.horizontal(|ui| {
                // "Drives" toggle button
                let drives_btn_label = if self.show_drives_page { "Drives ▲" } else { "Drives ▼" };
                if ui.button(drives_btn_label).clicked() {
                    self.show_drives_page = !self.show_drives_page;
                    if self.show_drives_page {
                        // Refresh space info each time the page is opened
                        self.drives_info.clear();
                        for drive in self.available_drives.clone() {
                            let letter = drive.chars().next().unwrap_or('C');
                            let kind = Self::classify_drive(letter);
                            let (free_bytes, total_bytes) = Self::get_drive_space(&drive);
                            self.drives_info.push(DriveInfo { drive, kind, free_bytes, total_bytes });
                        }
                    }
                }
                // Drive letter mini-buttons
                for drive in &self.available_drives {
                    let current_drive = self.current_path.to_string_lossy();
                    let is_current = current_drive.starts_with(drive);
                    let drive_display = drive.trim_end_matches(|c: char| c == '\\' || c == '/').to_string();
                    let border_color = self.drive_types.get(drive).copied().unwrap_or(DriveKind::Unknown).color();
                    let font_id = egui::FontId::proportional(11.0);
                    let text_size = ui.fonts(|f| {
                        f.layout_no_wrap(drive_display.clone(), font_id.clone(), egui::Color32::WHITE).size()
                    });
                    let pad = egui::vec2(6.0, 3.0);
                    let desired = text_size + pad * 2.0;
                    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
                    if ui.is_rect_visible(rect) {
                        let painter = ui.painter();
                        let bg = if is_current {
                            egui::Color32::from_rgb(60, 120, 220)
                        } else if resp.hovered() {
                            egui::Color32::from_white_alpha(30)
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        painter.rect_filled(rect, 3.0, bg);
                        if border_color != egui::Color32::TRANSPARENT {
                            painter.rect_stroke(rect, 3.0, egui::Stroke::new(1.5, border_color));
                        }
                        let text_color = if is_current { egui::Color32::WHITE } else { ui.visuals().text_color() };
                        painter.text(rect.center(), egui::Align2::CENTER_CENTER, &drive_display, font_id, text_color);
                    }
                    if resp.clicked() {
                        selected_drive = Some(PathBuf::from(drive));
                    }
                }
                ui.label("Filter:");
                let filter_alloc = ui.allocate_ui(egui::vec2(70.0, 20.0), |ui| {
                    // Red text when a filter is active
                    if !self.filter.is_empty() {
                        ui.visuals_mut().override_text_color = Some(egui::Color32::from_rgb(255, 80, 80));
                    }
                    ui.text_edit_singleline(&mut self.filter)
                });
                self.filter_edit_rect = filter_alloc.response.rect;
                // Little X button to clear the filter
                if !self.filter.is_empty() {
                    let x_btn = ui.add(egui::Button::new(
                        egui::RichText::new("✕").size(11.0).color(egui::Color32::from_rgb(255, 80, 80))
                    ).frame(false));
                    if x_btn.clicked() {
                        self.filter.clear();
                    }
                }
                // Thumbnail / list view toggle
                let is_thumb = self.thumb_view.get(&self.current_path).copied().unwrap_or(false);
                if ui.selectable_label(is_thumb, "🖼")
                    .on_hover_text(if is_thumb { "Switch to list view" } else { "Switch to thumbnail view" })
                    .clicked()
                {
                    let new_val = !is_thumb;
                    self.thumb_view.insert(self.current_path.clone(), new_val);
                    self.config.thumb_view = self.thumb_view.iter()
                        .map(|(k, v)| (k.to_string_lossy().to_string(), *v))
                        .collect();
                    self.config.save();
                }


            });

            // Handle drive selection
            if let Some(drive) = selected_drive {
                self.navigate_to(drive);
            }

            ui.separator();

            if self.show_drives_page {
                // Fixed column offsets (inside the row rect, after 12px left pad)
                // Fixed layout constants
                const ROW_H:  f32 = 36.0;
                const PAD_X:  f32 = 12.0;
                const PAD_Y:  f32 = 6.0;

                let mut navigate_to_drive: Option<PathBuf> = None;
                egui::ScrollArea::vertical()
                    .id_source("drives_overview")
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        let avail_w = ui.available_width() - 8.0;
                        for info in &self.drives_info {
                            let border_color = info.kind.color();
                            let used  = info.total_bytes.saturating_sub(info.free_bytes);
                            let fraction = if info.total_bytes > 0 {
                                used as f32 / info.total_bytes as f32
                            } else { 0.0 };
                            let size_text = if info.total_bytes > 0 {
                                format!("{} free  /  {} total",
                                    Self::format_bytes(info.free_bytes),
                                    Self::format_bytes(info.total_bytes))
                            } else {
                                "No media".to_string()
                            };
                            let drive_label = info.drive
                                .trim_end_matches(|c: char| c == '\\' || c == '/')
                                .to_string();

                            let (row_rect, resp) = ui.allocate_exact_size(
                                egui::vec2(avail_w, ROW_H + PAD_Y * 2.0),
                                egui::Sense::click(),
                            );

                            if ui.is_rect_visible(row_rect) {
                                let p = ui.painter();
                                let inner = row_rect.shrink2(egui::vec2(0.0, PAD_Y / 2.0));

                                // Background
                                let bg = if resp.hovered() {
                                    egui::Color32::from_white_alpha(18)
                                } else {
                                    egui::Color32::from_white_alpha(5)
                                };
                                p.rect_filled(inner, 4.0, bg);

                                // Border
                                if border_color != egui::Color32::TRANSPARENT {
                                    p.rect_stroke(inner, 4.0, egui::Stroke::new(1.5, border_color));
                                }

                                let content_x = inner.min.x + PAD_X;
                                let bar_right  = inner.max.x - PAD_X;
                                let cy = inner.center().y;

                                let fid_big = egui::FontId::new(13.0, egui::FontFamily::Proportional);
                                let fid_sm  = egui::FontId::proportional(11.0);

                                // Measure widths up front (no rendering yet)
                                let letter_w = ui.fonts(|f| f.layout_no_wrap(
                                    drive_label.clone(), fid_big.clone(), egui::Color32::WHITE).size().x);
                                let type_w = ui.fonts(|f| f.layout_no_wrap(
                                    info.kind.label().to_string(), fid_sm.clone(), egui::Color32::WHITE).size().x);
                                let size_w = ui.fonts(|f| f.layout_no_wrap(
                                    size_text.clone(), fid_sm.clone(), egui::Color32::WHITE).size().x);

                                let type_color = if border_color != egui::Color32::TRANSPARENT {
                                    border_color
                                } else {
                                    egui::Color32::from_gray(160)
                                };

                                // Col 1 — drive letter
                                p.text(egui::pos2(content_x, cy),
                                    egui::Align2::LEFT_CENTER, &drive_label,
                                    fid_big, ui.visuals().text_color());

                                // Col 2 — type badge, immediately after drive letter
                                let type_x = content_x + letter_w + 8.0;
                                p.text(egui::pos2(type_x, cy),
                                    egui::Align2::LEFT_CENTER, info.kind.label(),
                                    fid_sm.clone(), type_color);

                                // Col 3 — progress bar then size text right-aligned
                                let bar_x = type_x + type_w + 10.0;
                                p.text(egui::pos2(bar_right, cy),
                                    egui::Align2::RIGHT_CENTER, &size_text,
                                    fid_sm, egui::Color32::from_gray(180));

                                let bar_max_w = (bar_right - size_w - 10.0 - bar_x).max(20.0);
                                let bar_rect = egui::Rect::from_min_size(
                                    egui::pos2(bar_x, cy - 5.0),
                                    egui::vec2(bar_max_w, 10.0),
                                );
                                // Track
                                p.rect_filled(bar_rect, 3.0, egui::Color32::from_gray(60));
                                // Fill
                                let fill_color = if fraction > 0.9 {
                                    egui::Color32::from_rgb(200, 60, 60)
                                } else {
                                    egui::Color32::from_rgb(60, 140, 220)
                                };
                                let mut fill_rect = bar_rect;
                                fill_rect.set_right(bar_rect.left() + bar_rect.width() * fraction.clamp(0.0, 1.0));
                                p.rect_filled(fill_rect, 3.0, fill_color);
                            }

                            if resp.hovered() {
                                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            if resp.clicked() {
                                navigate_to_drive = Some(PathBuf::from(&info.drive));
                            }
                            ui.add_space(4.0);
                        }
                    });
                if let Some(drive) = navigate_to_drive {
                    self.navigate_to(drive);
                    self.show_drives_page = false;
                }
            } else {

            // Breadcrumbs
            let breadcrumbs = self.get_breadcrumbs();
            let mut navigate_to_path: Option<PathBuf> = None;

            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = [5.0, 5.0].into();

                for (i, (path, name)) in breadcrumbs.iter().enumerate() {
                    let is_last = i == breadcrumbs.len() - 1;

                    if i > 0 {
                        ui.label("/");
                    }

                    if is_last {
                        // Current directory - not clickable, just plain text
                        ui.label(name);
                    } else {
                        // Parent directories - clickable pills; also valid DnD drop targets
                        let is_bc_drop = self.dnd_active
                            && self.dnd_drop_target_prev.as_ref() == Some(path);
                        let fill = if is_bc_drop {
                            egui::Color32::from_rgb(80, 200, 80)
                        } else {
                            egui::Color32::from_rgb(255, 245, 150)
                        };
                        let text_color = if is_bc_drop { egui::Color32::WHITE } else { egui::Color32::BLACK };
                        let button = egui::Button::new(
                            egui::RichText::new(name).color(text_color),
                        )
                        .fill(fill)
                        .frame(true);
                        let resp = ui.add(button);
                        // Same-frame DnD detection for breadcrumbs (use raw rect check;
                        // resp.hovered() is suppressed while a mouse button is held)
                        if self.dnd_active && !self.dnd_sources.contains(path) {
                            if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                                if resp.rect.contains(pos) {
                                    self.dnd_drop_target = Some(path.clone());
                                }
                            }
                        }
                        if resp.clicked() {
                            navigate_to_path = Some(path.clone());
                        }
                    }
                }

                // Copy path button
                if ui.button("📋").on_hover_text("Copy full path").clicked() {
                    if let Ok(mut clipboard) = Clipboard::new() {
                        let path_display = Self::format_path_display(&self.current_path);
                        let _ = clipboard.set_text(path_display);
                    }
                }
            });

            if let Some(path) = navigate_to_path {
                self.navigate_to(path);
            }

            ui.separator();

            // ── Spin-up indicator ────────────────────────────────────────────────
            // When we navigated to a slow (HDD / USB / Network) drive that was idle,
            // the content is loaded in a background thread.  While we wait, show a
            // friendly message instead of a frozen / blank window.
            if self.loading_path.is_some() {
                let t = ctx.input(|i| i.time);
                // Simple 8-frame braille spinner that cycles at ~4 fps
                let spinners = ["⣾","⣽","⣻","⢿","⡿","⣟","⣯","⣷"];
                let frame = ((t * 8.0) as usize) % spinners.len();
                let spinner = spinners[frame];
                ui.add_space(50.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{} Spinning up…", spinner))
                            .color(egui::Color32::from_gray(210))
                            .size(15.0),
                    );
                    ui.add_space(6.0);
                    let drive = self.current_path.components().next()
                        .map(|c| {
                            let s = c.as_os_str().to_string_lossy().to_string();
                            s.trim_end_matches(['\\', '/']).to_string()
                        })
                        .unwrap_or_default();
                    ui.label(
                        egui::RichText::new(format!("Waiting for {}  to respond…", drive))
                            .color(egui::Color32::from_gray(130))
                            .size(11.0),
                    );
                });
                return; // skip table rendering until entries are ready
            }

            // ── Shared pre-computation (list view + thumbnail view) ───────────────
            let is_thumb_view = self.thumb_view.get(&self.current_path).copied().unwrap_or(false);
            let filter_lower = self.filter.to_lowercase();
            let filtered_entries: Vec<FileEntry> = self
                .contents
                .iter()
                .filter(|entry| {
                    entry.name.starts_with("[..]")
                        || self.filter.is_empty()
                        || entry.name.to_lowercase().contains(&filter_lower)
                })
                .cloned()
                .collect();
            let mut entry_right_clicked = false;
            let mut sort_changed = false;

            if is_thumb_view {
                // ── THUMBNAIL GRID ───────────────────────────────────────────────
                const CELL_W: f32 = 128.0;
                const CELL_H: f32 = 152.0; // 112 thumb + 8 gap + ~32 label area
                const THUMB:  f32 = 112.0;
                const GAP:    f32 = 6.0;

                // Kick off background loading for images not yet in cache.
                {
                    let to_load: Vec<PathBuf> = filtered_entries.iter()
                        .filter(|e| !e.name.starts_with("[..]") && Self::is_image_file(&e.name))
                        .map(|e| self.current_path.join(&e.name))
                        .filter(|p| !self.thumb_cache.contains_key(p) && !self.thumb_loading.contains(p))
                        .collect();
                    for p in &to_load { self.thumb_loading.insert(p.clone()); }
                    let tx = self.thumb_loader_tx.clone();
                    for p in to_load {
                        let tx2 = tx.clone();
                        std::thread::spawn(move || {
                            if let Ok(img) = image::open(&p) {
                                let thumb = img.thumbnail(120, 120);
                                let rgba  = thumb.to_rgba8();
                                let (w, h) = rgba.dimensions();
                                let ci = egui::ColorImage::from_rgba_unmultiplied(
                                    [w as usize, h as usize], &rgba.into_raw());
                                let _ = tx2.send((p, ci));
                            }
                        });
                    }
                }

                // DnD drop-target detection (last frame's rects).
                if self.dnd_active {
                    if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                        if let Some(found) = self.entry_rects.iter().find_map(|(name, rect)| {
                            if rect.contains(pos) {
                                let full = self.current_path.join(name);
                                if full.is_dir() && !self.dnd_sources.contains(&full) { Some(full) } else { None }
                            } else { None }
                        }) {
                            self.dnd_drop_target = Some(found);
                        }
                    }
                }
                self.entry_rects.clear();
                self.any_button_hovered = false;

                let thumb_entries: Vec<&FileEntry> = filtered_entries.iter()
                    .filter(|e| !e.name.starts_with("[..]")
                    ).collect();
                let avail_w = ui.available_width();
                let col_count = ((avail_w + GAP) / (CELL_W + GAP)).max(1.0) as usize;

                egui::ScrollArea::vertical()
                    .id_source("thumb_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for chunk in thumb_entries.chunks(col_count) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = GAP;
                                for entry in chunk {
                                    let full_path = self.current_path.join(&entry.name);
                                    let is_selected = self.selected_entries.contains(&entry.name);
                                    let is_drop_target = self.dnd_active && entry.is_dir
                                        && self.dnd_drop_target_prev.as_ref() == Some(&full_path);
                                    let (cell_rect, resp) = ui.allocate_exact_size(
                                        egui::vec2(CELL_W, CELL_H),
                                        egui::Sense::click_and_drag(),
                                    );
                                    self.entry_rects.insert(entry.name.clone(), cell_rect);

                                    if ui.is_rect_visible(cell_rect) {
                                        let p = ui.painter();
                                        let bg = if is_drop_target {
                                            egui::Color32::from_rgb(80, 200, 80)
                                        } else if is_selected {
                                            egui::Color32::from_rgb(80, 130, 220)
                                        } else if resp.hovered() {
                                            egui::Color32::from_white_alpha(18)
                                        } else {
                                            egui::Color32::TRANSPARENT
                                        };
                                        if bg != egui::Color32::TRANSPARENT {
                                            p.rect_filled(cell_rect, 6.0, bg);
                                        }

                                        let thumb_rect = egui::Rect::from_min_size(
                                            egui::pos2(
                                                cell_rect.min.x + (CELL_W - THUMB) / 2.0,
                                                cell_rect.min.y + 4.0,
                                            ),
                                            egui::vec2(THUMB, THUMB),
                                        );

                                        if Self::is_image_file(&entry.name) {
                                            if let Some(tex) = self.thumb_cache.get(&full_path) {
                                                let [tw, th] = tex.size();
                                                let draw_rect = egui::Rect::from_center_size(
                                                    thumb_rect.center(),
                                                    egui::vec2(tw as f32, th as f32),
                                                );
                                                p.image(
                                                    tex.id(), draw_rect,
                                                    egui::Rect::from_min_max(
                                                        egui::pos2(0.0, 0.0),
                                                        egui::pos2(1.0, 1.0),
                                                    ),
                                                    egui::Color32::WHITE,
                                                );
                                            } else {
                                                // Loading placeholder
                                                p.rect_filled(thumb_rect, 4.0, egui::Color32::from_gray(45));
                                                let spinners = ["⣾","⣽","⣻","⢿","⡿","⣟","⣯","⣷"];
                                                let fi = ((ctx.input(|i| i.time) * 8.0) as usize) % spinners.len();
                                                p.text(
                                                    thumb_rect.center(), egui::Align2::CENTER_CENTER,
                                                    spinners[fi], egui::FontId::proportional(16.0),
                                                    egui::Color32::from_gray(140),
                                                );
                                            }
                                        } else if Self::is_video_file(&entry.name) {
                                            p.rect_filled(thumb_rect, 4.0, egui::Color32::from_gray(35));
                                            p.text(
                                                thumb_rect.center(), egui::Align2::CENTER_CENTER,
                                                "🎦", egui::FontId::proportional(36.0),
                                                egui::Color32::from_gray(180),
                                            );
                                        } else if entry.is_dir {
                                            p.text(
                                                thumb_rect.center(), egui::Align2::CENTER_CENTER,
                                                "📁", egui::FontId::proportional(36.0),
                                                egui::Color32::from_rgb(255, 245, 150),
                                            );
                                        } else {
                                            p.text(
                                                thumb_rect.center(), egui::Align2::CENTER_CENTER,
                                                "📄", egui::FontId::proportional(36.0),
                                                egui::Color32::from_gray(200),
                                            );
                                        }

                                        // Filename label (truncated)
                                        let chars: Vec<char> = entry.name.chars().collect();
                                        let name_display = if chars.len() > 16 {
                                            format!("{}\u{2026}", chars[..15].iter().collect::<String>())
                                        } else {
                                            entry.name.clone()
                                        };
                                        p.text(
                                            egui::pos2(cell_rect.center().x, cell_rect.min.y + THUMB + 8.0),
                                            egui::Align2::CENTER_TOP, &name_display,
                                            egui::FontId::proportional(11.0),
                                            if is_selected { egui::Color32::WHITE } else { egui::Color32::from_gray(210) },
                                        );
                                    }

                                    // Click handling
                                    if resp.clicked() {
                                        let is_ctrl  = ctx.input(|i| i.modifiers.ctrl);
                                        let is_shift = ctx.input(|i| i.modifiers.shift);
                                        if is_ctrl {
                                            if !self.selected_entries.remove(&entry.name) {
                                                self.selected_entries.insert(entry.name.clone());
                                            }
                                        } else if is_shift {
                                            if let Some(ref anchor) = self.last_clicked_entry.clone() {
                                                let ai = thumb_entries.iter().position(|e| e.name == *anchor);
                                                let bi = thumb_entries.iter().position(|e| e.name == entry.name);
                                                if let (Some(a), Some(b)) = (ai, bi) {
                                                    self.selected_entries.clear();
                                                    for i in a.min(b)..=a.max(b) {
                                                        self.selected_entries.insert(thumb_entries[i].name.clone());
                                                    }
                                                }
                                            } else {
                                                self.selected_entries.clear();
                                                self.selected_entries.insert(entry.name.clone());
                                            }
                                            self.last_clicked_entry = Some(entry.name.clone());
                                        } else {
                                            self.selected_entries.clear();
                                            self.selected_entries.insert(entry.name.clone());
                                            self.last_clicked_entry = Some(entry.name.clone());
                                        }
                                    }

                                    if resp.double_clicked() {
                                        if entry.is_dir {
                                            self.selected_action = Some(FileAction::OpenDir(
                                                self.current_path.join(&entry.name)));
                                        } else {
                                            #[cfg(windows)]
                                            let _ = std::process::Command::new("explorer")
                                                .arg(&full_path).spawn();
                                        }
                                    }

                                    // Right-click context menu (skip when right-drag DnD active)
                                    let raw_sec = !self.dnd_is_right_click
                                        && self.dnd_suppress == 0
                                        && ctx.input(|i| i.pointer.secondary_released())
                                        && ctx.input(|i| i.pointer.hover_pos()
                                            .map_or(false, |pos| cell_rect.contains(pos)));
                                    if raw_sec {
                                        if !self.selected_entries.contains(&entry.name) {
                                            self.selected_entries.clear();
                                            self.selected_entries.insert(entry.name.clone());
                                        }
                                        self.context_menu_selection = self.selected_entries.iter()
                                            .map(|n| self.current_path.join(n)).collect();
                                        self.show_context_menu = true;
                                        self.show_bg_context_menu = false;
                                        self.context_menu_entry = Some((*entry).clone());
                                        self.context_menu_tree_path = None;
                                        self.context_menu_position =
                                            ctx.input(|i| i.pointer.hover_pos().unwrap_or_default());
                                        entry_right_clicked = true;
                                    }

                                    // DnD drag initiation
                                    let primary_down   = ctx.input(|i| i.pointer.primary_down());
                                    let secondary_down = ctx.input(|i| i.pointer.button_down(egui::PointerButton::Secondary));
                                    let any_btn   = primary_down || secondary_down;
                                    let cursor_over = ctx.input(|i| i.pointer.hover_pos()
                                        .map_or(false, |pos| cell_rect.contains(pos)));
                                    if cursor_over { self.any_button_hovered = true; }
                                    if cursor_over && any_btn && !self.dnd_active && self.dnd_suppress == 0
                                        && !self.is_dragging_selection && self.dnd_start_pos.is_none()
                                        && !self.ole_drag_in_active.load(Ordering::SeqCst)
                                    {
                                        self.dnd_start_pos = ctx.input(|i| i.pointer.hover_pos());
                                        self.dnd_drag_entry = Some(entry.name.clone());
                                        self.dnd_is_right_click = secondary_down;
                                    }
                                    if !self.dnd_active && !any_btn {
                                        self.dnd_start_pos = None;
                                        self.dnd_drag_entry = None;
                                        self.dnd_is_right_click = false;
                                    }
                                    if any_btn && self.dnd_drag_entry.as_deref() == Some(&entry.name)
                                        && !self.dnd_active && !self.is_dragging_selection
                                    {
                                        if let (Some(start), Some(cur)) = (
                                            self.dnd_start_pos,
                                            ctx.input(|i| i.pointer.hover_pos()),
                                        ) {
                                            if start.distance(cur) > 5.0 {
                                                if self.selected_entries.contains(&entry.name) {
                                                    self.dnd_sources = self.selected_entries.iter()
                                                        .map(|n| self.current_path.join(n)).collect();
                                                } else {
                                                    self.dnd_sources = vec![full_path.clone()];
                                                    self.selected_entries.clear();
                                                    self.selected_entries.insert(entry.name.clone());
                                                }
                                                let cnt = self.dnd_sources.len();
                                                self.dnd_label = if cnt == 1 {
                                                    format!("📄 {}", entry.name)
                                                } else {
                                                    format!("📦 {} items", cnt)
                                                };
                                                self.dnd_active = true;
                                                if self.dnd_is_right_click {
                                                    self.dnd_label = format!("{}  [Move / Copy / Shortcut]", self.dnd_label);
                                                }
                                            }
                                        }
                                    }
                                }
                            });
                            ui.add_space(GAP);
                        }
                    });

            } else {
            // ── TABLE (list view) ────────────────────────────────────────────────
            // Table with proper column alignment
            let show_dates = self
                .show_date_columns
                .get(&self.current_path)
                .copied()
                .unwrap_or(false);

            // Pre-compute time values once per frame (avoids per-row Windows API calls)
            let now = SystemTime::now();
            #[cfg(windows)]
            let tz_bias_secs: i64 = {
                use winapi::um::timezoneapi::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
                let mut tzi: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
                let tz_id = unsafe { GetTimeZoneInformation(&mut tzi) };
                let is_dst = tz_id == 2;
                (tzi.Bias + if is_dst { tzi.DaylightBias } else { tzi.StandardBias }) as i64 * 60
            };
            #[cfg(not(windows))]
            let tz_bias_secs: i64 = 0;

            // Same-frame DnD detection for file-table entries
            if self.dnd_active {
                let cursor = ctx.input(|i| i.pointer.hover_pos());
                if let Some(pos) = cursor {
                    if let Some(found) = self.entry_rects.iter().find_map(|(name, rect)| {
                        if rect.contains(pos) {
                            let is_parent = name.starts_with("[..]");
                            let full = if is_parent {
                                self.current_path.parent()?.to_path_buf()
                            } else {
                                self.current_path.join(name)
                            };
                            let is_dir = is_parent || full.is_dir();
                            let is_source = self.dnd_sources.contains(&full);
                            if is_dir && !is_source { Some(full) } else { None }
                        } else {
                            None
                        }
                    }) {
                        self.dnd_drop_target = Some(found);
                    }
                }
            }

            // Clear rect map for this frame
            self.entry_rects.clear();
            self.any_button_hovered = false;

            let row_height = 18.0;

            // Measure actual text widths for tight columns
            let font_id = egui::TextStyle::Body.resolve(ui.style());

            // Find the widest size label in current contents
            let max_size_str = self
                .contents
                .iter()
                .filter_map(|entry| {
                    if entry.name.starts_with("[..]") {
                        None
                    } else {
                        let full_path = self.current_path.join(&entry.name);
                        self.file_sizes
                            .get(&full_path)
                            .map(|size| Self::format_file_size(*size))
                            .or(Some(if entry.is_dir {
                                "0 B".to_string()
                            } else {
                                "...".to_string()
                            }))
                    }
                })
                .max_by_key(|s| s.len())
                .unwrap_or_else(|| "0 B".to_string());

            let size_text_width = ui.fonts(|f| {
                f.layout_no_wrap(max_size_str, font_id.clone(), egui::Color32::WHITE)
                    .size()
                    .x
            });

            // Check if any directories are still computing
            let has_computing = self.contents.iter().any(|entry| {
                if entry.is_dir && !entry.name.starts_with("[..]") {
                    let full_path = self.current_path.join(&entry.name);
                    !self.dirs_done.contains(&full_path)
                } else {
                    false
                }
            });

            let hourglass_width = if has_computing {
                ui.fonts(|f| {
                    f.layout_no_wrap("⏳".to_string(), font_id.clone(), egui::Color32::WHITE)
                        .size()
                        .x
                })
            } else {
                0.0
            };

            let date_text_width = if show_dates {
                ui.fonts(|f| {
                    f.layout_no_wrap(
                        "2026-02-17 14:30".to_string(),
                        font_id.clone(),
                        egui::Color32::WHITE,
                    )
                    .size()
                    .x
                })
            } else {
                0.0
            };

            // Calculate exact column widths from available space
            let available = ui.available_width();
            let size_col_w = size_text_width + hourglass_width + 7.0; // text + spinner (if any) + padding
            let date_col_w = if show_dates {
                date_text_width + 20.0
            } else {
                18.0
            }; // +20 for X button + padding
            let name_col_w = (available - size_col_w - date_col_w - 15.0).max(50.0);

            let num_rows = filtered_entries.len();

            let table_builder = TableBuilder::new(ui)
                .striped(true)
                .resizable(false)
                .vscroll(true)
                .drag_to_scroll(false)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(name_col_w).clip(true))
                .column(Column::exact(size_col_w))
                .column(Column::exact(date_col_w));

            table_builder
                .header(row_height, |mut header| {
                    // Name header
                    header.col(|ui| {
                        let arrow = if self.sort_column == SortColumn::Name {
                            if self.sort_ascending { " ↑" } else { " ↓" }
                        } else {
                            ""
                        };
                        let text = format!("Name{}", arrow);
                        if ui
                            .add_sized(
                                ui.available_size(),
                                egui::Button::new(egui::RichText::new(&text).strong()),
                            )
                            .clicked()
                        {
                            if self.sort_column == SortColumn::Name {
                                self.sort_ascending = !self.sort_ascending;
                            } else {
                                self.sort_column = SortColumn::Name;
                                self.sort_ascending = true;
                            }
                            sort_changed = true;
                        }
                    });

                    // Size header
                    header.col(|ui| {
                        let arrow = if self.sort_column == SortColumn::Size {
                            if self.sort_ascending { " ↑" } else { " ↓" }
                        } else {
                            ""
                        };
                        let text = format!("Size{}", arrow);
                        if ui
                            .add_sized(
                                ui.available_size(),
                                egui::Button::new(egui::RichText::new(&text).strong()),
                            )
                            .clicked()
                        {
                            if self.sort_column == SortColumn::Size {
                                self.sort_ascending = !self.sort_ascending;
                            } else {
                                self.sort_column = SortColumn::Size;
                                self.sort_ascending = false;
                            }
                            sort_changed = true;
                        }
                    });

                    // Date header
                    header.col(|ui| {
                        if show_dates {
                            ui.horizontal(|ui| {
                                if ui
                                    .small_button("X")
                                    .on_hover_text("Hide date column")
                                    .clicked()
                                {
                                    self.show_date_columns
                                        .insert(self.current_path.clone(), false);
                                    if self.sort_column == SortColumn::Date {
                                        self.sort_column = SortColumn::Name;
                                        self.sort_ascending = true;
                                    }
                                    sort_changed = true;
                                }
                                let arrow = if self.sort_column == SortColumn::Date {
                                    if self.sort_ascending { " ↑" } else { " ↓" }
                                } else {
                                    ""
                                };
                                let text = format!("Modified{}", arrow);
                                if ui
                                    .add_sized(
                                        egui::vec2(ui.available_width(), ui.available_height()),
                                        egui::Button::new(egui::RichText::new(&text).strong()),
                                    )
                                    .clicked()
                                {
                                    if self.sort_column == SortColumn::Date {
                                        self.sort_ascending = !self.sort_ascending;
                                    } else {
                                        self.sort_column = SortColumn::Date;
                                        self.sort_ascending = false;
                                    }
                                    sort_changed = true;
                                }
                            });
                        } else {
                            if ui
                                .small_button("📅")
                                .on_hover_text("Show modification date")
                                .clicked()
                            {
                                self.show_date_columns
                                    .insert(self.current_path.clone(), true);
                                self.sort_column = SortColumn::Date;
                                self.sort_ascending = false;
                                sort_changed = true;
                            }
                        }
                    });
                })
                .body(|body| {
                    body.rows(row_height, num_rows, |mut row| {
                        let entry = &filtered_entries[row.index()];

                        let is_selected = self.selected_entries.contains(&entry.name);
                        let is_in_clipboard = self
                            .clipboard_files
                            .contains(&self.current_path.join(&entry.name));
                        let full_path = self.current_path.join(&entry.name);
                        let is_computing = entry.is_dir
                            && !entry.name.starts_with("[..]")
                            && !self.dirs_done.contains(&full_path);

                        let size_label = if entry.name.starts_with("[..]") {
                            String::new()
                        } else {
                            match self.file_sizes.get(&full_path) {
                                Some(size) => Self::format_file_size(*size),
                                None => {
                                    if entry.is_dir {
                                        "0 B".to_string()
                                    } else {
                                        "...".to_string()
                                    }
                                }
                            }
                        };

                        // Determine if this folder is a drop target
                        let entry_abs = if entry.name.starts_with("[..]") {
                            self.current_path.parent().map(|p| p.to_path_buf())
                        } else {
                            Some(self.current_path.join(&entry.name))
                        };
                        let is_drop_target = self.dnd_active
                            && entry.is_dir
                            && entry_abs.as_ref() == self.dnd_drop_target_prev.as_ref();

                        // Name column
                        row.col(|ui| {
                                let col_width = ui.available_width();
                                let text_color = ui.visuals().text_color();
                                let dark_mode = ui.visuals().dark_mode;

                                let button = if is_drop_target {
                                    egui::Button::new(
                                        Self::name_layout_job(&entry.name, entry.is_dir, egui::Color32::WHITE, false, dark_mode)
                                    )
                                    .fill(egui::Color32::from_rgb(80, 200, 80))
                                    .frame(false)
                                } else if is_selected && is_in_clipboard {
                                    egui::Button::new(
                                        Self::name_layout_job(&entry.name, entry.is_dir, egui::Color32::WHITE, true, dark_mode)
                                    )
                                    .fill(egui::Color32::from_rgb(100, 150, 255))
                                    .frame(false)
                                } else if is_selected {
                                    egui::Button::new(
                                        Self::name_layout_job(&entry.name, entry.is_dir, egui::Color32::WHITE, false, dark_mode)
                                    )
                                    .fill(egui::Color32::from_rgb(100, 150, 255))
                                    .frame(false)
                                } else if is_in_clipboard && entry.is_dir {
                                    egui::Button::new(
                                        Self::name_layout_job(&entry.name, true, egui::Color32::from_gray(20), true, dark_mode)
                                    )
                                    .fill(egui::Color32::from_rgb(255, 245, 150))
                                    .frame(false)
                                } else if is_in_clipboard {
                                    egui::Button::new(
                                        Self::name_layout_job(&entry.name, entry.is_dir, text_color, true, dark_mode)
                                    )
                                    .frame(false)
                                } else if entry.name.starts_with("[..]") {
                                    egui::Button::new(&entry.name)
                                        .fill(egui::Color32::TRANSPARENT)
                                        .frame(false)
                                } else if entry.is_dir {
                                    egui::Button::new(
                                        Self::name_layout_job(&entry.name, true, egui::Color32::from_gray(20), false, dark_mode)
                                    )
                                    .fill(egui::Color32::from_rgb(255, 245, 150))
                                    .frame(false)
                                } else {
                                    egui::Button::new(
                                        Self::name_layout_job(&entry.name, false, text_color, false, dark_mode)
                                    )
                                    .frame(false)
                                };

                                let button = button.sense(egui::Sense::click_and_drag());
                                let response = ui.horizontal(|ui| ui.add(button)).inner;

                                self.entry_rects.insert(entry.name.clone(), response.rect);
                                // Use direct cursor-rect check so hover works even during drag
                                let cursor_over = ui.input(|i| {
                                    i.pointer.hover_pos().map_or(false, |p| response.rect.contains(p))
                                });
                                if cursor_over || response.hovered() {
                                    self.any_button_hovered = true;
                                }

                                // Drag-and-drop: raw pointer state detection
                                // (avoids egui's drag_started_by/dragged_by which desync
                                //  after the blocking DoDragDrop OLE call)
                                let primary_down = ui.input(|i| i.pointer.primary_down());
                                let secondary_down = ui.input(|i| i.pointer.button_down(egui::PointerButton::Secondary));
                                let any_btn_down = primary_down || secondary_down;

                                // Detect new press on this entry
                                if cursor_over
                                    && any_btn_down
                                    && !self.dnd_active
                                    && self.dnd_suppress == 0
                                    && !self.is_dragging_selection
                                    && self.dnd_start_pos.is_none()
                                    && !entry.name.starts_with("[..]")
                                    && !self.ole_drag_in_active.load(Ordering::SeqCst)
                                {
                                    self.dnd_start_pos = ui.input(|i| i.pointer.hover_pos());
                                    self.dnd_drag_entry = Some(entry.name.clone());
                                    self.dnd_is_right_click = secondary_down;
                                }

                                // Clear stale press when pointer is released without triggering a drag
                                if !self.dnd_active && !any_btn_down {
                                    self.dnd_start_pos = None;
                                    self.dnd_drag_entry = None;
                                    self.dnd_is_right_click = false;
                                }

                                // Activate DnD when dragged far enough with button held
                                let is_this_entry_drag = any_btn_down
                                    && self.dnd_drag_entry.as_deref() == Some(&entry.name);
                                if is_this_entry_drag
                                    && !self.dnd_active
                                    && !self.is_dragging_selection
                                    && !entry.name.starts_with("[..]")
                                {
                                    if let Some(start) = self.dnd_start_pos {
                                        if let Some(current) = ui.input(|i| i.pointer.hover_pos()) {
                                            if start.distance(current) > 5.0 {
                                                // Start drag
                                                if self.selected_entries.contains(&entry.name) {
                                                    // Drag all selected entries
                                                    self.dnd_sources = self
                                                        .selected_entries
                                                        .iter()
                                                        .map(|n| self.current_path.join(n))
                                                        .collect();
                                                } else {
                                                    // Drag just this entry
                                                    self.dnd_sources = vec![self.current_path.join(&entry.name)];
                                                    self.selected_entries.clear();
                                                    self.selected_entries.insert(entry.name.clone());
                                                }
                                                let count = self.dnd_sources.len();
                                                self.dnd_label = if count == 1 {
                                                    if entry.is_dir {
                                                        format!("📁 {}", &entry.name)
                                                    } else {
                                                        format!("📄 {}", &entry.name)
                                                    }
                                                } else {
                                                    format!("📦 {} items", count)
                                                };
                                                self.dnd_active = true;
                                                // Show move/copy hint in label for right-click drag
                                                if self.dnd_is_right_click {
                                                    self.dnd_label = format!("{}  [Move / Copy / Shortcut]", self.dnd_label);
                                                }
                                            }
                                        }
                                    }
                                }

                                if response.clicked() {
                                    let is_ctrl = ui.input(|i| i.modifiers.ctrl);
                                    let is_shift = ui.input(|i| i.modifiers.shift);
                                    if is_shift {
                                        // Range select: from last_clicked_entry to this entry
                                        if let Some(ref anchor) = self.last_clicked_entry {
                                            let anchor_idx = filtered_entries.iter().position(|e| e.name == *anchor);
                                            let click_idx = filtered_entries.iter().position(|e| e.name == entry.name);
                                            if let (Some(a), Some(b)) = (anchor_idx, click_idx) {
                                                let lo = a.min(b);
                                                let hi = a.max(b);
                                                if !is_ctrl {
                                                    self.selected_entries.clear();
                                                }
                                                for i in lo..=hi {
                                                    let name = &filtered_entries[i].name;
                                                    if !name.starts_with("[..]") {
                                                        self.selected_entries.insert(name.clone());
                                                    }
                                                }
                                            }
                                        } else {
                                            // No anchor yet — treat as normal click
                                            self.selected_entries.clear();
                                            self.selected_entries.insert(entry.name.clone());
                                            self.last_clicked_entry = Some(entry.name.clone());
                                        }
                                        // Don't update anchor on shift-click (allows extending)
                                    } else if is_ctrl {
                                        if self.selected_entries.contains(&entry.name) {
                                            self.selected_entries.remove(&entry.name);
                                        } else {
                                            self.selected_entries.insert(entry.name.clone());
                                        }
                                        self.last_clicked_entry = Some(entry.name.clone());
                                    } else {
                                        self.selected_entries.clear();
                                        self.selected_entries.insert(entry.name.clone());
                                        self.last_clicked_entry = Some(entry.name.clone());
                                    }
                                }

                                // Use raw pointer-position check instead of response.secondary_clicked()
                                // because secondary_clicked() relies on hovered() which returns false
                                // when a Foreground Area (bg context menu) overlaps this entry.
                                let raw_secondary = !self.dnd_is_right_click
                                    && self.dnd_suppress == 0
                                    && ctx.input(|i| i.pointer.secondary_released())
                                    && ctx.input(|i| {
                                        i.pointer.hover_pos()
                                            .map_or(false, |p| response.rect.contains(p))
                                    });

                                if raw_secondary {
                                    // Select the right-clicked entry if not already part of selection
                                    if !self.selected_entries.contains(&entry.name) {
                                        self.selected_entries.clear();
                                        self.selected_entries.insert(entry.name.clone());
                                    }
                                    // Snapshot the selection NOW before any click-through can clear it
                                    self.context_menu_selection = self
                                        .selected_entries
                                        .iter()
                                        .map(|n| self.current_path.join(n))
                                        .collect();
                                    self.show_context_menu = true;
                                    self.show_bg_context_menu = false;
                                    self.context_menu_entry = Some(entry.clone());
                                    self.context_menu_tree_path = None; // file list: use current_path + name
                                    self.context_menu_position =
                                        ui.input(|i| i.pointer.hover_pos().unwrap_or_default());
                                    entry_right_clicked = true;
                                }

                                if response.double_clicked() {
                                    if entry.name.starts_with("[..]") {
                                        self.selected_action = Some(FileAction::GoToParent);
                                    } else if entry.is_dir {
                                        let new_path = self.current_path.join(&entry.name);
                                        self.selected_action = Some(FileAction::OpenDir(new_path));
                                    } else {
                                        let full_path = self.current_path.join(&entry.name);
                                        // Resolve .lnk shortcuts
                                        #[cfg(windows)]
                                        let resolved = if entry.name
                                            .to_lowercase()
                                            .ends_with(".lnk")
                                        {
                                            resolve_lnk(&full_path)
                                        } else {
                                            None
                                        };
                                        #[cfg(not(windows))]
                                        let resolved: Option<PathBuf> = None;
                                        if let Some(target) = resolved {
                                            if target.is_dir() {
                                                self.selected_action =
                                                    Some(FileAction::OpenDir(target));
                                            } else {
                                                let _ = std::process::Command::new("explorer")
                                                    .arg(&target)
                                                    .spawn();
                                            }
                                        } else {
                                            let _ = std::process::Command::new("explorer")
                                                .arg(&full_path)
                                                .spawn();
                                        }
                                    }
                                }

                                // Draw size bar at bottom of cell
                                if !entry.name.starts_with("[..]") {
                                    if let Some(size) = self.file_sizes.get(&full_path) {
                                        let bar_width = if self.max_file_size > 0 {
                                            (*size as f32 / self.max_file_size as f32) * col_width
                                        } else {
                                            0.0
                                        };
                                        let bar_rect = egui::Rect::from_min_size(
                                            egui::pos2(
                                                response.rect.left(),
                                                response.rect.bottom() - 2.0,
                                            ),
                                            egui::vec2(bar_width, 1.0),
                                        );
                                        ui.painter().rect_filled(
                                            bar_rect,
                                            0.0,
                                            egui::Color32::from_rgb(100, 150, 255),
                                        );
                                    }
                                }
                            });

                            // Size column - right aligned, no extra padding
                            row.col(|ui| {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if !size_label.is_empty() {
                                            let size_text = if is_in_clipboard {
                                                egui::RichText::new(&size_label).weak().italics()
                                            } else {
                                                egui::RichText::new(&size_label).weak()
                                            };
                                            ui.label(size_text);
                                        }
                                        if is_computing {
                                            if self.is_focused {
                                                // Animated hourglass while computing
                                                let spinner_chars = ['⏳', '⌛'];
                                                let time = ui.input(|i| i.time);
                                                let idx =
                                                    ((time * 2.0) as usize) % spinner_chars.len();
                                                ui.label(spinner_chars[idx].to_string());
                                                ctx.request_repaint();
                                            } else {
                                                // Static hourglass when paused (window unfocused)
                                                ui.label("⏳");
                                            }
                                        }
                                    },
                                );
                            });

                            // Date column - right aligned, tight
                            row.col(|ui| {
                                if show_dates && !entry.name.starts_with("[..]") {
                                    let date_text = if let Some(modified) = entry.modified {
                                        Self::format_modified_time(modified, tz_bias_secs)
                                    } else {
                                        String::new()
                                    };
                                    if !date_text.is_empty() {
                                        // Paint age-based background color
                                        let is_future = entry.modified
                                            .map(|m| m > now)
                                            .unwrap_or(false);
                                        if let Some(modified) = entry.modified {
                                            let bg = Self::age_color(modified, now);
                                            ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
                                        }
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                let text_color = if is_future {
                                                    egui::Color32::WHITE
                                                } else {
                                                    egui::Color32::from_rgb(60, 60, 60)
                                                };
                                                let label = if is_in_clipboard {
                                                    egui::RichText::new(&date_text).color(text_color).italics()
                                                } else {
                                                    egui::RichText::new(&date_text).color(text_color)
                                                };
                                                ui.label(label);
                                            },
                                        );
                                    }
                                }
                            });
                        });
                    });

            } // end else (list view)

            // Background right-click: open menu only when no entry was clicked
            // and no context menu was already opened this frame (e.g. from tree panel)
            if !entry_right_clicked && !self.dnd_active && self.dnd_suppress == 0 && !self.show_context_menu {
                if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                    if !self.tab_bar_rect.contains(pos)
                        && ctx.input(|i| i.pointer.secondary_released())
                    {
                        self.show_bg_context_menu = true;
                        self.show_context_menu = false;
                        self.bg_context_position = pos;
                    }
                }
            }

            // Handle rectangular selection (only when not dragging files or tabs,
            // not hovering the tab bar area at the top of the panel,
            // and no modal dialog is open — e.g. rename suppresses rubber-band)
            if !self.dnd_active && self.tab_drag_index.is_none()
                && !self.show_rename_dialog
                && !self.show_new_item_dialog
            {            ctx.input(|i| {
                if let Some(pointer_pos) = i.pointer.hover_pos() {
                    let in_tab_bar = self.tab_bar_rect.contains(pointer_pos);
                    if i.pointer.primary_pressed() && !self.any_button_hovered && !in_tab_bar
                        && !self.filter_edit_rect.contains(pointer_pos)
                    {
                        self.is_dragging_selection = true;
                        self.selection_drag_start = Some(pointer_pos);
                        self.selection_drag_current = Some(pointer_pos);
                        self.selection_before_drag = self.selected_entries.clone();
                        // Cancel any pending DnD start so rubber-band takes priority
                        self.dnd_start_pos = None;
                        self.dnd_drag_entry = None;
                    }
                    if self.is_dragging_selection && i.pointer.primary_down() {
                        self.selection_drag_current = Some(pointer_pos);
                        if let (Some(start), Some(end)) =
                            (self.selection_drag_start, self.selection_drag_current)
                        {
                            let sel_rect = egui::Rect::from_two_pos(start, end);
                            if i.modifiers.ctrl {
                                self.selected_entries = self.selection_before_drag.clone();
                            } else {
                                self.selected_entries.clear();
                            }
                            for (name, rect) in &self.entry_rects {
                                if sel_rect.intersects(*rect) && !name.starts_with("[..]") {
                                    self.selected_entries.insert(name.clone());
                                }
                            }
                        }
                    }
                    if self.is_dragging_selection && !i.pointer.primary_down() {
                        self.is_dragging_selection = false;
                        self.selection_drag_start = None;
                        self.selection_drag_current = None;
                        self.selection_before_drag.clear();
                    }
                }
            });
            } // end if !self.dnd_active && tab_drag_index.is_none()

            // Cancel any active rubber-band if a modal dialog just opened
            if self.show_rename_dialog || self.show_new_item_dialog {
                self.is_dragging_selection = false;
                self.selection_drag_start = None;
                self.selection_drag_current = None;
            }

            if sort_changed {
                self.sort_contents();
                self.config.sort_column = self.sort_column.clone();
                self.config.sort_ascending = self.sort_ascending;
                self.config.show_date_columns = self
                    .show_date_columns
                    .iter()
                    .map(|(k, v)| (k.to_string_lossy().to_string(), *v))
                    .collect();
                self.config.save();
            }
            } // end else !show_drives_page

            // Draw selection rectangle if dragging
            if let (Some(start), Some(current)) =
                (self.selection_drag_start, self.selection_drag_current)
            {
                let sel_rect = egui::Rect::from_two_pos(start, current);
                ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("selection_rect"),
                ))
                .rect_stroke(
                    sel_rect,
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(100, 150, 255)),
                );
                ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("selection_rect"),
                ))
                .rect_filled(
                    sel_rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(100, 150, 255, 30),
                );
            }

            // Handle drag-and-drop: detect release and perform action
            if self.dnd_active {
                let left_down = ctx.input(|i| i.pointer.primary_down());
                let right_down = ctx.input(|i| i.pointer.button_down(egui::PointerButton::Secondary));
                let pointer_down = if self.dnd_is_right_click { right_down } else { left_down };
                let hover_pos = ctx.input(|i| i.pointer.hover_pos());

                // Hover-to-switch: auto-switch to a tab when dragging over it for ≥500 ms
                {
                    let hovered_tab = hover_pos.and_then(|pos| {
                        self.dnd_tab_rects.iter().enumerate()
                            .find_map(|(i, r)| if r.contains(pos) { Some(i) } else { None })
                    });
                    match hovered_tab {
                        Some(idx) if idx != self.active_tab => {
                            let now = std::time::Instant::now();
                            let reset = self.dnd_tab_hover.map(|(prev, _)| prev != idx).unwrap_or(true);
                            if reset {
                                self.dnd_tab_hover = Some((idx, now));
                            }
                            if let Some((_, started)) = self.dnd_tab_hover {
                                if started.elapsed().as_millis() >= 500 {
                                    self.switch_to_tab(idx);
                                    self.tab_scroll_to_active = true;
                                    self.dnd_tab_hover = Some((idx, std::time::Instant::now()));
                                }
                            }
                            ctx.request_repaint();
                        }
                        _ => { self.dnd_tab_hover = None; }
                    }
                }

                // Cursor left the window while dragging → OLE drag-and-drop to Explorer / other apps
                #[cfg(windows)]
                {
                    let screen_rect = ctx.input(|i| i.screen_rect());
                    let cursor_outside = match hover_pos {
                        Some(pos) => !screen_rect.contains(pos),
                        None => true,
                    };
                    let btn_held = if self.dnd_is_right_click { right_down } else { left_down };
                    if btn_held && cursor_outside && !self.dnd_sources.is_empty() {
                        let sources = self.dnd_sources.clone();
                        let is_right = self.dnd_is_right_click;
                        // Reset internal DnD state first
                        self.dnd_active = false;
                        self.dnd_is_right_click = false;
                        self.dnd_sources.clear();
                        self.dnd_label.clear();
                        self.dnd_start_pos = None;
                        self.dnd_drag_entry = None;
                        self.dnd_drop_target = None;
                        self.dnd_drop_target_prev = None;
                        self.dnd_suppress = 2;
                        // Blocking OLE drag — pumps Windows messages until drop/cancel
                        let was_move = ole_drag_files_out(&sources, is_right);
                        // DoDragDrop is blocking; the user released the mouse button inside the
                        // drop-target window (e.g. Chrome), so WM_LBUTTONUP was sent there and
                        // winit/egui never received it.  egui therefore believes the button is
                        // still held, which would trigger a phantom drag once dnd_suppress
                        // expires.  Fix: post a synthetic button-up to our own message queue
                        // so winit processes it and clears primary_down before the next
                        // update() frame runs the drag-detection code.
                        // the next update() frame runs the drag-detection code.
                        if let Some(hwnd) = crate::ole::find_own_hwnd() {
                            use winapi::um::winuser::{PostMessageW, WM_LBUTTONUP, WM_RBUTTONUP};
                            let msg = if is_right { WM_RBUTTONUP } else { WM_LBUTTONUP };
                            unsafe { PostMessageW(hwnd, msg, 0, 0); }
                        }
                        // Belt-and-suspenders: re-clear drag tracking fields that may have been
                        // populated by the Windows message-pump inside DoDragDrop.
                        self.dnd_start_pos = None;
                        self.dnd_drag_entry = None;
                        if was_move {
                            self.selected_entries.clear();
                        }
                        self.refresh_contents();
                    }
                }

                if !pointer_down && self.dnd_active {
                    // If the cursor is outside our window, the user intended a cross-window
                    // drop.  OLE should have handled it; if the button-release and cursor-exit
                    // raced on the same frame, just cancel — don't act in the source window.
                    let cursor_inside = ctx.input(|i| {
                        i.pointer.hover_pos()
                            .or(i.pointer.latest_pos())
                            .map_or(false, |pos| i.screen_rect().contains(pos))
                    });

                    // If releasing over a tab, use that tab's folder as the drop destination
                    let drop_tab_idx = ctx.input(|i| i.pointer.latest_pos().or(i.pointer.hover_pos()))
                        .and_then(|pos| {
                            self.dnd_tab_rects.iter().enumerate()
                                .find_map(|(i, r)| if r.contains(pos) { Some(i) } else { None })
                        });
                    if let Some(tab_idx) = drop_tab_idx {
                        self.dnd_drop_target = Some(self.tabs[tab_idx].path.clone());
                    }

                    // Fallback: if no specific folder target, use current directory
                    let dest = self.dnd_drop_target.take()
                        .filter(|d| d.is_dir())
                        .unwrap_or_else(|| self.current_path.clone());

                    let sources: Vec<PathBuf> = self.dnd_sources
                        .iter()
                        .filter(|s| **s != dest)
                        .cloned()
                        .collect();

                    if !sources.is_empty() && cursor_inside {
                        if self.dnd_is_right_click {
                            // Right-click drop: open the move/copy/shortcut menu
                            // Use latest pointer position (may be over the tree panel)
                            let drop_pos = ctx.input(|i|
                                i.pointer.latest_pos().or(i.pointer.hover_pos()).unwrap_or_default()
                            );
                            self.dnd_right_drop_menu = Some((sources, dest, drop_pos));
                        } else {
                            // Left-click drop: always move
                            self.start_copy_job(sources, dest.clone(), true, false);
                            self.selected_entries.clear();
                            // Switch to destination tab so user sees where the files landed
                            if let Some(tab_idx) = drop_tab_idx {
                                self.switch_to_tab(tab_idx);
                                self.tab_scroll_to_active = true;
                            }
                        }
                    }

                    self.dnd_active = false;
                    self.dnd_is_right_click = false;
                    self.dnd_tab_hover = None;
                    self.dnd_sources.clear();
                    self.dnd_label.clear();
                    self.dnd_start_pos = None;
                    self.dnd_drag_entry = None;
                    self.dnd_drop_target = None;
                    self.dnd_suppress = 2;
                }

                // Draw ghost label near cursor
                if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                    let painter = ctx.layer_painter(egui::LayerId::new(
                        egui::Order::Tooltip,
                        egui::Id::new("dnd_ghost"),
                    ));
                    let galley = painter.layout_no_wrap(
                        self.dnd_label.clone(),
                        egui::FontId::proportional(12.0),
                        egui::Color32::WHITE,
                    );
                    let text_rect = egui::Rect::from_min_size(
                        pos + egui::vec2(12.0, 12.0),
                        galley.size() + egui::vec2(12.0, 6.0),
                    );
                    painter.rect_filled(
                        text_rect,
                        4.0,
                        egui::Color32::from_rgba_unmultiplied(40, 40, 40, 220),
                    );
                    painter.rect_stroke(
                        text_rect,
                        4.0,
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(100, 150, 255)),
                    );
                    painter.galley(
                        text_rect.min + egui::vec2(6.0, 3.0),
                        galley,
                        egui::Color32::WHITE,
                    );
                    ctx.request_repaint();
                }
            }
        });

        // Drop menu context window
        if self.show_drop_menu && !self.dragged_files.is_empty() {
            let old_resp = egui::Window::new("drop_copy_move_menu")
                .collapsible(false)
                .resizable(false)
                .title_bar(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .frame(egui::Frame {
                    fill: egui::Color32::from_rgb(230, 240, 255),
                    stroke: egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                    inner_margin: egui::Margin::same(4.0),
                    ..Default::default()
                })
                .show(ctx, |ui| {
                    ui.style_mut().spacing.button_padding = egui::vec2(6.0, 3.0);
                    if ui.button("Move here").clicked() {
                        let files = self.dragged_files.clone();
                        let dest = self.current_path.clone();
                        self.start_copy_job(files, dest, true, false);
                        self.show_drop_menu = false;
                        self.dragged_files.clear();
                    }
                    if ui.button("Copy here").clicked() {
                        let files = self.dragged_files.clone();
                        let dest = self.current_path.clone();
                        self.start_copy_job(files, dest, false, false);
                        self.show_drop_menu = false;
                        self.dragged_files.clear();
                    }
                });
            let old_clicked_outside = ctx.input(|i| {
                i.pointer.any_click()
                    && old_resp.as_ref().map_or(true, |r| {
                        i.pointer.interact_pos()
                            .map_or(false, |p| !r.response.rect.contains(p))
                    })
            });
            if old_clicked_outside {
                self.show_drop_menu = false;
                self.dragged_files.clear();
            }
        }

        // Refresh contents periodically to catch updates from background threads
        if self.dragged_files.is_empty() && !self.show_drop_menu {
            // Let the file watcher pick up changes
        }

        // ── Right-click drop menu (Move / Copy / Create Shortcut) ──────────
        if let Some((ref sources, ref dest, menu_pos)) = self.dnd_right_drop_menu.clone() {
            let sources = sources.clone();
            let dest = dest.clone();
            let mut action: Option<&str> = None;
            let is_same_drive = sources.first()
                .and_then(|s| s.components().next())
                .zip(dest.components().next())
                .map(|(a, b)| a == b)
                .unwrap_or(false);

            let win_resp = egui::Window::new("drop_action_menu")
                .collapsible(false)
                .resizable(false)
                .title_bar(false)
                .fixed_pos(menu_pos)
                .default_width(160.0)
                .frame(egui::Frame {
                    fill: egui::Color32::from_rgb(230, 240, 255),
                    stroke: egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                    inner_margin: egui::Margin::same(4.0),
                    ..Default::default()
                })
                .show(ctx, |ui| {
                    ui.style_mut().spacing.button_padding = egui::vec2(6.0, 3.0);
                    if ui.button(if is_same_drive { "Move here" } else { "Move here  (copy+delete)" }).clicked() {
                        action = Some("move");
                    }
                    if ui.button("Copy here").clicked() {
                        action = Some("copy");
                    }
                    #[cfg(windows)]
                    if ui.button("Create shortcut here").clicked() {
                        action = Some("shortcut");
                    }
                });
            // Dismiss on click outside the menu window
            let clicked_outside = ctx.input(|i| {
                i.pointer.any_click()
                    && win_resp.as_ref().map_or(true, |r| {
                        i.pointer.interact_pos()
                            .map_or(false, |p| !r.response.rect.contains(p))
                    })
            });
            match action {
                Some("move") => {
                    self.start_copy_job(sources, dest, true, false);
                    self.selected_entries.clear();
                    self.dnd_right_drop_menu = None;
                }
                Some("copy") => {
                    self.start_copy_job(sources, dest, false, false);
                    self.dnd_right_drop_menu = None;
                }
                #[cfg(windows)]
                Some("shortcut") => {
                    for s in &sources {
                        let _ = create_lnk_shortcut(s, &dest);
                    }
                    self.refresh_contents();
                    self.dnd_right_drop_menu = None;
                }
                Some(_) | None if clicked_outside => {
                    self.dnd_right_drop_menu = None;
                }
                _ => {}
            }
        }

        // Context menu
        if self.show_context_menu {
            if let Some(ref entry) = self.context_menu_entry {
                // When opened from the tree, use the full path directly;
                // when opened from the file list, join current_path + name.
                let full_path = self.context_menu_tree_path
                    .clone()
                    .unwrap_or_else(|| self.current_path.join(&entry.name));

                // Pre-compute required width from all possible button labels
                let btn_padding = 8.0 + 8.0; // button padding (4+4) × 2 sides + frame inner margin
                let font_id = egui::TextStyle::Button.resolve(&ctx.style());
                let archive_stem = full_path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let extract_label = format!("Extract to ./{}", archive_stem);
                let mut labels: Vec<&str> = vec![
                    "Add to archive",
                    "📋 Copy full path",
                    "Rename",
                    "Properties",
                ];
                if entry.is_dir || Self::is_code_file(&full_path) {
                    labels.push("Open with VS Code");
                }
                let is_ps1 = full_path.extension()
                    .map(|e| e.to_ascii_lowercase() == "ps1")
                    .unwrap_or(false);
                if is_ps1 {
                    labels.push("▶ Run in PowerShell");
                }
                if Self::is_archive(&full_path) {
                    labels.push(extract_label.as_str());
                }
                let max_text_w = labels.iter()
                    .map(|l| ctx.fonts(|f| f.layout_no_wrap(l.to_string(), font_id.clone(), egui::Color32::WHITE).size().x))
                    .fold(0.0f32, f32::max);
                let menu_w = max_text_w + btn_padding;

                // Adjust position: clamp so the menu stays within the window.
                let screen = ctx.screen_rect();
                let ms = egui::vec2(menu_w + 10.0, self.context_menu_size.y);
                let raw = self.context_menu_position;
                let adj_x = raw.x.min(screen.max.x - ms.x).max(screen.min.x);
                let adj_y = raw.y.min(screen.max.y - ms.y).max(screen.min.y);

                let area_resp = egui::Area::new(egui::Id::new("ctx_menu"))
                    .fixed_pos(egui::pos2(adj_x, adj_y))
                    .order(egui::Order::Foreground)
                    .interactable(true)
                    .show(ctx, |ui| {
                        egui::Frame {
                            fill: egui::Color32::from_rgb(200, 220, 255),
                            stroke: egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                            inner_margin: egui::Margin::same(4.0),
                            rounding: egui::Rounding::same(4.0),
                            ..Default::default()
                        }
                        .show(ui, |ui| {
                            ui.set_max_width(menu_w);
                            ui.set_min_width(menu_w);
                            ui.style_mut().spacing.button_padding = egui::vec2(4.0, 2.0);

                            // Open with VS Code
                            if (entry.is_dir || Self::is_code_file(&full_path))
                                && ui.add_sized([menu_w, 0.0], egui::Button::new("Open with VS Code")).clicked()
                            {
                                #[cfg(windows)]
                                let _ = std::process::Command::new("cmd")
                                    .args(["/C", "code", full_path.to_string_lossy().as_ref()])
                                    .spawn();
                                #[cfg(not(windows))]
                                let _ = std::process::Command::new("code").arg(&full_path).spawn();
                                self.show_context_menu = false;
                                self.context_menu_tree_path = None;
                                self.context_menu_tree_highlight = None;
                            }

                            // Run in PowerShell (.ps1)
                            if is_ps1
                                && ui.add_sized([menu_w, 0.0], egui::Button::new("▶ Run in PowerShell")).clicked()
                            {
                                #[cfg(windows)]
                                let _ = std::process::Command::new("powershell")
                                    .args(["-ExecutionPolicy", "Bypass", "-File",
                                           full_path.to_string_lossy().as_ref()])
                                    .spawn();
                                self.show_context_menu = false;
                                self.context_menu_tree_highlight = None;
                            }

                            // Extract to ./<archive name>
                            if Self::is_archive(&full_path)
                                && ui.add_sized([menu_w, 0.0], egui::Button::new(extract_label.as_str())).clicked()
                            {
                                self.extract_archive_path = full_path.clone();
                                self.show_extract_dialog = true;
                                self.show_context_menu = false;
                                self.context_menu_tree_highlight = None;
                            }

                            // Add to archive
                            if ui.add_sized([menu_w, 0.0], egui::Button::new("Add to archive")).clicked() {
                                self.files_to_archive.clear();
                                // Use the snapshot taken at right-click time so that any
                                // click-through to the table can't clear our selection.
                                if !self.context_menu_selection.is_empty() {
                                    self.files_to_archive = self.context_menu_selection.clone();
                                } else {
                                    self.files_to_archive.push(full_path.clone());
                                }

                                // Default archive name based on first item
                                let stem = if let Some(first) = self.files_to_archive.first() {
                                    first
                                        .file_stem()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .to_string()
                                } else {
                                    "archive".to_string()
                                };
                                self.archive_name_buffer = stem;
                                self.show_archive_dialog = true;
                                self.show_context_menu = false;
                                self.context_menu_tree_highlight = None;
                            }

                            // Thin separator line (fixed width, no stretch)
                            let (line_rect, _) = ui.allocate_exact_size(egui::vec2(menu_w, 1.0), egui::Sense::hover());
                            ui.painter().rect_filled(line_rect, 0.0, egui::Color32::from_gray(160));
                            ui.add_space(2.0);

                            // Copy full path
                            if ui.add_sized([menu_w, 0.0], egui::Button::new("📋 Copy full path")).clicked() {
                                if let Ok(mut clipboard) = Clipboard::new() {
                                    let _ = clipboard.set_text(full_path.to_string_lossy().replace("\\", "/"));
                                }
                                self.show_context_menu = false;
                                self.context_menu_tree_highlight = None;
                            }

                            // Rename
                            if !entry.name.starts_with("[..]")
                                && ui.add_sized([menu_w, 0.0], egui::Button::new("Rename")).clicked()
                            {
                                self.rename_ext = std::path::Path::new(&entry.name)
                                    .extension()
                                    .map(|e| format!(".{}", e.to_string_lossy()))
                                    .unwrap_or_default();
                                self.rename_buffer = std::path::Path::new(&entry.name)
                                    .file_stem()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_else(|| entry.name.clone());
                                self.rename_show_ext = false;
                                self.show_rename_dialog = true;
                                self.show_context_menu = false;
                                self.context_menu_tree_highlight = None;
                            }

                            // Properties
                            if ui.add_sized([menu_w, 0.0], egui::Button::new("Properties")).clicked() {
                                #[cfg(windows)]
                                {
                                    use std::ffi::OsStr;
                                    use std::os::windows::ffi::OsStrExt;
                                    use winapi::um::shellapi::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_INVOKEIDLIST};
                                    use winapi::um::winuser::SW_SHOW;
                                    let verb: Vec<u16> = OsStr::new("properties")
                                        .encode_wide().chain(std::iter::once(0)).collect();
                                    let file: Vec<u16> = OsStr::new(full_path.to_str().unwrap_or(""))
                                        .encode_wide().chain(std::iter::once(0)).collect();
                                    unsafe {
                                        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
                                        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
                                        info.fMask = SEE_MASK_INVOKEIDLIST;
                                        info.lpVerb = verb.as_ptr();
                                        info.lpFile = file.as_ptr();
                                        info.nShow = SW_SHOW as i32;
                                        ShellExecuteExW(&mut info);
                                    }
                                }
                                self.show_context_menu = false;
                                self.context_menu_tree_highlight = None;
                            }

                        });
                    });

                // Store actual rendered size for next-frame clamping
                self.context_menu_size = area_resp.response.rect.size();
            }

            // Close context menu if clicked elsewhere
            if ctx.input(|i| i.pointer.primary_clicked() || i.key_pressed(egui::Key::Escape)) {
                self.show_context_menu = false;
                self.context_menu_tree_path = None;
                self.context_menu_tree_highlight = None;
                self.context_menu_selection.clear();
            }
        }

        // ── Background context menu (right-click on empty space) ─────────────
        if self.show_bg_context_menu {
            let can_undo = !self.last_deleted_paths.is_empty();
            // Pre-compute required width from button labels
            let btn_padding = 8.0 + 8.0;
            let font_id = egui::TextStyle::Button.resolve(&ctx.style());
            let mut bg_labels = vec!["📁  New folder", "📄  New text file", "🔄  Refresh"];
            if can_undo {
                bg_labels.push("↩  Undo delete");
            }
            let max_text_w = bg_labels.iter()
                .map(|l| ctx.fonts(|f| f.layout_no_wrap(l.to_string(), font_id.clone(), egui::Color32::WHITE).size().x))
                .fold(0.0f32, f32::max);
            let menu_w = max_text_w + btn_padding;

            let screen = ctx.screen_rect();
            let ms = egui::vec2(menu_w + 10.0, self.bg_context_menu_size.y);
            let raw = self.bg_context_position;
            let adj_x = raw.x.min(screen.max.x - ms.x).max(screen.min.x);
            let adj_y = raw.y.min(screen.max.y - ms.y).max(screen.min.y);

            let bg_area_resp = egui::Area::new(egui::Id::new("bg_ctx_menu"))
                .fixed_pos(egui::pos2(adj_x, adj_y))
                .order(egui::Order::Foreground)
                .interactable(true)
                .show(ctx, |ui| {
                    egui::Frame {
                        fill: egui::Color32::from_rgb(200, 220, 255),
                        stroke: egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                        inner_margin: egui::Margin::same(4.0),
                        rounding: egui::Rounding::same(4.0),
                        ..Default::default()
                    }
                    .show(ui, |ui| {
                        ui.set_max_width(menu_w);
                        ui.set_min_width(menu_w);
                        ui.style_mut().spacing.button_padding = egui::vec2(4.0, 2.0);

                        if ui.add_sized([menu_w, 0.0], egui::Button::new("📁  New folder")).clicked() {
                            self.new_item_is_dir = true;
                            self.new_item_name_buffer = "New folder".to_string();
                            self.show_new_item_dialog = true;
                            self.show_bg_context_menu = false;
                        }
                        if ui.add_sized([menu_w, 0.0], egui::Button::new("📄  New text file")).clicked() {
                            self.new_item_is_dir = false;
                            self.new_item_name_buffer = "New file.txt".to_string();
                            self.show_new_item_dialog = true;
                            self.show_bg_context_menu = false;
                        }
                        // Thin separator line
                        let (line_rect, _) = ui.allocate_exact_size(egui::vec2(menu_w, 1.0), egui::Sense::hover());
                        ui.painter().rect_filled(line_rect, 0.0, egui::Color32::from_gray(160));
                        ui.add_space(2.0);
                        if ui.add_sized([menu_w, 0.0], egui::Button::new("🔄  Refresh")).clicked() {
                            self.refresh_contents();
                            self.show_bg_context_menu = false;
                        }
                        if can_undo {
                            // Separator
                            let (line_rect2, _) = ui.allocate_exact_size(egui::vec2(menu_w, 1.0), egui::Sense::hover());
                            ui.painter().rect_filled(line_rect2, 0.0, egui::Color32::from_gray(160));
                            ui.add_space(2.0);
                            if ui.add_sized([menu_w, 0.0], egui::Button::new("↩  Undo delete")).clicked() {
                                #[cfg(windows)]
                                {
                                    let paths = self.last_deleted_paths.clone();
                                    if Self::restore_from_recycle_bin(&paths) {
                                        self.last_deleted_paths.clear();
                                    }
                                    self.refresh_contents();
                                }
                                self.show_bg_context_menu = false;
                            }
                        }
                    });
                });

            // Store actual rendered size for next-frame clamping
            self.bg_context_menu_size = bg_area_resp.response.rect.size();

            if ctx.input(|i| i.pointer.primary_clicked() || i.key_pressed(egui::Key::Escape)) {
                self.show_bg_context_menu = false;
            }
        }

        // Archive dialog
        if self.show_archive_dialog {
            // Draw semi-transparent backdrop
            let screen_rect = ctx.screen_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::PanelResizeLine,
                egui::Id::new("archive_backdrop"),
            ));
            painter.rect_filled(screen_rect, 0.0, egui::Color32::from_black_alpha(128));

            egui::Window::new("Add to archive")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Archive name:");
                        ui.text_edit_singleline(&mut self.archive_name_buffer);
                    });

                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        ui.label("Format:");
                        ui.selectable_value(&mut self.archive_type, 0, "7z");
                        ui.selectable_value(&mut self.archive_type, 1, "zip");
                    });

                    ui.horizontal(|ui| {
                        ui.label("Compression:");
                        ui.selectable_value(&mut self.compression_level, 0, "Store");
                        ui.selectable_value(&mut self.compression_level, 1, "Medium");
                        ui.selectable_value(&mut self.compression_level, 2, "High");
                    });

                    ui.add_space(4.0);

                    ui.label(format!(
                        "{} {} to archive",
                        self.files_to_archive.len(),
                        if self.files_to_archive.len() == 1 { "item" } else { "items" }
                    ));

                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        if ui.button("Compress").clicked() {
                            let ext = match self.archive_type {
                                0 => "7z",
                                _ => "zip",
                            };
                            let format_flag = match self.archive_type {
                                0 => "-t7z",
                                _ => "-tzip",
                            };
                            let level_flag = match self.compression_level {
                                0 => "-mx0",
                                1 => "-mx5",
                                _ => "-mx9",
                            };

                            let archive_path = self
                                .current_path
                                .join(format!("{}.{}", self.archive_name_buffer, ext));
                            let archive_str = archive_path.to_string_lossy().to_string();

                            let archive_filename = format!("{}.{}", self.archive_name_buffer, ext);
                            let files_clone = self.files_to_archive.clone();
                            let (done_tx, done_rx) = channel();
                            let archive_str_clone = archive_str.clone();
                            let format_flag = format_flag.to_string();
                            let level_flag = level_flag.to_string();

                            std::thread::spawn(move || {
                                let mut cmd =
                                    std::process::Command::new("C:\\Program Files\\7-Zip\\7z.exe");
                                cmd.args(&["a", &format_flag, &level_flag, &archive_str_clone]);
                                for f in &files_clone {
                                    cmd.arg(f);
                                }
                                let result = cmd.spawn().or_else(|_| {
                                    let mut cmd2 = std::process::Command::new("7z.exe");
                                    cmd2.args(&[
                                        "a",
                                        &format_flag,
                                        &level_flag,
                                        &archive_str_clone,
                                    ]);
                                    for f in &files_clone {
                                        cmd2.arg(f);
                                    }
                                    cmd2.spawn()
                                });
                                if let Ok(mut child) = result {
                                    let _ = child.wait();
                                }
                                let _ = done_tx.send(archive_filename);
                            });

                            self.archive_done_receiver = Some(done_rx);
                            self.show_archive_dialog = false;
                            self.files_to_archive.clear();
                        }

                        if ui.button("Cancel").clicked() {
                            self.show_archive_dialog = false;
                            self.files_to_archive.clear();
                        }
                    });
                });
        }

        // Start extraction if dialog was shown (one-time trigger)
        if self.show_extract_dialog && self.extract_done_receiver.is_none() {
            let archive_path = self.extract_archive_path.clone();
            let stem = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let dest = self.current_path.join(&stem);
            let _ = std::fs::create_dir_all(&dest);
            let (done_tx, done_rx) = channel();

            std::thread::spawn(move || {
                let dest_str = dest.to_string_lossy().to_string();
                let archive_str = archive_path.to_string_lossy().to_string();

                let result = std::process::Command::new("C:\\Program Files\\7-Zip\\7z.exe")
                    .args(&["x", &archive_str, &format!("-o{}", dest_str)])
                    .spawn()
                    .or_else(|_| {
                        std::process::Command::new("7z.exe")
                            .args(&["x", &archive_str, &format!("-o{}", dest_str)])
                            .spawn()
                    });

                if let Ok(mut child) = result {
                    let _ = child.wait();
                }
                let _ = done_tx.send(());
            });

            self.extract_done_receiver = Some(done_rx);
        }

        // Rename dialog
        // ── New folder / New file dialog ──────────────────────────────────
        if self.show_new_item_dialog {
            let title = if self.new_item_is_dir { "New folder" } else { "New text file" };
            let mut close_dialog = false;
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label("Name:");
                    let resp = ui.text_edit_singleline(&mut self.new_item_name_buffer);

                    // Auto-focus on first frame
                    resp.request_focus();

                    let confirmed = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() || confirmed {
                            let name = self.new_item_name_buffer.trim().to_string();
                            if !name.is_empty() {
                                let target = self.current_path.join(&name);
                                if self.new_item_is_dir {
                                    let _ = std::fs::create_dir(&target);
                                } else {
                                    let _ = std::fs::File::create(&target);
                                }
                                self.refresh_contents();
                            }
                            close_dialog = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close_dialog = true;
                        }
                    });
                });
            if close_dialog {
                self.show_new_item_dialog = false;
            }
        }

        if self.show_rename_dialog {
            if let Some(entry) = self.context_menu_entry.clone() {
                let entry_name = entry.name.clone();
                let has_ext = !self.rename_ext.is_empty();
                let mut close_dialog = false;
                let mut do_rename = false;
                egui::Window::new("Rename")
                    .collapsible(false)
                    .resizable(false)
                    .min_width(280.0)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        // ESC closes without renaming
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            close_dialog = true;
                        }

                        // Label shows current full name as hint
                        ui.label(format!("Renaming: {}", &entry_name));
                        ui.add_space(4.0);

                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.rename_buffer)
                                .desired_width(f32::INFINITY)
                        );
                        response.request_focus();

                        // Extension toggle (only shown for files that have an extension)
                        if has_ext {
                            let prev = self.rename_show_ext;
                            ui.checkbox(&mut self.rename_show_ext,
                                format!("Show extension ({})", self.rename_ext));
                            if self.rename_show_ext != prev {
                                if self.rename_show_ext {
                                    // unchecked → checked: append stored extension
                                    let ext = self.rename_ext.clone();
                                    self.rename_buffer.push_str(&ext);
                                } else {
                                    // checked → unchecked: strip stored extension from end
                                    let ext = self.rename_ext.clone();
                                    if !ext.is_empty() && self.rename_buffer.ends_with(&ext) {
                                        let new_len = self.rename_buffer.len() - ext.len();
                                        self.rename_buffer.truncate(new_len);
                                    }
                                }
                            }
                        }

                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("OK").clicked()
                                || ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                do_rename = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close_dialog = true;
                            }
                        });
                    });

                if do_rename {
                    let stem = self.rename_buffer.trim().to_string();
                    if !stem.is_empty() {
                        let final_name = if self.rename_show_ext || !has_ext {
                            stem
                        } else {
                            format!("{}{}", stem, self.rename_ext)
                        };
                        let old_path = self.current_path.join(&entry_name);
                        let new_path = self.current_path.join(&final_name);
                        let _ = std::fs::rename(&old_path, &new_path);
                        self.refresh_contents();
                    }
                    self.show_rename_dialog = false;
                } else if close_dialog {
                    self.show_rename_dialog = false;
                }
            }
        }

        // Extraction status strip — shown at bottom while running, disappears when done
        if self.show_extract_dialog {
            let archive_name = self
                .extract_archive_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            egui::Window::new("##extract_strip")
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -8.0))
                .frame(egui::Frame::none()
                    .fill(egui::Color32::from_rgb(40, 80, 140))
                    .inner_margin(egui::Margin::symmetric(16.0, 6.0))
                    .rounding(egui::Rounding::same(6.0)))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new(
                            format!("Extracting {}…", archive_name))
                            .color(egui::Color32::WHITE).size(12.0));
                    });
                });
        }

        // Request repaints only while background work is in-flight.
        // Otherwise egui repaints on user input automatically — no need
        // to poll, so the app uses 0% CPU when idle.
        let has_bg_work = self.size_receiver.is_some()
            || self.archive_done_receiver.is_some()
            || self.extract_done_receiver.is_some()
            || self.dir_load_receiver.is_some()
            || !self.copy_jobs.is_empty();
        if has_bg_work {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        } else {
            // Wake up every 2 s to detect hotplugged / ejected drives.
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }

        // Hotplug detection: re-scan the drive list every 2 s.
        if self.last_drive_check.elapsed() >= std::time::Duration::from_secs(2) {
            self.last_drive_check = std::time::Instant::now();
            let current = Self::list_drives();
            if current != self.available_drives {
                // Classify any drives that weren't known before.
                for d in &current {
                    if !self.drive_types.contains_key(d) {
                        let letter = d.chars().next().unwrap_or('C');
                        let kind = Self::classify_drive(letter);
                        self.drive_types.insert(d.clone(), kind);
                    }
                }
                // Drop stale entries for removed drives.
                self.drive_types.retain(|k, _| current.contains(k));
                self.drives_info.retain(|info| current.contains(&info.drive));
                for d in &current {
                    if !self.drives_info.iter().any(|info| &info.drive == d) {
                        let letter = d.chars().next().unwrap_or('C');
                        let kind = Self::classify_drive(letter);
                        let (free_bytes, total_bytes) = Self::get_drive_space(d);
                        self.drives_info.push(DriveInfo { drive: d.clone(), kind, free_bytes, total_bytes });
                    }
                }
                self.available_drives = current;
            }
        }
    }
}

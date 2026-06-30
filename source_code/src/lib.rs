#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use arboard::Clipboard;
use eframe::egui;
use eframe::egui_wgpu;

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};

#[cfg(windows)]
use winapi::um::winuser::GetAsyncKeyState;

mod clipboard;
mod fs_ops;
mod ole;
mod shortcuts;
mod tree;
mod config;
mod types;
mod file_utils;
mod drives;
mod navigation;
mod jobs;
mod ui_dialogs;
mod ui_left_panel;
mod ui_file_list;
mod ui_thumbnails;
mod updater;

#[cfg(windows)]
use clipboard::{copy_files_to_clipboard, read_clipboard_drop_effect_is_cut, read_files_from_clipboard};
use fs_ops::{CopyJobState, ConflictChoice, ConflictInfo};
#[cfg(windows)]
use ole::{find_own_hwnd, ole_drag_files_out, register_ole_drop_target, try_move_to_rusplorer_desktop};
use config::*;
use types::*;
use updater::UpdateUiState;

pub fn run_app() -> Result<(), eframe::Error> {
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

    let window_title = if is_dev {
        concat!("Rusplorer (dev) (", env!("BUILD_TIMESTAMP"), ")")
    } else {
        concat!("Rusplorer (", env!("BUILD_TIMESTAMP"), ")")
    };

    // 1. Try wgpu with primary backends (DX12 / Vulkan).
    let result = launch(eframe::Renderer::Wgpu, None, session.clone(), &window_title);

    // 2. Last resort: use the GL (ANGLE) backend.
    //    On Windows, ANGLE implements OpenGL ES on top of D3D11's software path,
    //    which works on AWS WorkSpaces, Hyper-V guests, and any environment where
    //    there is no GPU and DX12/Vulkan are unavailable.
    //    Do NOT set WGPU_ADAPTER_NAME ï¿½ wgpu panics with unwrap() if that env var
    //    is set but no adapter name matches.
    match result {
        Err(ref e) if format!("{:?}", e).contains("NoSuitableAdapterFound") => {
            let mut wgpu_config = egui_wgpu::WgpuConfiguration::default();
            wgpu_config.supported_backends = eframe::wgpu::Backends::GL;
            wgpu_config.power_preference = eframe::wgpu::PowerPreference::None;
            launch(eframe::Renderer::Wgpu, Some(wgpu_config), session, &window_title)
        }
        other => other,
    }
}

fn launch(
    renderer: eframe::Renderer,
    wgpu_config_override: Option<egui_wgpu::WgpuConfiguration>,
    session: Option<SessionData>,
    window_title: &str,
) -> Result<(), eframe::Error> {
    let mut options = eframe::NativeOptions::default();
    options.renderer = renderer;
    if let Some(wgpu_config) = wgpu_config_override {
        options.wgpu_options = wgpu_config;
    }
    // Disable multisampling ï¿½ required on some corporate/VM environments
    // where the GPU driver does not expose MSAA sample counts.
    options.multisampling = 0;
    // Keep winit's drag_and_drop=true so winit calls OleInitialize and OLE's
    // internal HWND?drop-target routing is set up correctly.
    // We replace winit's own IDropTarget a few frames later via
    // RevokeDragDrop + RegisterDragDrop inside register_ole_drop_target.
    // Fall back to the position/size persisted in the config file when the
    // session file does not carry its own geometry (or when no session is open).
    let saved_config = Config::load();
    options.viewport.inner_size = session
        .as_ref()
        .and_then(|s| s.window_size)
        .or(saved_config.window_size)
        .map(|[w, h]| egui::vec2(w, h))
        .or(Some(egui::vec2(700.0, 600.0)));
    options.viewport.position = session
        .as_ref()
        .and_then(|s| s.window_pos)
        .or(saved_config.window_pos)
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
            fonts.font_data.insert(
                "NotoEmoji".to_owned(),
                egui::FontData::from_static(include_bytes!("fonts/NotoEmoji-Regular.ttf")),
            );
            // Replace the default proportional font with Iosevka Aile Regular,
            // with NotoEmoji as fallback for emoji glyphs not in Iosevka.
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "IosevkaAile-Regular".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("NotoEmoji".to_owned());
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

#[derive(Clone)]
struct DeepFilterItem {
    full_path: PathBuf,
    display_name: String,
    is_dir: bool,
}

enum DeepFilterEvent {
    Hit { section: String, item: DeepFilterItem },
    Done,
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
    config: Config,
    max_file_size: u64,
    is_focused: bool,
    filter: String,
    #[allow(dead_code)]
    file_watcher: Option<notify::RecommendedWatcher>,
    watch_receiver: Option<Receiver<jobs::WatchEvent>>,
    stop_watcher: Option<Sender<()>>,
    /// When Some, a file-system event arrived. We delay the actual refresh
    /// until this instant is >500 ms in the past (debounce).
    watch_pending_refresh: Option<std::time::Instant>,
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
    /// When true the rename dialog creates a new folder instead of renaming.
    new_folder_mode: bool,
    rename_buffer: String,
    /// The file extension (e.g. ".txt") stored separately while renaming.
    rename_ext: String,
    /// Whether the extension is shown in the rename text field.
    rename_show_ext: bool,
    /// One-shot autofocus when rename dialog is opened.
    rename_focus_pending: bool,
    // New folder / new file dialogs (triggered from background right-click menu)
    show_new_item_dialog: bool,
    new_item_is_dir: bool,
    new_item_name_buffer: String,
    /// One-shot autofocus when new-item dialog is opened.
    new_item_focus_pending: bool,
    show_bg_context_menu: bool,
    bg_context_position: egui::Pos2,
    bg_context_menu_size: egui::Vec2,
    selected_entries: HashSet<String>,
    /// Anchor entry name for shift-click range selection.
    last_clicked_entry: Option<String>,
    /// Undo stack: most-recent reversible operation is at the end.
    undo_stack: Vec<UndoAction>,
    /// Whether the undo history popup is open.
    show_undo_dialog: bool,
    /// Completion channel for async delete-to-recycle-bin operation.
    delete_done_receiver: Option<Receiver<(Vec<PathBuf>, bool)>>,
    /// Streaming status updates for delete workflow.
    delete_status_receiver: Option<Receiver<String>>,
    /// Transient feedback message shown after delete attempts.
    delete_feedback_msg: Option<String>,
    delete_feedback_until: Option<std::time::Instant>,
    delete_feedback_is_error: bool,
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
    filter_edit_rect: egui::Rect,    // rect of the filter TextEdit × excluded from rubber-band
    // Internal drag-and-drop
    dnd_active: bool,
    dnd_sources: Vec<PathBuf>,
    dnd_label: String,
    dnd_start_pos: Option<egui::Pos2>,
    dnd_drag_entry: Option<String>,  // entry name when pointer was pressed (raw tracking)
    /// Tree-panel DnD: where the pointer first pressed down on a tree node.
    tree_dnd_start_pos: Option<egui::Pos2>,
    /// Tree-panel DnD: the folder being tracked for a potential drag from the tree.
    tree_dnd_source: Option<PathBuf>,
    dnd_drop_target: Option<PathBuf>,
    dnd_drop_target_prev: Option<PathBuf>, // previous frame's value, used for color display
    /// True when the active internal DnD was initiated from the left tree panel.
    /// Used to require an explicit drop target (no fallback to current_path) so
    /// dragging a tree folder over itself doesn't accidentally copy into the
    /// currently-displayed folder.
    dnd_from_tree: bool,
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
    // OLE drop-in channel: Explorer ? Rusplorer
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
    /// Which drive letters each active job touches (parallel to copy_jobs).
    copy_job_drives: Vec<std::collections::HashSet<char>>,
    /// Jobs waiting for a drive to become free.
    copy_pending: std::collections::VecDeque<(Vec<PathBuf>, PathBuf, Arc<CopyJobState>)>,
    /// Window geometry persisted on exit (sampled every 5 s, not every frame).
    last_window_pos:  Option<[f32; 2]>,
    last_window_size: Option<[f32; 2]>,
    last_window_geo_check: std::time::Instant,
    /// Type-to-select: last pressed letter and which match index we are on.
    type_select_char: Option<char>,
    type_select_index: usize,
    /// Row to scroll to in the next render_file_list call (consumed after use).
    type_select_scroll: Option<usize>,
    /// Unlock-file dialog.
    show_unlock_dialog: bool,
    unlock_dialog_path: Option<PathBuf>,
    unlock_locking_processes: Vec<(u32, String)>,
    /// Last filter text that the deep recursive worker was started for.
    deep_filter_last_query: String,
    /// Last folder root that the deep recursive worker was started for.
    deep_filter_last_root: PathBuf,
    /// Incremental recursive results grouped by first-level subfolder name.
    deep_filter_sections: HashMap<String, Vec<DeepFilterItem>>,
    /// Used to dedupe incremental recursive hits.
    deep_filter_seen: HashSet<PathBuf>,
    /// Receiver for deep recursive search events.
    deep_filter_rx: Option<Receiver<DeepFilterEvent>>,
    /// Cancellation token for the currently running deep recursive search worker.
    deep_filter_cancel: Arc<AtomicBool>,
    /// True while a deep recursive search is actively running.
    deep_filter_running: bool,
    update_state: UpdateUiState,
    update_check_receiver: Option<std::sync::mpsc::Receiver<updater::UpdateCheckResult>>,
    update_apply_receiver: Option<std::sync::mpsc::Receiver<updater::UpdateApplyResult>>,
    update_last_check: std::time::Instant,
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
            config,
            max_file_size: 0,
            is_focused: true,
            filter: String::new(),
            file_watcher: None,
            watch_receiver: None,
            stop_watcher: None,
            watch_pending_refresh: None,
            show_context_menu: false,
            context_menu_entry: None,
            context_menu_tree_path: None,
            context_menu_tree_highlight: None,
            context_menu_position: egui::Pos2::ZERO,
            context_menu_size: egui::vec2(100.0, 100.0),
            context_menu_selection: Vec::new(),
            show_rename_dialog: false,
            new_folder_mode: false,
            rename_buffer: String::new(),
            rename_ext: String::new(),
            rename_show_ext: false,
            rename_focus_pending: false,
            show_new_item_dialog: false,
            new_item_is_dir: false,
            new_item_name_buffer: String::new(),
            new_item_focus_pending: false,
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
            tree_dnd_start_pos: None,
            tree_dnd_source: None,
            dnd_drop_target: None,
            dnd_drop_target_prev: None,
            dnd_from_tree: false,
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
            undo_stack: Vec::new(),
            show_undo_dialog: false,
            delete_done_receiver: None,
            delete_status_receiver: None,
            delete_feedback_msg: None,
            delete_feedback_until: None,
            delete_feedback_is_error: false,
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
            copy_job_drives: Vec::new(),
            copy_pending: std::collections::VecDeque::new(),
            last_window_pos:  None,
            last_window_size: None,
            last_window_geo_check: std::time::Instant::now(),
            type_select_char: None,
            type_select_index: 0,
            type_select_scroll: None,
            show_unlock_dialog: false,
            unlock_dialog_path: None,
            unlock_locking_processes: Vec::new(),
            deep_filter_last_query: String::new(),
            deep_filter_last_root: PathBuf::new(),
            deep_filter_sections: HashMap::new(),
            deep_filter_seen: HashSet::new(),
            deep_filter_rx: None,
            deep_filter_cancel: Arc::new(AtomicBool::new(false)),
            deep_filter_running: false,
            update_state: UpdateUiState::Idle,
            update_check_receiver: None,
            update_apply_receiver: None,
            update_last_check: std::time::Instant::now() - std::time::Duration::from_secs(3600),
        };

        // Initialise tabs ï¿½ from session if provided, then config, then single default
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
            app.drives_info.push(DriveInfo { drive: drive.clone(), kind, free_bytes, total_bytes, label: Self::get_volume_label(&drive) });
        }

        app.start_update_check();

        app
    }

    fn cancel_deep_filter_search(&mut self) {
        self.deep_filter_cancel.store(true, Ordering::SeqCst);
        self.deep_filter_rx = None;
        self.deep_filter_running = false;
    }

    fn start_deep_filter_search(&mut self, query: &str) {
        self.cancel_deep_filter_search();
        self.deep_filter_sections.clear();
        self.deep_filter_seen.clear();

        if query.is_empty() {
            self.deep_filter_last_query.clear();
            self.deep_filter_last_root = self.current_path.clone();
            return;
        }

        self.deep_filter_last_query = query.to_string();
        self.deep_filter_last_root = self.current_path.clone();

        let root = self.current_path.clone();
        let query_lower = query.to_lowercase();
        let cancel = Arc::new(AtomicBool::new(false));
        self.deep_filter_cancel = cancel.clone();

        let (tx, rx) = std::sync::mpsc::channel::<DeepFilterEvent>();
        self.deep_filter_rx = Some(rx);
        self.deep_filter_running = true;

        std::thread::spawn(move || {
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                if cancel.load(Ordering::SeqCst) {
                    return;
                }

                let read = match std::fs::read_dir(&dir) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                for entry in read {
                    if cancel.load(Ordering::SeqCst) {
                        return;
                    }

                    let entry = match entry {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    let path = entry.path();
                    let file_type = match entry.file_type() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    let name = entry.file_name().to_string_lossy().to_string();

                    if file_type.is_dir() && !file_type.is_symlink() {
                        stack.push(path.clone());
                    }

                    if !name.to_lowercase().contains(&query_lower) {
                        continue;
                    }

                    let rel = match path.strip_prefix(&root) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    let mut comps = rel.components();
                    let first = match comps.next() {
                        Some(c) => c.as_os_str().to_string_lossy().to_string(),
                        None => continue,
                    };
                    let remainder = comps.collect::<std::path::PathBuf>();
                    // Current-folder matches are shown in the instant section,
                    // so deep async results start from subfolders only.
                    if remainder.as_os_str().is_empty() {
                        continue;
                    }

                    let display_name = remainder.to_string_lossy().to_string();
                    let item = DeepFilterItem {
                        full_path: path,
                        display_name,
                        is_dir: file_type.is_dir(),
                    };
                    let _ = tx.send(DeepFilterEvent::Hit {
                        section: first,
                        item,
                    });
                }
            }

            let _ = tx.send(DeepFilterEvent::Done);
        });
    }

    fn update_deep_filter_state(&mut self, ctx: &egui::Context) {
        let query = self.filter.trim().to_string();

        if query.is_empty() {
            if self.deep_filter_running
                || !self.deep_filter_sections.is_empty()
                || self.deep_filter_last_query != query
                || self.deep_filter_last_root != self.current_path
            {
                self.cancel_deep_filter_search();
                self.deep_filter_sections.clear();
                self.deep_filter_seen.clear();
                self.deep_filter_last_query.clear();
                self.deep_filter_last_root = self.current_path.clone();
            }
            return;
        }

        if query != self.deep_filter_last_query || self.current_path != self.deep_filter_last_root {
            self.start_deep_filter_search(&query);
            ctx.request_repaint();
        }

        if let Some(ref rx) = self.deep_filter_rx {
            let mut changed = false;
            loop {
                match rx.try_recv() {
                    Ok(DeepFilterEvent::Hit { section, item }) => {
                        if self.deep_filter_seen.insert(item.full_path.clone()) {
                            self.deep_filter_sections
                                .entry(section)
                                .or_default()
                                .push(item);
                            changed = true;
                        }
                    }
                    Ok(DeepFilterEvent::Done) => {
                        self.deep_filter_running = false;
                        self.deep_filter_rx = None;
                        changed = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.deep_filter_running = false;
                        self.deep_filter_rx = None;
                        changed = true;
                        break;
                    }
                }
            }

            if changed {
                for items in self.deep_filter_sections.values_mut() {
                    items.sort_by(|a, b| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()));
                }
                ctx.request_repaint();
            }
        }
    }

    fn render_section_header(ui: &mut egui::Ui, title: &str) {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(title).strong());
    }

    fn render_filter_results(&mut self, ui: &mut egui::Ui, this_folder_entries: &[FileEntry]) {
        Self::render_section_header(ui, "This folder:");

        if this_folder_entries.is_empty() {
            ui.label(egui::RichText::new("(no match)").italics().color(egui::Color32::from_gray(150)));
        } else {
            for entry in this_folder_entries {
                let label = if entry.is_dir {
                    format!("📁 {}", entry.name)
                } else {
                    format!("📄 {}", entry.name)
                };

                let is_selected = self.selected_entries.contains(&entry.name);
                let resp = ui.selectable_label(is_selected, label);

                if resp.clicked() {
                    self.selected_entries.clear();
                    self.selected_entries.insert(entry.name.clone());
                    self.last_clicked_entry = Some(entry.name.clone());
                }

                if resp.double_clicked() {
                    if entry.is_dir {
                        self.selected_action = Some(FileAction::OpenDir(self.current_path.join(&entry.name)));
                    } else {
                        let full_path = self.current_path.join(&entry.name);
                        #[cfg(windows)]
                        let _ = std::process::Command::new("explorer").arg(&full_path).spawn();
                    }
                }
            }
        }

        let mut sections: Vec<String> = self.deep_filter_sections.keys().cloned().collect();
        sections.sort_by_key(|s| s.to_lowercase());

        for section in sections {
            Self::render_section_header(ui, &format!("{}:", section));
            if let Some(items) = self.deep_filter_sections.get(&section) {
                if items.is_empty() {
                    ui.label(egui::RichText::new("(no match)").italics().color(egui::Color32::from_gray(150)));
                    continue;
                }

                for item in items {
                    let label = if item.is_dir {
                        format!("📁 {}", item.display_name)
                    } else {
                        format!("📄 {}", item.display_name)
                    };

                    let resp = ui.selectable_label(false, label);
                    if resp.double_clicked() {
                        if item.is_dir {
                            self.selected_action = Some(FileAction::OpenDir(item.full_path.clone()));
                        } else {
                            #[cfg(windows)]
                            let _ = std::process::Command::new("explorer")
                                .arg(&item.full_path)
                                .spawn();
                        }
                    }
                }
            }
        }

        if self.deep_filter_running {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Searching subfolders…").italics().color(egui::Color32::from_gray(160)));
        }
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
}

impl eframe::App for RusplorerApp {
    fn on_exit(&mut self) {
        // Flush active tab state back into the tabs vec, then persist to config.
        self.save_active_tab();
        self.config.tabs = Some(self.tabs.clone());
        self.config.active_tab = Some(self.active_tab);
        // Persist window geometry tracked from the last frame.
        self.config.window_pos  = self.last_window_pos;
        self.config.window_size = self.last_window_size;
        self.config.save();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_update_tasks(ctx);
        if self.update_check_receiver.is_none()
            && self.update_apply_receiver.is_none()
            && self.update_last_check.elapsed() >= std::time::Duration::from_secs(30 * 60)
            && !matches!(
                self.update_state,
                UpdateUiState::Downloading { .. } | UpdateUiState::Applying { .. }
            )
        {
            self.start_update_check();
        }

        // Sample window geometry at most once every 5 seconds to avoid
        // doing it every frame; the value is only needed when the app exits.
        if self.last_window_geo_check.elapsed().as_secs() >= 5 {
            self.last_window_geo_check = std::time::Instant::now();
            ctx.input(|i| {
                let vp = i.viewport();
                if let Some(r) = vp.outer_rect {
                    self.last_window_pos = Some([r.min.x, r.min.y]);
                }
                if let Some(r) = vp.inner_rect {
                    self.last_window_size = Some([r.width(), r.height()]);
                }
            });
        }

        // Rotate drop target: prev holds last frame's value for color display;
        // current is reset to None so tree / breadcrumbs / table can detect fresh this frame.
        if self.dnd_active {
            self.dnd_drop_target_prev = self.dnd_drop_target.clone();
            self.dnd_drop_target = None;
        } else {
            self.dnd_drop_target_prev = None;
        }

        // Decrement the suppress frame-counter.  Suppress blocks new drag
        // detection and context-menu triggers for a short window after an OLE
        // drag-out completes or an in-window drop finishes.
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

        // Register OLE IDropTarget on our HWND so Explorer can drag files in.
        // We must replace winit's own IDropTarget with our custom one that
        // understands right-button drags.  Winit registers its target a few
        // frames after startup, so we keep retrying until RevokeDragDrop
        // succeeds (= winit's target was there and we replaced it).
        #[cfg(windows)]
        if !self.drop_target_registered {
            if let Some(hwnd_raw) = find_own_hwnd() {
                // Ensure senders are available for (re)registration attempts.
                // On the first attempt they are taken; on retries we use the
                // stored IDropTarget's channels, so we only need them once.
                if self._ole_drop_target.is_none() {
                    // First attempt: consume the one-shot senders.
                    if let (Some(tx), Some(rc_tx)) = (
                        self.ole_drop_sender.take(),
                        self.ole_rclick_drop_sender.take(),
                    ) {
                        let drag_in_flag = self.ole_drag_in_active.clone();
                        match register_ole_drop_target(
                            hwnd_raw as *mut _,
                            tx,
                            rc_tx,
                            drag_in_flag,
                        ) {
                            Some((target, revoked)) => {
                                self._ole_drop_target = Some(target);
                                if revoked {
                                    self.drop_target_registered = true;
                                }
                                // else: registered but winit not yet present ï¿½
                                // need to re-revoke next frame
                            }
                            None => {
                                self.drop_target_registered = true; // fatal, stop
                            }
                        }
                    }
                } else {
                    // Subsequent attempts: our target is already constructed,
                    // just try to revoke whatever's there and re-register ours.
                    if let Some(target) = self._ole_drop_target.as_ref() {
                        use windows::Win32::Foundation::HWND;
                        use windows::Win32::System::Ole::{
                            RegisterDragDrop, RevokeDragDrop,
                        };
                        let hwnd = HWND(hwnd_raw as *mut _);
                        unsafe {
                            let revoked = RevokeDragDrop(hwnd).is_ok();
                            crate::ole::log_dnd(&format!("Retry RevokeDragDrop ok={revoked}"));
                            if revoked {
                                let ok = RegisterDragDrop(hwnd, target).is_ok();
                                crate::ole::log_dnd(&format!("Retry RegisterDragDrop ok={ok}"));
                                if ok {
                                    self.drop_target_registered = true;
                                }
                            }
                        }
                    }
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

        // Consume in-flight delete status updates.
        if let Some(ref rx) = self.delete_status_receiver {
            while let Ok(msg) = rx.try_recv() {
                self.delete_feedback_msg = Some(msg);
                self.delete_feedback_until = None;
                self.delete_feedback_is_error = false;
            }
        }

        // Check if async delete-to-recycle-bin finished
        if let Some(ref rx) = self.delete_done_receiver {
            if let Ok((paths, ok)) = rx.try_recv() {
                if ok {
                    self.undo_stack.push(UndoAction::Delete { paths: paths.clone() });
                    self.selected_entries.clear();
                    self.delete_feedback_msg = Some("Deleted to Recycle Bin".to_string());
                    self.delete_feedback_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                    self.delete_feedback_is_error = false;
                } else {
                    let mut msg = "Delete failed".to_string();
                    if paths.len() == 1 {
                        let p = &paths[0];
                        let locked_by = crate::fs_ops::find_locking_processes(p);
                        if !locked_by.is_empty() {
                            let names: Vec<String> = locked_by.iter()
                                .take(3)
                                .map(|(_, n)| n.clone())
                                .collect();
                            msg = format!(
                                "Delete failed: item is in use by {}",
                                names.join(", ")
                            );
                        }
                    }
                    self.delete_feedback_msg = Some(msg);
                    self.delete_feedback_until = None;
                    self.delete_feedback_is_error = true;
                }

                if ok {
                    let current_was_deleted = paths.iter().any(|deleted| {
                        self.current_path == *deleted || self.current_path.starts_with(deleted)
                    });

                    self.invalidate_tree_after_delete(&paths);

                    if current_was_deleted {
                        let mut fallback = self.current_path.clone();
                        while let Some(parent) = fallback.parent().map(|p| p.to_path_buf()) {
                            let parent_is_deleted = paths.iter().any(|deleted| {
                                parent == *deleted || parent.starts_with(deleted)
                            });
                            if !parent_is_deleted {
                                self.commit_navigation(parent);
                                break;
                            }
                            if parent == fallback {
                                break;
                            }
                            fallback = parent;
                        }
                    }
                }

                // Always refresh after an attempted delete to avoid stale UI.
                if !paths.iter().any(|deleted| {
                    self.current_path == *deleted || self.current_path.starts_with(deleted)
                }) {
                    self.refresh_contents();
                }
                self.delete_done_receiver = None;
                self.delete_status_receiver = None;
            }
        }

        // Drain completed copy/move jobs
        {
            let mut need_refresh = false;
            let mut i = 0;
            while i < self.copy_jobs.len() {
                if self.copy_jobs[i].done.load(Ordering::SeqCst) {
                    let job = self.copy_jobs.remove(i);
                    self.copy_job_drives.remove(i);
                    if let Some(err) = job.error.lock().unwrap().clone() {
                        let op = if job.is_move { "Move" } else { "Copy" };
                        if err.starts_with("Completed with errors") {
                            self.delete_feedback_msg = Some(format!("{} {}", op, err.to_lowercase()));
                        } else {
                            self.delete_feedback_msg = Some(format!("{} failed: {}", op, err));
                        }
                        // Keep copy/move failures visible until dismissed so users don't miss
                        // them after switching apps/desktops during long transfers.
                        self.delete_feedback_until = None;
                        self.delete_feedback_is_error = true;
                    }
                    // Harvest results
                    let names = job.pasted_names.lock().unwrap().clone();
                    if !names.is_empty() {
                        self.selected_entries.clear();
                        for name in names {
                            self.selected_entries.insert(name);
                        }
                    }
                    // Record undo action for moves so Ctrl+Z can reverse them.
                    // Skip if the job itself was started by the undo system (no_undo flag).
                    if job.is_move && !job.no_undo.load(Ordering::Relaxed) {
                        let original_sources = job.original_sources.lock().unwrap().clone();
                        if !original_sources.is_empty() {
                            self.undo_stack.push(UndoAction::Move {
                                sources: original_sources,
                                dest: PathBuf::from(&job.dest_display),
                            });
                        }
                    }
                    if job.clear_clipboard.load(Ordering::Relaxed) {
                        self.clipboard_files.clear();
                        self.clipboard_mode = None;
                    }
                    need_refresh = true;
                    // don't increment i ï¿½ the next job has shifted into slot i
                } else {
                    i += 1;
                }
            }
            // Advance the queue: launch any pending job whose drives are now free.
            // Also purge any queued jobs the user cancelled.
            self.copy_pending.retain(|(_, _, s)| !s.cancelled.load(Ordering::Relaxed));
            if need_refresh || !self.copy_pending.is_empty() {
                self.advance_copy_queue();
            }
            if need_refresh {
                self.refresh_contents();
            }
        }

        // Process any file system changes detected by watcher
        self.process_file_changes();
        self.flush_watch_debounce();
        if self.watch_pending_refresh.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Track window focus and pause/resume background work
        let is_focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if is_focused != self.is_focused {
            self.is_focused = is_focused;
            self.pause_token.store(!is_focused, Ordering::SeqCst);
        }

        // Receive OLE drops from Explorer (drag-in)  ï¿½ left-click = move
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
                    crate::ole::log_dnd(&format!("OLE recv: starting move of {} file(s) -> {}", files.len(), dest.display()));
                    // External left-drop follows Rusplorer semantics: move by default.
                    // (Right-click drop still asks via context menu.)
                    let is_move = true;
                    self.start_copy_job(files, dest, is_move, false, false);
                    ctx.request_repaint();
                }
            }
        }

        // Receive OLE right-click drops ï¿½ show menu
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

        // Handle drag and drop (from external apps via egui/winit ï¿½ legacy fallback)
        // NOTE: with drag_and_drop=false on Windows, this path is inactive.
        // Our custom OLE IDropTarget handles drops instead.
        ctx.input(|i| {
            let dropped_files = &i.raw.dropped_files;
            if !dropped_files.is_empty() {
                self.dragged_files = dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect();
                if !self.dragged_files.is_empty() {
                    // Winit's drop target has no right-click awareness,
                    // so always present the right-click menu for safety.
                    let dest = self.current_path.clone();
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
                                let ppp = i.pixels_per_point();
                                pos = egui::pos2(pt.x as f32 / ppp, pt.y as f32 / ppp);
                            }
                        }
                        #[cfg(not(windows))]
                        if let Some(p) = i.pointer.hover_pos() { pos = p; }
                        pos
                    };
                    let files = self.dragged_files.clone();
                    self.dragged_files.clear();
                    self.dnd_right_drop_menu = Some((files, dest, drop_pos));
                }
            }
        });

        // Receive file sizes from background thread
        let mut sizes_updated = false;
        let size_scan_done = if let Some(ref rx) = self.size_receiver {
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok((path, size)) => {
                        self.file_sizes.insert(path, size);
                        if size > self.max_file_size {
                            self.max_file_size = size;
                        }
                        sizes_updated = true;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            disconnected
        } else {
            false
        };
        if size_scan_done {
            self.size_receiver = None;
        }

        // Receive directory completion signals
        let dirs_done_scan_done = if let Some(ref rx) = self.dirs_done_receiver {
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(path) => { self.dirs_done.insert(path); }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            disconnected
        } else {
            false
        };
        if dirs_done_scan_done {
            self.dirs_done_receiver = None;
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
                // Drive is now spinning — expand the tree and list the directory.
                let path_snap = self.current_path.clone();
                self.expand_tree_to(&path_snap);
                self.refresh_contents();
                self.start_file_watcher();
            }
            // If not accessible (e.g. drive removed), leave the empty listing.
            ctx.request_repaint();
        }
        // Keep repainting and show wait cursor while we are waiting for spin-up.
        if self.loading_path.is_some() {
            ctx.set_cursor_icon(egui::CursorIcon::Progress);
            ctx.request_repaint();
        }

        // Start/refresh/cancel incremental deep filtering based on the current
        // filter text and path, and consume worker events each frame.
        self.update_deep_filter_state(ctx);

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

        // Block global shortcuts when any modal dialog is open
        let any_modal_open = self.show_rename_dialog
            || self.show_new_item_dialog
            || self.show_archive_dialog
            || self.show_save_session_dialog
            || self.show_unlock_dialog
            || self.show_undo_dialog;

        // Type-to-select: pressing a letter jumps to the next entry starting with that letter.
        // Ignored when a modal is open, when Ctrl/Alt is held, or when the filter box is focused.
        if !any_modal_open {
            let filter_focused = ctx.memory(|m| {
                m.focused().map_or(false, |id| {
                    // The filter TextEdit widget gets an id derived from its rect/position;
                    // we detect focus by checking if any text-editing widget is active.
                    ctx.memory(|m2| m2.has_focus(id))
                })
            });
            let typed_char: Option<char> = ctx.input(|i| {
                if i.modifiers.ctrl || i.modifiers.alt || i.modifiers.command {
                    return None;
                }
                i.events.iter().find_map(|e| {
                    if let egui::Event::Text(s) = e {
                        let c = s.chars().next()?;
                        if c.is_alphabetic() { Some(c.to_ascii_lowercase()) } else { None }
                    } else {
                        None
                    }
                })
            });
            if !filter_focused {
                if let Some(ch) = typed_char {
                    // Collect indices of matching entries (skip "[..]")
                    let matches: Vec<usize> = self.contents
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| {
                            !e.name.starts_with("[..]")
                                && e.name.to_ascii_lowercase().starts_with(ch)
                        })
                        .map(|(i, _)| i)
                        .collect();
                    if !matches.is_empty() {
                        // Advance to next match if same letter pressed again
                        let next_idx = if self.type_select_char == Some(ch) {
                            (self.type_select_index + 1) % matches.len()
                        } else {
                            0
                        };
                        let row = matches[next_idx];
                        self.type_select_char = Some(ch);
                        self.type_select_index = next_idx;
                        self.selected_entries.clear();
                        self.selected_entries.insert(self.contents[row].name.clone());
                        self.type_select_scroll = Some(row);
                    }
                }
            }
        }

        // Handle Ctrl+A to select all
        if !any_modal_open && ctx.input(|i| i.key_pressed(egui::Key::A) && i.modifiers.ctrl) {
            self.selected_entries.clear();
            for entry in &self.contents {
                if !entry.name.starts_with("[..]") {
                    self.selected_entries.insert(entry.name.clone());
                }
            }
        }

        // Handle F5 to refresh the file list and the tree for the current folder
        if !any_modal_open && ctx.input(|i| i.key_pressed(egui::Key::F5)) {
            self.refresh_contents();
            self.refresh_tree_children_for_dir(&self.current_path.clone());
        }

        // Handle Up/Down arrow keys to move selection through the file list
        if !any_modal_open && !ctx.input(|i| i.modifiers.alt) {
            let up   = ctx.input(|i| i.key_pressed(egui::Key::ArrowUp));
            let down = ctx.input(|i| i.key_pressed(egui::Key::ArrowDown));
            if up || down {
                // Build the list of navigable rows (skip [..] parent entry)
                let navigable: Vec<usize> = self.contents
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| !e.name.starts_with("[..]"))
                    .map(|(i, _)| i)
                    .collect();
                if !navigable.is_empty() {
                    let is_shift = ctx.input(|i| i.modifiers.shift);
                    // Find the index inside `navigable` of the current anchor (last_clicked_entry).
                    let current_nav_idx = self.last_clicked_entry.as_ref().and_then(|anchor| {
                        navigable.iter().position(|&ci| self.contents[ci].name == *anchor)
                    });
                    let next_nav_idx = match current_nav_idx {
                        None => 0,
                        Some(ni) if up   => ni.saturating_sub(1),
                        Some(ni) => (ni + 1).min(navigable.len() - 1),
                    };
                    let row = navigable[next_nav_idx];
                    let name = self.contents[row].name.clone();
                    if is_shift {
                        // Extend selection from current anchor to new position
                        if let Some(ref anchor) = self.last_clicked_entry.clone() {
                            let anchor_nav = navigable.iter().position(|&ci| self.contents[ci].name == *anchor).unwrap_or(next_nav_idx);
                            let lo = anchor_nav.min(next_nav_idx);
                            let hi = anchor_nav.max(next_nav_idx);
                            self.selected_entries.clear();
                            for ni in lo..=hi {
                                self.selected_entries.insert(self.contents[navigable[ni]].name.clone());
                            }
                        }
                    } else {
                        self.selected_entries.clear();
                        self.selected_entries.insert(name.clone());
                        self.last_clicked_entry = Some(name);
                    }
                    self.type_select_scroll = Some(row);
                }
            }
        }

        // Handle Ctrl+Z: open undo history popup (if there is anything to undo)
        if !any_modal_open && ctx.input(|i| i.key_pressed(egui::Key::Z) && i.modifiers.ctrl) {
            if !self.undo_stack.is_empty() {
                self.show_undo_dialog = true;
            }
        }

        // Handle Escape to deselect all and cancel any active DnD
        if !any_modal_open && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selected_entries.clear();
            // Cancel any active or pending internal DnD
            self.dnd_active = false;
            self.dnd_start_pos = None;
            self.dnd_drag_entry = None;
            self.dnd_is_right_click = false;
            self.dnd_from_tree = false;
            self.dnd_sources.clear();
            self.dnd_label.clear();
            self.dnd_drop_target = None;
            self.dnd_drop_target_prev = None;
            self.dnd_right_drop_menu = None;
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
                // Use GetForegroundWindow for reliable check ï¿½ egui's viewport().focused
                // can return None (defaulting to true), causing false positives when
                // the user presses shortcuts in another window while GetAsyncKeyState
                // reports global key state.
                let dialog_open = self.show_rename_dialog || self.show_new_item_dialog
                    || self.show_archive_dialog
                    || self.show_save_session_dialog || self.show_unlock_dialog;
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
                crate::ole::log_dnd(&format!("Ctrl+C: {} selected, {} files", self.selected_entries.len(), files.len()));
                if let Err(e) = copy_files_to_clipboard(&files, false) {
                    self.delete_feedback_msg = Some(format!(
                        "System clipboard copy failed (internal copy still available): {}",
                        e
                    ));
                    self.delete_feedback_until = None;
                    self.delete_feedback_is_error = true;
                }
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
                if let Err(e) = copy_files_to_clipboard(&files, true) {
                    self.delete_feedback_msg = Some(format!(
                        "System clipboard cut failed (internal cut still available): {}",
                        e
                    ));
                    self.delete_feedback_until = None;
                    self.delete_feedback_is_error = true;
                }
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

                // Always try the Windows clipboard first ï¿½ it may have
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
                //   internal clipboard ? prefer Windows (external copy).
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

                crate::ole::log_dnd(&format!(
                    "Ctrl+V: win_clipboard={} internal={} use_internal={} dest={}",
                    win_clipboard.len(), self.clipboard_files.len(), use_internal, dest.display()
                ));

                if use_internal {
                    // Use our own internal clipboard ï¿½ reliable cut/copy detection
                    let files = self.clipboard_files.clone();
                    let is_cut = self.clipboard_mode == Some(ClipboardMode::Cut);
                    self.start_copy_job(files, dest, is_cut, is_cut, false);
                    if is_cut {
                        self.clipboard_files.clear();
                        self.clipboard_mode = None;
                    }
                } else if !win_clipboard.is_empty() {
                    // Use Windows clipboard (files from another app)
                    self.start_copy_job(win_clipboard.clone(), dest, win_is_cut, win_is_cut, false);
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
                                self.start_copy_job(files, dest, false, false, false);
                                Vec::new()
                            }
                            ClipboardMode::Cut => {
                                self.start_copy_job(files, dest, true, true, false);
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
            self.start_delete_job(files_to_delete);
        }

        // F2 ? rename the single selected entry
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
                    self.rename_focus_pending = true;
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


        // Measure ideal panel width from visible content (for this frame, apply next frame)
        {
            let font_id = egui::FontId::new(11.0, egui::FontFamily::Proportional);
            let mut max_w: f32 = 80.0;
            // Measure favorites (8px indent + name + 16px for ï¿½ button)
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
        //   "Drives?" button + drive mini-buttons + "Filter:" label + 70px input + "?" button
        let right_panel_min: f32 = {
            let font11 = egui::FontId::proportional(11.0);
            let font14 = egui::FontId::proportional(14.0);
            let item_sp = 8.0_f32; // default egui item_spacing.x

            // "Drives ▲▼" button: text width + ~10px button padding each side
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

            // "?" selectable_label
            let thumb_w = ctx.fonts(|f| {
                f.layout_no_wrap("?".to_string(), font14.clone(), egui::Color32::WHITE).size().x
            }) + 10.0;

            // Updater status button/label width budget.
            let update_w = 100.0 + item_sp;

            // Add outer panel margins / frame padding
            let margin = 20.0;

            (drives_btn_w + drive_btns_w + filter_label_w + filter_edit_w + thumb_w + update_w + margin)
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
                // Left panel changed ï¿½ resize window to preserve right panel width
                let desired_w = self.left_panel_width + self.right_panel_width + 8.0;
                let h = ctx.input(|i| i.viewport().inner_rect.map(|r| r.height())).unwrap_or(600.0);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(desired_w, h)));
                self.prev_left_panel_width = self.left_panel_width;
            } else {
                // Left panel unchanged ï¿½ if window width changed, user resized: update right_panel_width.
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

        // -- Copy/Move progress panel ------------------------------------------
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
                        let file_bytes_done = job.current_file_bytes.load(Ordering::Relaxed);
                        let file_bytes_total = job.current_file_total_bytes.load(Ordering::Relaxed);
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
                                format!("{} ï¿½ scanning ï¿½", op)
                            };
                            ui.label(egui::RichText::new(title).small());

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                // Abort button
                                if ui.small_button("Abort").clicked() {
                                    job.cancelled.store(true, Ordering::SeqCst);
                                }
                                // Pause / Resume
                                let paused = job.paused.load(Ordering::Relaxed);
                                let pause_label = if paused { "Resume" } else { "Pause" };
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
                            if file_bytes_total > 0 {
                                let file_fraction = (file_bytes_done as f32 / file_bytes_total as f32).clamp(0.0, 1.0);
                                let file_text = format!(
                                    "Current file: {} / {}",
                                    Self::format_bytes(file_bytes_done),
                                    Self::format_bytes(file_bytes_total),
                                );
                                ui.add(
                                    egui::ProgressBar::new(file_fraction)
                                        .text(file_text)
                                        .desired_width(ui.available_width()),
                                );
                            }

                            let fraction = bytes_done as f32 / bytes_total as f32;
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            // Speed display: EMA sampled a few times per second.
                            //
                            // Windows write-back caching causes burst-stall cycles where
                            // the OS accepts data at RAM speed, then stalls the writer while
                            // it flushes to disk.  A short fixed window (e.g. 750 ms) lands
                            // at a different phase of the cycle every time, producing wildly
                            // erratic values.  An EMA with a ~2 s time constant smooths the
                            // bursts and stalls into the actual sustained throughput.
                            //
                            // alpha = dt / tau  -> consistent time constant regardless of
                            // the actual sample interval. dt is clamped to [50, 500] ms
                            // so an occasional slow frame does not over-weight one sample.
                            const SPEED_SAMPLE_MIN_MS: u64 = 250;
                            let paused_now = job.paused.load(Ordering::Relaxed);
                            let speed_mbps = if paused_now {
                                // Reset the sample so resuming doesn't show a spike.
                                job.speed_sample_ms.store(now_ms, Ordering::Relaxed);
                                job.speed_sample_bytes.store(bytes_done, Ordering::Relaxed);
                                job.current_speed_bps.store(0, Ordering::Relaxed);
                                0.0
                            } else {
                                let last_ms    = job.speed_sample_ms.load(Ordering::Relaxed);
                                let last_bytes = job.speed_sample_bytes.load(Ordering::Relaxed);
                                let old_bps = job.current_speed_bps.load(Ordering::Relaxed) as f64;
                                if last_ms == 0 {
                                    job.speed_sample_ms.store(now_ms, Ordering::Relaxed);
                                    job.speed_sample_bytes.store(bytes_done, Ordering::Relaxed);
                                    old_bps / (1024.0 * 1024.0)
                                } else if now_ms.saturating_sub(last_ms) >= SPEED_SAMPLE_MIN_MS
                                    && bytes_done >= last_bytes
                                {
                                    // Advance the snapshot only when taking a sample.
                                    job.speed_sample_ms.store(now_ms, Ordering::Relaxed);
                                    job.speed_sample_bytes.store(bytes_done, Ordering::Relaxed);
                                    let dt_s       = (now_ms - last_ms) as f64 / 1000.0;
                                    let dt_clamped = dt_s.clamp(0.05, 0.5);
                                    let db         = (bytes_done - last_bytes) as f64;
                                    let instant    = db / dt_s;
                                    // tau = 2 s; alpha scales with actual frame time.
                                    let alpha    = (dt_clamped / 2.0).min(1.0);
                                    let smoothed = if old_bps == 0.0 {
                                        instant // bootstrap: accept first reading as-is
                                    } else {
                                        alpha * instant + (1.0 - alpha) * old_bps
                                    };
                                    job.current_speed_bps.store(smoothed as u64, Ordering::Relaxed);
                                    smoothed / (1024.0 * 1024.0)
                                } else {
                                    old_bps / (1024.0 * 1024.0)
                                }
                            };
                            let speed_bps = job.current_speed_bps.load(Ordering::Relaxed);
                            let eta_text = if !paused_now && speed_bps > 0 && bytes_total > bytes_done {
                                let secs_left = ((bytes_total - bytes_done) as f64 / speed_bps as f64).ceil() as u64;
                                if secs_left >= 3600 {
                                    let h = secs_left / 3600;
                                    let m = (secs_left % 3600) / 60;
                                    Some(format!("{}h {:02}m left", h, m))
                                } else if secs_left >= 60 {
                                    let m = secs_left / 60;
                                    let s = secs_left % 60;
                                    Some(format!("{}m {:02}s left", m, s))
                                } else {
                                    Some(format!("{}s left", secs_left))
                                }
                            } else {
                                None
                            };
                            let bar_text = match eta_text {
                                Some(eta) => format!(
                                    "{} / {}  ({:.1} MB/s, {})",
                                    Self::format_bytes(bytes_done),
                                    Self::format_bytes(bytes_total),
                                    speed_mbps,
                                    eta,
                                ),
                                None => format!(
                                    "{} / {}  ({:.1} MB/s)",
                                    Self::format_bytes(bytes_done),
                                    Self::format_bytes(bytes_total),
                                    speed_mbps,
                                ),
                            };
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

                        if job_idx + 1 < self.copy_jobs.len() || !self.copy_pending.is_empty() {
                            ui.separator();
                        }
                    }
                    // Show queued (waiting) jobs
                    for (q_idx, (sources, dest, state)) in self.copy_pending.iter().enumerate() {
                        let op = if state.is_move { "Move" } else { "Copy" };
                        let n = sources.len();
                        let noun = if n == 1 { "file" } else { "files" };
                        let dest_str = dest.to_string_lossy();
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(
                                    format!("Queued: {op} {n} {noun} -> {dest_str}")
                                )
                                .small()
                                .color(egui::Color32::from_rgb(160, 160, 160))
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("Cancel").clicked() {
                                    state.cancelled.store(true, Ordering::SeqCst);
                                }
                            });
                        });
                        if q_idx + 1 < self.copy_pending.len() { ui.separator(); }
                    }
                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                });
        }

        // -- File-conflict dialog ---------------------------------------------
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
                let title = format!("{} \"{}\" - file already exists", op, ci.file_name);

                // Measure button widths
                let btn_labels = [
                    "Overwrite this file",
                    "Skip this file",
                    "Overwrite all if different",
                    "Skip all with same name",
                    "Abort",
                ];
                let font_id = egui::TextStyle::Button.resolve(&ctx.style());
                let btn_w = btn_labels.iter()
                    .map(|l| ctx.fonts(|f| f.layout_no_wrap(l.to_string(), font_id.clone(), egui::Color32::WHITE).size().x))
                    .fold(0.0f32, f32::max) + 16.0; // 8px padding each side

                let mut answer: Option<ConflictChoice> = None;
                let scrim_rect = ctx.screen_rect();

                // Darken the app behind the dialog so the modal stands out.
                egui::Area::new(egui::Id::new("conflict_dialog_scrim"))
                    .order(egui::Order::Middle)
                    .fixed_pos(scrim_rect.min)
                    .interactable(true)
                    .show(ctx, |ui| {
                        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, scrim_rect.size());
                        ui.painter().rect_filled(
                            rect,
                            0.0,
                            egui::Color32::from_black_alpha(150),
                        );
                        // Consume clicks so background widgets are not interacted with.
                        let _ = ui.interact(
                            rect,
                            egui::Id::new("conflict_dialog_scrim_blocker"),
                            egui::Sense::click(),
                        );
                    });

                egui::Area::new(egui::Id::new("conflict_dialog_area"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .interactable(true)
                    .show(ctx, |ui| {
                        egui::Frame {
                            fill: ctx.style().visuals.window_fill(),
                            stroke: egui::Stroke::new(1.5, egui::Color32::from_rgb(100, 140, 220)),
                            inner_margin: egui::Margin::same(12.0),
                            rounding: egui::Rounding::same(6.0),
                            ..Default::default()
                        }
                        .show(ui, |ui| {
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
                                egui::Button::new(egui::RichText::new("Abort").color(egui::Color32::from_rgb(210, 80, 80)))
                            ).clicked() {
                                answer = Some(ConflictChoice::Abort);
                            }
                        });
                    });

                if let Some(choice) = answer {
                    *job.conflict_answer.lock().unwrap() = Some(choice);
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        if let Some(path) = self.render_left_panel(ctx) {
            self.navigate_to(path);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // -- Tab bar --------------------------------------------------
            let mut switch_to: Option<usize> = None;
            let mut close_idx: Option<usize> = None;
            let mut open_new_tab = false;
            let mut open_save_session = false;
            let mut drag_swap: Option<(usize, usize)> = None;

            // -- Scrollable tab bar ---------------------------------------
            let tab_bar_id = egui::Id::new("tab_bar_scroll");

            let tab_bar_resp = ui.horizontal(|ui| {
                // Tabs in a scroll area (save button is outside the scroll area)
                let available_w = ui.available_width() - 28.0; // reserve space for save button
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
                        let display = if label_text.chars().count() > 20 {
                            let truncated: String = label_text.chars().take(19).collect();
                            format!("{}…", truncated)
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

                                // Close label (only when more than 1 tab) ï¿½ interaction
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

                        // Single interact over the whole tab rect ï¿½ no competing widgets inside.
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

                // Double-click on empty tab-bar space ? new tab
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
                        // center the tab in the viewport, clamped to = 0
                        let target = (content_x - viewport_w / 2.0).max(0.0);
                        self.tab_scroll_target = target;
                        self.tab_scroll_offset = target; // snap immediately on tab switch
                    }
                    self.tab_scroll_to_active = false;
                    ctx.request_repaint();
                }

                // Mouse wheel on tab bar ? horizontal scroll (smooth)
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

                // Save button OUTSIDE the scroll area (pinned at end)
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

            // -- Save-session dialog --------------------------------------
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
                            self.drives_info.push(DriveInfo { drive: drive.clone(), kind, free_bytes, total_bytes, label: Self::get_volume_label(&drive) });
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
                // Little × button to clear the filter
                if !self.filter.is_empty() {
                    let x_btn = ui.add(egui::Button::new(
                        egui::RichText::new("×").size(11.0).color(egui::Color32::from_rgb(255, 80, 80))
                    ).frame(false));
                    if x_btn.clicked() {
                        self.filter.clear();
                    }
                }
                // Thumbnail / list view toggle
                let is_thumb = self.thumb_view.get(&self.current_path).copied().unwrap_or(false);
                let thumb_btn = ui.add_sized(
                    ui.spacing().interact_size,
                    egui::Button::new(egui::RichText::new(if is_thumb { "📄" } else { "▦" }).size(16.0))
                        .selected(is_thumb)
                );
                if thumb_btn.on_hover_text(if is_thumb { "Switch to list view" } else { "Switch to thumbnail view" }).clicked()
                {
                    let new_val = !is_thumb;
                    self.thumb_view.insert(self.current_path.clone(), new_val);
                    self.config.thumb_view = self.thumb_view.iter()
                        .map(|(k, v)| (k.to_string_lossy().to_string(), *v))
                        .collect();
                    self.config.save();
                }

                self.render_update_button(ui);


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
                            let drive_letter = info.drive
                                .trim_end_matches(|c: char| c == '\\' || c == '/')
                                .to_string();
                            let drive_label = if info.label.is_empty() {
                                drive_letter.clone()
                            } else {
                                format!("{}  {}", drive_letter, info.label)
                            };

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

                                // Col 1 ï¿½ drive letter
                                p.text(egui::pos2(content_x, cy),
                                    egui::Align2::LEFT_CENTER, &drive_label,
                                    fid_big, ui.visuals().text_color());

                                // Col 2 ï¿½ type badge, immediately after drive letter
                                let type_x = content_x + letter_w + 8.0;
                                p.text(egui::pos2(type_x, cy),
                                    egui::Align2::LEFT_CENTER, info.kind.label(),
                                    fid_sm.clone(), type_color);

                                // Col 3 ï¿½ progress bar then size text right-aligned
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

                // Paste path from clipboard button
                let paste_btn = ui.button("📂").on_hover_text("Paste path from clipboard");
                if paste_btn.clicked() {
                    if let Ok(mut clipboard) = Clipboard::new() {
                        if let Ok(text) = clipboard.get_text() {
                            let candidate = std::path::PathBuf::from(text.trim());
                            if candidate.is_dir() {
                                navigate_to_path = Some(candidate);
                            }
                        }
                    }
                }
            });

            if let Some(path) = navigate_to_path {
                self.navigate_to(path);
            }

            ui.separator();

            // -- Spin-up indicator ------------------------------------------------
            // When we navigated to a slow (HDD / USB / Network) drive that was idle,
            // the content is loaded in a background thread.  While we wait, show a
            // friendly message instead of a frozen / blank window.
            if self.loading_path.is_some() {
                let t = ctx.input(|i| i.time);
                // Simple 8-frame braille spinner that cycles at ~4 fps
                let spinners = ["|", "/", "—", "\\", "|", "/", "—", "\\"];
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

            // -- Shared pre-computation (list view + thumbnail view) ---------------
            let is_thumb_view = self.thumb_view.get(&self.current_path).copied().unwrap_or(false);
            let filter_lower = self.filter.to_lowercase();
            let filtered_entries: Vec<FileEntry> = self
                .contents
                .iter()
                .filter(|entry| {
                    self.filter.is_empty() || entry.name.to_lowercase().contains(&filter_lower)
                })
                .cloned()
                .collect();
            let entry_right_clicked;
            let mut sort_changed = false;

            let this_folder_entries: Vec<FileEntry> = filtered_entries
                .iter()
                .filter(|entry| !entry.name.starts_with("[..]"))
                .cloned()
                .collect();

            if !self.filter.is_empty() {
                entry_right_clicked = false;
                self.render_filter_results(ui, &this_folder_entries);
            } else if is_thumb_view {
                entry_right_clicked = self.render_thumbnails(ui, &filtered_entries);

            } else {
                let (erc, sc) = self.render_file_list(ui, &filtered_entries);
                entry_right_clicked = erc;
                sort_changed = sc;
            }

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
            // and no modal dialog is open ï¿½ e.g. rename suppresses rubber-band)
            if !self.dnd_active && self.tab_drag_index.is_none()
                && !self.show_rename_dialog
                && !self.show_new_item_dialog
            {            ctx.input(|i| {
                if let Some(pointer_pos) = i.pointer.hover_pos() {
                    let in_tab_bar = self.tab_bar_rect.contains(pointer_pos);
                    if i.pointer.primary_pressed() && !self.any_button_hovered && !in_tab_bar
                        && !self.filter_edit_rect.contains(pointer_pos)
                        && pointer_pos.x > self.left_panel_width + 2.0  // don't start rubber-band in the tree panel
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

                // Hover-to-switch: auto-switch to a tab when dragging over it for =500 ms
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

                // Cursor left the window while dragging ? OLE drag-and-drop to Explorer / other apps
                #[cfg(windows)]
                {
                    let screen_rect = ctx.input(|i| i.screen_rect());
                    let cursor_outside = if let Some(hwnd) = crate::ole::find_own_hwnd() {
                        unsafe {
                            use winapi::shared::windef::{POINT, RECT};
                            use winapi::um::winuser::{GetCursorPos, GetWindowRect};
                            let mut pt = POINT { x: 0, y: 0 };
                            let mut wr: RECT = std::mem::zeroed();
                            if GetCursorPos(&mut pt) != 0 && GetWindowRect(hwnd, &mut wr) != 0 {
                                pt.x < wr.left || pt.x >= wr.right || pt.y < wr.top || pt.y >= wr.bottom
                            } else {
                                match hover_pos {
                                    Some(pos) => !screen_rect.contains(pos),
                                    None => true,
                                }
                            }
                        }
                    } else {
                        match hover_pos {
                            Some(pos) => !screen_rect.contains(pos),
                            None => true,
                        }
                    };
                    let hw_btn_held = unsafe {
                        let vk = if self.dnd_is_right_click { 0x02 } else { 0x01 };
                        windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk)
                    } & (0x8000u16 as i16) != 0;
                    let btn_held = if self.dnd_is_right_click { right_down } else { left_down } || hw_btn_held;
                    if btn_held && cursor_outside && !self.dnd_sources.is_empty() {
                        let sources = self.dnd_sources.clone();
                        let is_right = self.dnd_is_right_click;
                        // Reset internal DnD state before blocking OLE call
                        self.dnd_active = false;
                        self.dnd_is_right_click = false;
                        self.dnd_from_tree = false;
                        self.dnd_sources.clear();
                        self.dnd_label.clear();
                        self.dnd_start_pos = None;
                        self.dnd_drag_entry = None;
                        self.dnd_drop_target = None;
                        self.dnd_drop_target_prev = None;
                        // Blocking OLE drag ï¿½ DoDragDrop requires the UI thread
                        // for mouse capture (SetCapture). The UI won't repaint
                        // while this blocks, but Windows pumps messages so the
                        // window stays responsive to the OS.
                        crate::ole::log_dnd(&format!("DragOut: is_right={is_right} files={}", sources.len()));
                        let was_move = ole_drag_files_out(&sources, is_right);
                        // Post synthetic button-ups so egui clears stale
                        // held-state that accumulated during the blocking call.
                        if let Some(hwnd) = crate::ole::find_own_hwnd() {
                            use winapi::um::winuser::{PostMessageW, WM_LBUTTONUP, WM_RBUTTONUP, GetCursorPos, ScreenToClient};
                            use winapi::shared::windef::POINT;
                            unsafe {
                                let mut pt = POINT { x: 0, y: 0 };
                                GetCursorPos(&mut pt);
                                ScreenToClient(hwnd, &mut pt);
                                let lparam = ((pt.y as u32) << 16 | (pt.x as u32 & 0xFFFF)) as isize;
                                PostMessageW(hwnd, WM_LBUTTONUP, 0, lparam);
                                PostMessageW(hwnd, WM_RBUTTONUP, 0, lparam);
                            }
                        }
                        // Re-clear any DnD state that the Windows message pump
                        // inside DoDragDrop may have populated.
                        self.dnd_start_pos = None;
                        self.dnd_drag_entry = None;
                        self.dnd_is_right_click = false;
                        self.dnd_suppress = 3;
                        if was_move {
                            self.selected_entries.clear();
                            self.refresh_contents();
                        }
                    }
                }

                if !pointer_down && self.dnd_active {
                    // If the cursor is outside our window, the user intended a cross-window
                    // drop.  OLE should have handled it; if the button-release and cursor-exit
                    // raced on the same frame, just cancel ï¿½ don't act in the source window.
                    let cursor_inside = ctx.input(|i| {
                        i.pointer.hover_pos()
                            .or(i.pointer.latest_pos())
                            .map_or(false, |pos| i.screen_rect().contains(pos))
                    });

                    // If releasing over a tab, treat this as a tab-bar gesture,
                    // not as a filesystem move target.
                    let drop_tab_idx = ctx.input(|i| i.pointer.latest_pos().or(i.pointer.hover_pos()))
                        .and_then(|pos| {
                            self.dnd_tab_rects.iter().enumerate()
                                .find_map(|(i, r)| if r.contains(pos) { Some(i) } else { None })
                        });
                    let all_sources: Vec<PathBuf> = self.dnd_sources.iter().cloned().collect();

                    if cursor_inside {
                        if drop_tab_idx.is_some() {
                            let tab_idx = drop_tab_idx.unwrap();
                            let tab_dest = self.tabs.get(tab_idx).map(|t| t.path.clone());
                            if let Some(dest) = tab_dest {
                                let sources: Vec<PathBuf> = all_sources.iter()
                                    .filter(|s| **s != dest)
                                    .cloned()
                                    .collect();
                                if !sources.is_empty() {
                                    if self.dnd_is_right_click {
                                        let drop_pos = ctx.input(|i|
                                            i.pointer.latest_pos().or(i.pointer.hover_pos()).unwrap_or_default()
                                        );
                                        self.dnd_right_drop_menu = Some((sources, dest, drop_pos));
                                    } else {
                                        // Any drop over a tab should transfer to that tab's folder,
                                        // even if the tab has already been activated by hover.
                                        self.start_copy_job(sources, dest, true, false, false);
                                        self.selected_entries.clear();
                                    }
                                }
                            }
                        } else {
                            // Normal in-content drop: use detected folder target, or current directory.
                            // Tree-originated drags must NOT fall back to current_path: if the
                            // user drags a tree folder over itself (or any non-target area), the
                            // drop should be a no-op rather than silently copying into whatever
                            // folder is currently displayed.
                            let dest_opt = self.dnd_drop_target.take()
                                .filter(|d| d.is_dir());
                            let dest = if self.dnd_from_tree {
                                match dest_opt {
                                    Some(d) => d,
                                    None => {
                                        self.dnd_active = false;
                                        self.dnd_is_right_click = false;
                                        self.dnd_from_tree = false;
                                        self.dnd_tab_hover = None;
                                        self.dnd_sources.clear();
                                        self.dnd_label.clear();
                                        self.dnd_start_pos = None;
                                        self.dnd_drag_entry = None;
                                        self.dnd_drop_target = None;
                                        self.dnd_suppress = 2;
                                        return;
                                    }
                                }
                            } else {
                                dest_opt.unwrap_or_else(|| self.current_path.clone())
                            };

                            let sources: Vec<PathBuf> = self.dnd_sources
                                .iter()
                                .filter(|s| **s != dest)
                                .cloned()
                                .collect();

                            if !sources.is_empty() {
                                if self.dnd_is_right_click {
                                    // Right-click drop: open the move/copy/shortcut menu
                                    // Use latest pointer position (may be over the tree panel)
                                    let drop_pos = ctx.input(|i|
                                        i.pointer.latest_pos().or(i.pointer.hover_pos()).unwrap_or_default()
                                    );
                                    self.dnd_right_drop_menu = Some((sources, dest, drop_pos));
                                } else {
                                    // Left-click drop: always move
                                    self.start_copy_job(sources, dest.clone(), true, false, false);
                                    self.selected_entries.clear();
                                }
                            }
                        }
                    }

                    self.dnd_active = false;
                    self.dnd_is_right_click = false;
                    self.dnd_from_tree = false;
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
        self.render_dialogs(ctx);

        // Request repaints only while background work is in-flight.
        // Otherwise egui repaints on user input automatically ï¿½ no need
        // to poll, so the app uses 0% CPU when idle.
        let has_bg_work = self.size_receiver.is_some()
            || self.archive_done_receiver.is_some()
            || self.extract_done_receiver.is_some()
            || self.dir_load_receiver.is_some()
            || self.update_check_receiver.is_some()
            || self.update_apply_receiver.is_some()
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
                        self.drives_info.push(DriveInfo { drive: d.clone(), kind, free_bytes, total_bytes, label: Self::get_volume_label(d) });
                    }
                }
                self.available_drives = current;
            }
        }
    }
}





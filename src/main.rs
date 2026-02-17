#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use egui_extras::{TableBuilder, Column};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::collections::HashMap;
use arboard::Clipboard;
use serde::{Deserialize, Serialize};
use notify::{Watcher, RecursiveMode};
use notify::recommended_watcher;
use std::collections::HashSet;

#[cfg(windows)]
use winapi::um::winuser::{OpenClipboard, CloseClipboard, SetClipboardData, EmptyClipboard, GetAsyncKeyState, GetClipboardData, IsClipboardFormatAvailable};
#[cfg(windows)]
use winapi::um::shellapi::{SHFileOperationW, SHFILEOPSTRUCTW, FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, DragQueryFileW};
#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

/// Copy files to Windows clipboard in HDROP format so they can be pasted in Explorer
#[cfg(windows)]
fn copy_files_to_clipboard(files: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    use winapi::um::winuser::CF_HDROP;
    
    // DROPFILES structure: 20 bytes total
    // offset 0:  pFiles (DWORD) - offset to file list = 20
    // offset 4:  pt.x (LONG)
    // offset 8:  pt.y (LONG)
    // offset 12: fNC (BOOL)
    // offset 16: fWide (BOOL) - must be 1 for Unicode
    
    // Build the wide-char file list: each path null-terminated, double-null at end
    let mut wide_chars: Vec<u16> = Vec::new();
    for file in files {
        let path_str = file.to_string_lossy();
        let wide: Vec<u16> = OsStr::new(path_str.as_ref())
            .encode_wide()
            .chain(std::iter::once(0u16))
            .collect();
        wide_chars.extend_from_slice(&wide);
    }
    wide_chars.push(0u16); // Final double-null terminator
    
    let dropfiles_size: usize = 20; // sizeof(DROPFILES)
    let file_data_size = wide_chars.len() * 2; // bytes for wide chars
    let total_size = dropfiles_size + file_data_size;
    
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("Failed to open clipboard".into());
        }
        
        if EmptyClipboard() == 0 {
            CloseClipboard();
            return Err("Failed to empty clipboard".into());
        }
        
        let hglobal = winapi::um::winbase::GlobalAlloc(
            winapi::um::winbase::GMEM_MOVEABLE | winapi::um::winbase::GMEM_ZEROINIT,
            total_size,
        );
        if hglobal.is_null() {
            CloseClipboard();
            return Err("Failed to allocate global memory".into());
        }
        
        let ptr = winapi::um::winbase::GlobalLock(hglobal) as *mut u8;
        if ptr.is_null() {
            winapi::um::winbase::GlobalFree(hglobal);
            CloseClipboard();
            return Err("Failed to lock global memory".into());
        }
        
        // Write DROPFILES structure
        // pFiles = 20 (offset to file data)
        let pfiles: u32 = 20;
        std::ptr::copy_nonoverlapping(&pfiles as *const u32 as *const u8, ptr, 4);
        // pt.x = 0 (offset 4, already zeroed)
        // pt.y = 0 (offset 8, already zeroed)
        // fNC = 0  (offset 12, already zeroed)
        // fWide = 1 (offset 16)
        let fwide: u32 = 1;
        std::ptr::copy_nonoverlapping(&fwide as *const u32 as *const u8, ptr.add(16), 4);
        
        // Write file paths after DROPFILES structure
        std::ptr::copy_nonoverlapping(
            wide_chars.as_ptr() as *const u8,
            ptr.add(dropfiles_size),
            file_data_size,
        );
        
        winapi::um::winbase::GlobalUnlock(hglobal);
        
        if SetClipboardData(CF_HDROP, hglobal as *mut winapi::ctypes::c_void).is_null() {
            winapi::um::winbase::GlobalFree(hglobal);
            CloseClipboard();
            return Err("Failed to set clipboard data".into());
        }
        
        CloseClipboard();
    }
    
    Ok(())
}

/// Read files from Windows clipboard in HDROP format
#[cfg(windows)]
fn read_files_from_clipboard() -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    use winapi::um::winuser::CF_HDROP;
    
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("Failed to open clipboard".into());
        }
        
        // Check if clipboard has HDROP format
        if IsClipboardFormatAvailable(CF_HDROP) == 0 {
            CloseClipboard();
            return Ok(Vec::new()); // No files in clipboard
        }
        
        let hglobal = GetClipboardData(CF_HDROP);
        if hglobal.is_null() {
            CloseClipboard();
            return Err("Failed to get clipboard data".into());
        }
        
        // Query the number of files
        let file_count = DragQueryFileW(hglobal as *mut _, 0xFFFFFFFF, std::ptr::null_mut(), 0);
        
        let mut files = Vec::new();
        for i in 0..file_count {
            // Get the length of the file path
            let path_len = DragQueryFileW(hglobal as *mut _, i, std::ptr::null_mut(), 0);
            
            // Allocate buffer and get the file path
            let mut buffer: Vec<u16> = vec![0; (path_len + 1) as usize];
            DragQueryFileW(hglobal as *mut _, i, buffer.as_mut_ptr(), buffer.len() as u32);
            
            // Convert to PathBuf
            let path_str = String::from_utf16_lossy(&buffer[..path_len as usize]);
            files.push(PathBuf::from(path_str));
        }
        
        CloseClipboard();
        Ok(files)
    }
}

/// Recursively calculate directory size, sending updates progressively
fn calculate_dir_size_progressive(
    path: &Path,
    root_path: &Path,
    cancel_token: &Arc<AtomicBool>,
    pause_token: &Arc<AtomicBool>,
    tx: &std::sync::mpsc::Sender<(PathBuf, u64)>,
    accumulated: &mut u64,
) -> bool {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => {
            // Permission denied or other error - send accumulated size so far
            let _ = tx.send((root_path.to_path_buf(), *accumulated));
            return false;
        }
    };
    
    for entry in entries.filter_map(|e| e.ok()) {
        // Check cancellation every iteration
        if cancel_token.load(Ordering::Relaxed) {
            return false;
        }
        // Check pause and sleep if paused
        while pause_token.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if cancel_token.load(Ordering::Relaxed) {
                return false;
            }
        }
        
        let entry_path = entry.path();
        if entry_path.is_dir() {
            calculate_dir_size_progressive(&entry_path, root_path, cancel_token, pause_token, tx, accumulated);
        } else if let Ok(metadata) = entry.metadata() {
            *accumulated += metadata.len();
            // Send update every time we add file size
            let _ = tx.send((root_path.to_path_buf(), *accumulated));
        }
    }
    true
}

fn main() -> Result<(), eframe::Error> {
    let mut options = eframe::NativeOptions::default();
    options.viewport.inner_size = Some(egui::vec2(400.0, 600.0));
    eframe::run_native(
        "Rusplorer",
        options,
        Box::new(|cc| {
            let mut style = (*cc.egui_ctx.style()).clone();
            // Set 11pt font size for all text styles
            for (_, font_id) in &mut style.text_styles {
                font_id.size = 11.0;
            }
            style.spacing.button_padding = egui::vec2(2.0, 0.0);
            style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 10);
            style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
            style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
            cc.egui_ctx.set_style(style);
            Box::new(RusplorerApp::default())
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
    #[serde(default = "default_sort_column")]
    sort_column: SortColumn,
    #[serde(default = "default_sort_ascending")]
    sort_ascending: bool,
}

fn default_sort_column() -> SortColumn { SortColumn::Name }
fn default_sort_ascending() -> bool { true }

impl Config {
    fn path() -> PathBuf {
        let exe_path = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("rusplorer.exe"));
        let mut config_path = exe_path.parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf();
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
            sort_column: SortColumn::Name,
            sort_ascending: true,
        }
    }

    fn save(&self) {
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), content);
        }
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
    context_menu_position: egui::Pos2,
    show_rename_dialog: bool,
    rename_buffer: String,
    selected_entries: HashSet<String>,
    show_archive_dialog: bool,
    archive_type: usize,       // 0 = 7z, 1 = zip
    compression_level: usize,  // 0 = store, 1 = medium, 2 = high
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
    any_button_hovered: bool,
    dirs_done: HashSet<PathBuf>,
    dirs_done_receiver: Option<Receiver<PathBuf>>,
    show_date_columns: HashMap<PathBuf, bool>,
    sort_column: SortColumn,
    sort_ascending: bool,
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

#[derive(Clone)]
struct FileEntry {
    name: String,
    is_dir: bool,
    #[allow(dead_code)]
    size: u64,
    modified: Option<SystemTime>,
}

impl Default for RusplorerApp {
    fn default() -> Self {
        let available_drives = Self::list_drives();
        let config = Config::load();
        let start_path = PathBuf::from(&config.last_path);
        let current_path = if start_path.exists() {
            start_path
        } else {
            PathBuf::from("C:\\")
        };
        let show_date_columns: HashMap<PathBuf, bool> = config.show_date_columns.iter()
            .map(|(k, v)| (PathBuf::from(k), *v))
            .collect();
        let sort_column = config.sort_column.clone();
        let sort_ascending = config.sort_ascending;

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
            context_menu_position: egui::Pos2::ZERO,
            show_rename_dialog: false,
            rename_buffer: String::new(),
            selected_entries: HashSet::new(),
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
            any_button_hovered: false,
            dirs_done: HashSet::new(),
            dirs_done_receiver: None,
            show_date_columns,
            sort_column,
            sort_ascending,
        };
        app.refresh_contents();
        app.start_file_watcher();
        app
    }
}

impl RusplorerApp {
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
                    FileEntry { name, is_dir, size: 0, modified }
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
                        if self.sort_ascending { ord } else { ord.reverse() }
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
                            calculate_dir_size_progressive(&dir_path, &dir_path, &cancel, &pause, &tx, &mut accumulated);
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
            if a.name.starts_with("[..]") { return std::cmp::Ordering::Less; }
            if b.name.starts_with("[..]") { return std::cmp::Ordering::Greater; }

            // Dirs always before files
            match (a.is_dir, b.is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => {
                    let ord = match sort_column {
                        SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                        SortColumn::Size => {
                            let sa = file_sizes.get(&current_path.join(&a.name)).copied().unwrap_or(0);
                            let sb = file_sizes.get(&current_path.join(&b.name)).copied().unwrap_or(0);
                            sa.cmp(&sb)
                        }
                        SortColumn::Date => a.modified.cmp(&b.modified),
                    };
                    if sort_ascending { ord } else { ord.reverse() }
                }
            }
        });
    }

    fn navigate_to(&mut self, path: PathBuf) {
        if path.exists() && path.is_dir() {
            // Only add to history if it's different from current path
            if path != self.current_path {
                self.back_history.push(self.current_path.clone());
                self.forward_history.clear(); // Clear forward history on new navigation
            }
            self.current_path = path;
            
            // Save the current path to config
            self.config.last_path = self.current_path.to_string_lossy().to_string();
            self.config.show_date_columns = self.show_date_columns.iter()
                .map(|(k, v)| (k.to_string_lossy().to_string(), *v))
                .collect();
            self.config.save();
            
            self.refresh_contents();
            // Restart watcher for the new directory
            self.start_file_watcher();
        }
    }

    fn go_back(&mut self) {
        if let Some(previous) = self.back_history.pop() {
            self.forward_history.push(self.current_path.clone());
            self.current_path = previous;
            self.refresh_contents();
        }
    }

    fn go_forward(&mut self) {
        if let Some(next) = self.forward_history.pop() {
            self.back_history.push(self.current_path.clone());
            self.current_path = next;
            self.refresh_contents();
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
            matches!(ext_str.as_str(), 
                "rs" | "js" | "ts" | "jsx" | "tsx" | "py" | "java" | "c" | "cpp" | "h" | "hpp" | 
                "cs" | "go" | "rb" | "php" | "html" | "css" | "scss" | "json" | "xml" | "yaml" | 
                "yml" | "toml" | "md" | "txt" | "sh" | "bat" | "ps1" | "sql" | "vue" | "svelte"
            )
        } else {
            false
        }
    }

    fn is_archive(path: &PathBuf) -> bool {
        if let Some(ext) = path.extension() {
            let ext_str = ext.to_string_lossy().to_lowercase();
            matches!(ext_str.as_str(), 
                "7z" | "zip" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "iso"
            )
        } else {
            false
        }
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

    fn format_modified_time(time: SystemTime) -> String {
        use std::time::UNIX_EPOCH;
        if let Ok(duration) = time.duration_since(UNIX_EPOCH) {
            let secs = duration.as_secs();
            let days = secs / 86400;
            let epoch_start = 719163; // Days from year 0 to 1970-01-01
            let total_days = epoch_start + days as i64;
            
            // Simple date calculation
            let mut remaining_days = total_days;
            
            // Find the year
            let mut year = (remaining_days / 365) as i32;
            let mut days_in_years = 0i64;
            for y in 0..=year {
                let is_leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
                days_in_years += if is_leap { 366 } else { 365 };
            }
            while days_in_years > total_days {
                year -= 1;
                let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
                days_in_years -= if is_leap { 366 } else { 365 };
            }
            remaining_days = total_days - days_in_years;
            
            // Find month and day
            let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            let days_in_months = if is_leap {
                [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
            } else {
                [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
            };
            
            let mut month = 1;
            for (i, &days_in_month) in days_in_months.iter().enumerate() {
                if remaining_days < days_in_month as i64 {
                    month = i + 1;
                    break;
                }
                remaining_days -= days_in_month as i64;
            }
            let day = remaining_days + 1;
            
            // Time calculation
            let time_secs = secs % 86400;
            let hour = time_secs / 3600;
            let minute = (time_secs % 3600) / 60;
            
            format!("{:04}-{:02}-{:02} {:02}:{:02}", year, month, day, hour, minute)
        } else {
            String::new()
        }
    }

    fn copy_files(sources: &[PathBuf], dest: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        for source in sources {
            if source.is_dir() {
                copy_dir_recursive(source, &dest.join(source.file_name().unwrap()))?;
            } else {
                let file_name = source.file_name().unwrap();
                std::fs::copy(source, dest.join(file_name))?;
            }
        }
        Ok(())
    }

    fn move_files(sources: &[PathBuf], dest: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        for source in sources {
            let file_name = source.file_name().unwrap();
            std::fs::rename(source, dest.join(file_name))?;
        }
        Ok(())
    }
}

fn copy_dir_recursive(src: &PathBuf, dst: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dst.join(&file_name);

        if path.is_dir() {
            copy_dir_recursive(&path, &dest_path)?;
        } else {
            std::fs::copy(&path, &dest_path)?;
        }
    }
    Ok(())
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
            if let Ok(mut watcher) = recommended_watcher(
                move |res| {
                    match res {
                        Ok(notify::event::Event {
                            kind: notify::event::EventKind::Modify(_) 
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
                },
            ) {
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
                        let file_name = path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        let exists_in_list = self.contents.iter().any(|e| e.name == file_name);
                        let exists_on_disk = path.exists();
                        
                        if (exists_on_disk && !exists_in_list) || (!exists_on_disk && exists_in_list) {
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
        }
    }
}

impl eframe::App for RusplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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

        // Process any file system changes detected by watcher
        self.process_file_changes();
        
        // Track window focus and pause/resume background work
        let is_focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if is_focused != self.is_focused {
            self.is_focused = is_focused;
            self.pause_token.store(!is_focused, Ordering::SeqCst);
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
                    self.is_right_click_drag = i.pointer.button_down(egui::PointerButton::Secondary);
                    self.show_drop_menu = self.is_right_click_drag;
                    
                    // Left click defaults to move
                    if !self.is_right_click_drag {
                        let files = self.dragged_files.clone();
                        let dest = self.current_path.clone();
                        std::thread::spawn(move || {
                            let _ = RusplorerApp::move_files(&files, &dest);
                        });
                        self.dragged_files.clear();
                        // Schedule refresh for next frame
                        std::thread::sleep(std::time::Duration::from_millis(100));
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

        // Re-sort when sizes arrive and we're sorting by size
        if sizes_updated && self.sort_column == SortColumn::Size {
            self.sort_contents();
        }

        // Handle mouse buttons 4 and 5 (back/forward)
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::PointerButton { button, pressed, .. } = event {
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
                
                // Edge detection: only trigger on press (transition from not-pressed to pressed)
                let copy_pressed = c_down && !self.prev_ctrl_c_down;
                let paste_pressed = v_down && !self.prev_ctrl_v_down;
                let cut_pressed = x_down && !self.prev_ctrl_x_down;
                let delete_pressed = del_down && !self.prev_del_down;
                
                self.prev_ctrl_c_down = c_down;
                self.prev_ctrl_v_down = v_down;
                self.prev_ctrl_x_down = x_down;
                self.prev_del_down = del_down;
                
                (copy_pressed, cut_pressed, paste_pressed, delete_pressed)
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
            let files: Vec<PathBuf> = self.selected_entries.iter()
                .map(|name| self.current_path.join(name))
                .collect();
            
            #[cfg(windows)]
            {
                if let Ok(_) = copy_files_to_clipboard(&files) {
                    self.clipboard_files = files;
                    self.clipboard_mode = Some(ClipboardMode::Copy);
                }
            }
            #[cfg(not(windows))]
            {
                self.clipboard_files = files;
                self.clipboard_mode = Some(ClipboardMode::Copy);
            }
        }

        if got_cut && !self.selected_entries.is_empty() {
            let files: Vec<PathBuf> = self.selected_entries.iter()
                .map(|name| self.current_path.join(name))
                .collect();
            
            #[cfg(windows)]
            {
                if let Ok(_) = copy_files_to_clipboard(&files) {
                    self.clipboard_files = files;
                    self.clipboard_mode = Some(ClipboardMode::Cut);
                }
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
                // Try to read from Windows clipboard
                if let Ok(clipboard_files) = read_files_from_clipboard() {
                    if !clipboard_files.is_empty() {
                        let dest = self.current_path.clone();
                        
                        // Check if these are our internal cut files
                        let is_cut = self.clipboard_mode == Some(ClipboardMode::Cut) 
                            && clipboard_files == self.clipboard_files;
                        
                        if is_cut {
                            let _ = RusplorerApp::move_files(&clipboard_files, &dest);
                            self.clipboard_files.clear();
                            self.clipboard_mode = None;
                        } else {
                            let _ = RusplorerApp::copy_files(&clipboard_files, &dest);
                        }
                        
                        self.refresh_contents();
                    }
                }
            }
            #[cfg(not(windows))]
            {
                if let Some(mode) = self.clipboard_mode {
                    if !self.clipboard_files.is_empty() {
                        let files = self.clipboard_files.clone();
                        let dest = self.current_path.clone();
                        
                        match mode {
                            ClipboardMode::Copy => {
                                let _ = RusplorerApp::copy_files(&files, &dest);
                            }
                            ClipboardMode::Cut => {
                                let _ = RusplorerApp::move_files(&files, &dest);
                                self.clipboard_files.clear();
                                self.clipboard_mode = None;
                            }
                        }
                        
                        self.refresh_contents();
                    }
                }
            }
        }

        // Handle DEL key - send to recycle bin
        if got_delete && !self.selected_entries.is_empty() {
            let files_to_delete: Vec<PathBuf> = self.selected_entries.iter()
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
                        self.selected_entries.clear();
                        self.refresh_contents();
                    }
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

        egui::CentralPanel::default().show(ctx, |ui| {
            // Drive selector with filter and navigation buttons
            let mut selected_drive: Option<PathBuf> = None;
            ui.horizontal(|ui| {
                ui.label("Drive:");
                for drive in &self.available_drives {
                    let current_drive = self.current_path.to_string_lossy();
                    let is_current = current_drive.starts_with(drive);
                    
                    if ui.selectable_label(is_current, drive).clicked() {
                        selected_drive = Some(PathBuf::from(drive));
                    }
                }
                
                // Filter in the middle
                ui.label("Filter:");
                ui.allocate_ui(egui::vec2(70.0, 20.0), |ui| {
                    ui.text_edit_singleline(&mut self.filter);
                });
                
                // Add space and push navigation buttons to the right
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("🔄").on_hover_text("Refresh").clicked() {
                        self.refresh_contents();
                    }
                    
                    let forward_enabled = !self.forward_history.is_empty();
                    if ui.add_enabled(forward_enabled, egui::Button::new("▶")).clicked() {
                        self.go_forward();
                    }
                    
                    let back_enabled = !self.back_history.is_empty();
                    if ui.add_enabled(back_enabled, egui::Button::new("◀")).clicked() {
                        self.go_back();
                    }
                });
            });
            
            // Handle drive selection
            if let Some(drive) = selected_drive {
                self.navigate_to(drive);
            }

            ui.separator();

            // Breadcrumbs
            let breadcrumbs = self.get_breadcrumbs();
            let mut navigate_to_path: Option<PathBuf> = None;
            
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = [5.0, 5.0].into();
                
                for (i, (path, name)) in breadcrumbs.iter().enumerate() {
                    let is_last = i == breadcrumbs.len() - 1;
                    
                    if i > 0 {
                        ui.vertical(|ui| {
                            ui.add_space(5.0);
                            ui.label("/");
                        });
                    }
                    
                    if is_last {
                        // Current directory - not clickable, just plain text
                        ui.vertical(|ui| {
                            ui.add_space(3.0);
                            ui.label(name);
                        });
                    } else {
                        // Parent directories - clickable pills
                        let button = egui::Button::new(egui::RichText::new(name).color(egui::Color32::BLACK))
                            .fill(egui::Color32::from_rgb(255, 245, 150))
                            .frame(true);
                        if ui.add(button).clicked() {
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

            // Table with proper column alignment
            let show_dates = self.show_date_columns.get(&self.current_path).copied().unwrap_or(false);
            let mut sort_changed = false;

            // Clear entry rects for this frame
            self.entry_rects.clear();
            self.any_button_hovered = false;

            let row_height = 18.0;
            
            // Measure actual text widths for tight columns
            let font_id = egui::TextStyle::Body.resolve(ui.style());
            let size_text_width = ui.fonts(|f| f.layout_no_wrap("999.9 TB".to_string(), font_id.clone(), egui::Color32::WHITE).size().x);
            let date_text_width = if show_dates {
                ui.fonts(|f| f.layout_no_wrap("2026-02-17 14:30".to_string(), font_id.clone(), egui::Color32::WHITE).size().x)
            } else {
                0.0
            };
            
            // Calculate exact column widths from available space
            let available = ui.available_width();
            let size_col_w = size_text_width + 8.0;  // small padding
            let date_col_w = if show_dates { date_text_width + 20.0 } else { 18.0 }; // +20 for X button + padding
            let name_col_w = (available - size_col_w - date_col_w - 15.0).max(50.0);
            
            let table_builder = TableBuilder::new(ui)
                .striped(true)
                .resizable(false)
                .vscroll(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(name_col_w).clip(true))
                .column(Column::exact(size_col_w))
                .column(Column::exact(date_col_w));

            table_builder
                .header(row_height, |mut header| {
                    // Name header
                    header.col(|ui| {
                        let arrow = if self.sort_column == SortColumn::Name {
                            if self.sort_ascending { " ^" } else { " v" }
                        } else { "" };
                        let text = format!("Name{}", arrow);
                        if ui.add_sized(
                            ui.available_size(),
                            egui::Button::new(egui::RichText::new(&text).strong())
                        ).clicked() {
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
                            if self.sort_ascending { " ^" } else { " v" }
                        } else { "" };
                        let text = format!("Size{}", arrow);
                        if ui.add_sized(
                            ui.available_size(),
                            egui::Button::new(egui::RichText::new(&text).strong())
                        ).clicked() {
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
                                if ui.small_button("X").on_hover_text("Hide date column").clicked() {
                                    self.show_date_columns.insert(self.current_path.clone(), false);
                                    if self.sort_column == SortColumn::Date {
                                        self.sort_column = SortColumn::Name;
                                        self.sort_ascending = true;
                                    }
                                    sort_changed = true;
                                }
                                let arrow = if self.sort_column == SortColumn::Date {
                                    if self.sort_ascending { " ^" } else { " v" }
                                } else { "" };
                                let text = format!("Modified{}", arrow);
                                if ui.add_sized(
                                    egui::vec2(ui.available_width(), ui.available_height()),
                                    egui::Button::new(egui::RichText::new(&text).strong())
                                ).clicked() {
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
                            if ui.small_button("📅").on_hover_text("Show modification date").clicked() {
                                self.show_date_columns.insert(self.current_path.clone(), true);
                                self.sort_column = SortColumn::Date;
                                self.sort_ascending = false;
                                sort_changed = true;
                            }
                        }
                    });
                })
                .body(|mut body| {
                    for entry in self.contents.clone() {
                        // Filter
                        if !entry.name.starts_with("[..]") && !self.filter.is_empty() {
                            if !entry.name.to_lowercase().contains(&self.filter.to_lowercase()) {
                                continue;
                            }
                        }

                        let is_selected = self.selected_entries.contains(&entry.name);
                        let is_in_clipboard = self.clipboard_files.contains(&self.current_path.join(&entry.name));
                        let full_path = self.current_path.join(&entry.name);
                        let is_computing = entry.is_dir && !entry.name.starts_with("[..]")
                            && !self.dirs_done.contains(&full_path);

                        let size_label = if entry.name.starts_with("[..]") {
                            String::new()
                        } else {
                            match self.file_sizes.get(&full_path) {
                                Some(size) => Self::format_file_size(*size),
                                None => if entry.is_dir { "0 B".to_string() } else { "...".to_string() },
                            }
                        };

                        body.row(row_height, |mut row| {
                            // Name column
                            row.col(|ui| {
                                let button = if is_selected && is_in_clipboard {
                                    egui::Button::new(egui::RichText::new(&entry.name).color(egui::Color32::WHITE).italics())
                                        .fill(egui::Color32::from_rgb(100, 150, 255))
                                        .frame(false)
                                } else if is_selected {
                                    egui::Button::new(egui::RichText::new(&entry.name).color(egui::Color32::WHITE))
                                        .fill(egui::Color32::from_rgb(100, 150, 255))
                                        .frame(false)
                                } else if is_in_clipboard && entry.is_dir {
                                    egui::Button::new(egui::RichText::new(&entry.name).italics())
                                        .fill(egui::Color32::from_rgb(255, 245, 150))
                                        .frame(false)
                                } else if is_in_clipboard {
                                    egui::Button::new(egui::RichText::new(&entry.name).italics())
                                        .frame(false)
                                } else if entry.name.starts_with("[..]") {
                                    egui::Button::new(&entry.name)
                                        .fill(egui::Color32::TRANSPARENT)
                                        .frame(false)
                                } else if entry.is_dir {
                                    egui::Button::new(&entry.name)
                                        .fill(egui::Color32::from_rgb(255, 245, 150))
                                        .frame(false)
                                } else {
                                    egui::Button::new(&entry.name)
                                        .frame(false)
                                };

                                let response = ui.add_sized(
                                    egui::vec2(ui.available_width(), ui.available_height()),
                                    button,
                                );

                                self.entry_rects.insert(entry.name.clone(), response.rect);
                                if response.hovered() {
                                    self.any_button_hovered = true;
                                }

                                if response.clicked() {
                                    let is_ctrl = ui.input(|i| i.modifiers.ctrl);
                                    if is_ctrl {
                                        if self.selected_entries.contains(&entry.name) {
                                            self.selected_entries.remove(&entry.name);
                                        } else {
                                            self.selected_entries.insert(entry.name.clone());
                                        }
                                    } else {
                                        self.selected_entries.clear();
                                        self.selected_entries.insert(entry.name.clone());
                                    }
                                }

                                if response.secondary_clicked() {
                                    self.show_context_menu = true;
                                    self.context_menu_entry = Some(entry.clone());
                                    self.context_menu_position = ui.input(|i| i.pointer.hover_pos().unwrap_or_default());
                                }

                                if response.double_clicked() {
                                    if entry.name.starts_with("[..]") {
                                        self.selected_action = Some(FileAction::GoToParent);
                                    } else if entry.is_dir {
                                        let new_path = self.current_path.join(&entry.name);
                                        self.selected_action = Some(FileAction::OpenDir(new_path));
                                    } else {
                                        let full_path = self.current_path.join(&entry.name);
                                        let _ = std::process::Command::new("explorer")
                                            .arg(&full_path)
                                            .spawn();
                                    }
                                }

                                // Draw size bar at bottom of cell
                                if !entry.name.starts_with("[..]") {
                                    if let Some(size) = self.file_sizes.get(&full_path) {
                                        let bar_width = if self.max_file_size > 0 {
                                            (*size as f32 / self.max_file_size as f32) * response.rect.width()
                                        } else {
                                            0.0
                                        };
                                        let bar_rect = egui::Rect::from_min_size(
                                            egui::pos2(response.rect.left(), response.rect.bottom() - 2.0),
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
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if !size_label.is_empty() {
                                        let size_text = if is_in_clipboard {
                                            egui::RichText::new(&size_label).weak().italics()
                                        } else {
                                            egui::RichText::new(&size_label).weak()
                                        };
                                        ui.label(size_text);
                                    }
                                    if is_computing {
                                        let spinner_chars = ['⏳', '⌛'];
                                        let time = ui.input(|i| i.time);
                                        let idx = ((time * 2.0) as usize) % spinner_chars.len();
                                        ui.label(spinner_chars[idx].to_string());
                                        ctx.request_repaint();
                                    }
                                });
                            });

                            // Date column - right aligned, tight
                            row.col(|ui| {
                                if show_dates && !entry.name.starts_with("[..]") {
                                    let date_text = if let Some(modified) = entry.modified {
                                        Self::format_modified_time(modified)
                                    } else {
                                        String::new()
                                    };
                                    if !date_text.is_empty() {
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            let label = if is_in_clipboard {
                                                egui::RichText::new(&date_text).weak().italics()
                                            } else {
                                                egui::RichText::new(&date_text).weak()
                                            };
                                            ui.label(label);
                                        });
                                    }
                                }
                            });
                        });
                    }
                });

            // Handle rectangular selection
            ctx.input(|i| {
                if let Some(pointer_pos) = i.pointer.hover_pos() {
                    if i.pointer.primary_pressed() && !self.any_button_hovered {
                        self.is_dragging_selection = true;
                        self.selection_drag_start = Some(pointer_pos);
                        self.selection_drag_current = Some(pointer_pos);
                    }
                    if self.is_dragging_selection && i.pointer.primary_down() {
                        self.selection_drag_current = Some(pointer_pos);
                    }
                    if self.is_dragging_selection && !i.pointer.primary_down() {
                        if let (Some(start), Some(end)) = (self.selection_drag_start, self.selection_drag_current) {
                            let sel_rect = egui::Rect::from_two_pos(start, end);
                            if !i.modifiers.ctrl {
                                self.selected_entries.clear();
                            }
                            for (name, rect) in &self.entry_rects {
                                if sel_rect.intersects(*rect) && !name.starts_with("[..]") {
                                    self.selected_entries.insert(name.clone());
                                }
                            }
                        }
                        self.is_dragging_selection = false;
                        self.selection_drag_start = None;
                        self.selection_drag_current = None;
                    }
                }
                if i.pointer.primary_clicked() && !self.any_button_hovered && !self.is_dragging_selection {
                    self.selected_entries.clear();
                }
            });

            if sort_changed {
                self.sort_contents();
                self.config.sort_column = self.sort_column.clone();
                self.config.sort_ascending = self.sort_ascending;
                self.config.show_date_columns = self.show_date_columns.iter()
                    .map(|(k, v)| (k.to_string_lossy().to_string(), *v))
                    .collect();
                self.config.save();
            }

            // Draw selection rectangle if dragging
            if let (Some(start), Some(current)) = (self.selection_drag_start, self.selection_drag_current) {
                let sel_rect = egui::Rect::from_two_pos(start, current);
                ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("selection_rect")))
                    .rect_stroke(sel_rect, 0.0, egui::Stroke::new(2.0, egui::Color32::from_rgb(100, 150, 255)));
                ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("selection_rect")))
                    .rect_filled(sel_rect, 0.0, egui::Color32::from_rgba_unmultiplied(100, 150, 255, 30));
            }
        });

        // Drop menu context window
        if self.show_drop_menu && !self.dragged_files.is_empty() {
            egui::Window::new("Copy or Move?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(format!("{} item(s) dropped", self.dragged_files.len()));
                    
                    ui.horizontal(|ui| {
                        if ui.button("Copy").clicked() {
                            let files = self.dragged_files.clone();
                            let dest = self.current_path.clone();
                            std::thread::spawn(move || {
                                let _ = RusplorerApp::copy_files(&files, &dest);
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            });
                            self.show_drop_menu = false;
                            self.dragged_files.clear();
                        }
                        
                        if ui.button("Move").clicked() {
                            let files = self.dragged_files.clone();
                            let dest = self.current_path.clone();
                            std::thread::spawn(move || {
                                let _ = RusplorerApp::move_files(&files, &dest);
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            });
                            self.show_drop_menu = false;
                            self.dragged_files.clear();
                        }

                        if ui.button("Cancel").clicked() {
                            self.show_drop_menu = false;
                            self.dragged_files.clear();
                        }
                    });
                });
        }

        // Refresh contents periodically to catch updates from background threads
        if self.dragged_files.is_empty() && !self.show_drop_menu {
            // Let the file watcher pick up changes
        }

        // Context menu
        if self.show_context_menu {
            if let Some(ref entry) = self.context_menu_entry {
                let full_path = self.current_path.join(&entry.name);
                
                egui::Window::new("Context Menu")
                    .collapsible(false)
                    .resizable(false)
                    .title_bar(false)
                    .fixed_pos(self.context_menu_position)
                    .default_width(0.0)
                    .frame(egui::Frame {
                        fill: egui::Color32::from_rgb(200, 220, 255),
                        stroke: egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                        ..Default::default()
                    })
                    .show(ctx, |ui| {
                        ui.style_mut().spacing.button_padding = egui::vec2(4.0, 2.0);
                        
                        // Open with VS Code
                        if (entry.is_dir || Self::is_code_file(&full_path)) && ui.button("Open with Code").clicked() {
                            let _ = std::process::Command::new("code")
                                .arg(&full_path)
                                .spawn();
                            self.show_context_menu = false;
                        }
                        
                        // Extract here
                        if Self::is_archive(&full_path) && ui.button("Extract here").clicked() {
                            self.extract_archive_path = full_path.clone();
                            self.show_extract_dialog = true;
                            self.show_context_menu = false;
                        }
                        
                        // Add to archive
                        if ui.button("Add to archive").clicked() {
                            self.files_to_archive.clear();
                            if !self.selected_entries.is_empty() {
                                for name in &self.selected_entries {
                                    self.files_to_archive.push(self.current_path.join(name));
                                }
                            } else {
                                self.files_to_archive.push(full_path.clone());
                            }
                            
                            // Default archive name based on first item
                            let stem = if let Some(first) = self.files_to_archive.first() {
                                first.file_stem()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string()
                            } else {
                                "archive".to_string()
                            };
                            self.archive_name_buffer = stem;
                            self.show_archive_dialog = true;
                            self.show_context_menu = false;
                        }
                        
                        ui.separator();
                        
                        // Copy full path
                        if ui.button("📋 Copy full path").clicked() {
                            if let Ok(mut clipboard) = Clipboard::new() {
                                let _ = clipboard.set_text(full_path.to_string_lossy().to_string());
                            }
                            self.show_context_menu = false;
                        }
                        
                        // Rename
                        if !entry.name.starts_with("[..]") && ui.button("Rename").clicked() {
                            self.rename_buffer = entry.name.clone();
                            self.show_rename_dialog = true;
                            self.show_context_menu = false;
                        }
                        
                        // Properties
                        if ui.button("Properties").clicked() {
                            let _ = std::process::Command::new("explorer")
                                .args(&["/select,", &full_path.to_string_lossy()])
                                .spawn();
                            self.show_context_menu = false;
                        }
                        
                        ui.separator();
                        
                        if ui.button("Cancel").clicked() {
                            self.show_context_menu = false;
                        }
                    });
            }
            
            // Close context menu if clicked elsewhere
            if ctx.input(|i| i.pointer.primary_clicked() || i.key_pressed(egui::Key::Escape)) {
                self.show_context_menu = false;
            }
        }

        // Archive dialog
        if self.show_archive_dialog {
            // Draw semi-transparent backdrop
            let screen_rect = ctx.screen_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::PanelResizeLine, egui::Id::new("archive_backdrop")));
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
                    
                    ui.label(format!("{} item(s) to archive", self.files_to_archive.len()));
                    
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
                            
                            let archive_path = self.current_path.join(
                                format!("{}.{}", self.archive_name_buffer, ext)
                            );
                            let archive_str = archive_path.to_string_lossy().to_string();
                            
                            let archive_filename = format!("{}.{}", self.archive_name_buffer, ext);
                            let files_clone = self.files_to_archive.clone();
                            let (done_tx, done_rx) = channel();
                            let archive_str_clone = archive_str.clone();
                            let format_flag = format_flag.to_string();
                            let level_flag = level_flag.to_string();
                            
                            std::thread::spawn(move || {
                                let mut cmd = std::process::Command::new("C:\\Program Files\\7-Zip\\7z.exe");
                                cmd.args(&["a", &format_flag, &level_flag, &archive_str_clone]);
                                for f in &files_clone {
                                    cmd.arg(f);
                                }
                                let result = cmd.spawn().or_else(|_| {
                                    let mut cmd2 = std::process::Command::new("7z.exe");
                                    cmd2.args(&["a", &format_flag, &level_flag, &archive_str_clone]);
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
            let dest = self.current_path.clone();
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
        if self.show_rename_dialog {
            if let Some(entry) = self.context_menu_entry.clone() {
                let entry_name = entry.name.clone();
                egui::Window::new("Rename")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label("New name:");
                        let response = ui.text_edit_singleline(&mut self.rename_buffer);
                        
                        // Auto-focus the text field
                        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            // Perform rename
                            let old_path = self.current_path.join(&entry_name);
                            let new_path = self.current_path.join(&self.rename_buffer);
                            if let Err(_) = std::fs::rename(&old_path, &new_path) {
                                // Error handling could be improved
                            }
                            self.show_rename_dialog = false;
                            self.refresh_contents();
                        }
                        
                        ui.horizontal(|ui| {
                            if ui.button("OK").clicked() {
                                let old_path = self.current_path.join(&entry_name);
                                let new_path = self.current_path.join(&self.rename_buffer);
                                if let Err(_) = std::fs::rename(&old_path, &new_path) {
                                    // Error handling could be improved
                                }
                                self.show_rename_dialog = false;
                                self.refresh_contents();
                            }
                            
                            if ui.button("Cancel").clicked() {
                                self.show_rename_dialog = false;
                            }
                        });
                    });
            }
        }

        // Extract dialog
        if self.show_extract_dialog {
            // Draw semi-transparent backdrop
            let screen_rect = ctx.screen_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::PanelResizeLine, egui::Id::new("extract_backdrop")));
            painter.rect_filled(screen_rect, 0.0, egui::Color32::from_black_alpha(128));

            let archive_name = self.extract_archive_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            egui::Window::new("Extracting...")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(format!("Extracting: {}", archive_name));
                    ui.label("Please wait...");
                });
        }

        // Only repaint if sizes are still being loaded or user is interacting
        if self.size_receiver.is_some() {
            ctx.request_repaint();
        } else {
            // No active operations, repaint at a lower rate
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}

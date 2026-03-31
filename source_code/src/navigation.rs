//! Navigation helpers: tab management, directory traversal, browsing history,
//! breadcrumbs, and the background directory-listing logic (`refresh_contents`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;

use crate::types::{DriveKind, FileEntry, SortColumn, TabState};
use crate::fs_ops::{calculate_dir_size_progressive, read_dir_children};
use super::RusplorerApp;

impl RusplorerApp {
    /// Collapse the entire tree, then expand only the ancestors of `path`.
    /// This ensures unrelated drives/folders are hidden after every navigation.
    pub(crate) fn expand_tree_to(&mut self, path: &PathBuf) {
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
    pub(crate) fn save_active_tab(&mut self) {
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
    pub(crate) fn restore_tab(&mut self, index: usize) {
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
    pub(crate) fn switch_to_tab(&mut self, index: usize) {
        if index == self.active_tab || index >= self.tabs.len() {
            return;
        }
        self.save_active_tab();
        self.active_tab = index;
        self.restore_tab(index);
    }

    /// Open a new tab.  Clones the current path by default.
    pub(crate) fn new_tab(&mut self, path: Option<PathBuf>) {
        self.save_active_tab();
        let tab_path = path.unwrap_or_else(|| self.current_path.clone());
        self.tabs.push(TabState::new(tab_path));
        self.active_tab = self.tabs.len() - 1;
        self.restore_tab(self.active_tab);
    }

    /// Close the tab at `index`.  Won't close the last remaining tab.
    pub(crate) fn close_tab(&mut self, index: usize) {
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

    pub(crate) fn refresh_contents(&mut self) {
        // Cancel any running background computation
        self.cancel_token.store(true, Ordering::SeqCst);
        self.cancel_token = Arc::new(AtomicBool::new(false));

        self.contents.clear();
        self.file_sizes.clear();
        self.max_file_size = 0;
        self.dirs_done.clear();
        // Reset type-to-select state so a new folder starts fresh.
        self.type_select_char = None;
        self.type_select_index = 0;

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
                    // Use whichever is more recent: modified or created.
                    // This handles files copied from older sources where the
                    // creation date can be newer than the modification date.
                    let modified = e.metadata().ok().and_then(|m| {
                        let mtime = m.modified().ok();
                        let ctime = m.created().ok();
                        match (mtime, ctime) {
                            (Some(m), Some(c)) => Some(m.max(c)),
                            (Some(m), None)    => Some(m),
                            (None,    Some(c)) => Some(c),
                            (None,    None)    => None,
                        }
                    });
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
                            SortColumn::Size => std::cmp::Ordering::Equal,
                        };
                        if self.sort_ascending { ord } else { ord.reverse() }
                    }
                }
            });

            self.contents.extend(items);
        }

        // Collect paths for background processing
        let mut file_paths: Vec<PathBuf> = Vec::new();
        let mut dir_paths:  Vec<PathBuf> = Vec::new();
        for entry in &self.contents {
            if entry.name.starts_with("[..]") { continue; }
            let full_path = self.current_path.join(&entry.name);
            if entry.is_dir { dir_paths.push(full_path); } else { file_paths.push(full_path); }
        }

        // Start background thread to load file and folder sizes
        let cancel_token = self.cancel_token.clone();
        let pause_token  = self.pause_token.clone();
        let (tx, rx)           = channel();
        let (done_tx, done_rx) = channel::<PathBuf>();

        std::thread::spawn(move || {
            // First: send all file sizes immediately (fast)
            for path in file_paths {
                if cancel_token.load(Ordering::SeqCst) { return; }
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
                    let queue    = work_queue.clone();
                    let cancel   = cancel_token.clone();
                    let pause    = pause_token.clone();
                    let tx       = tx.clone();
                    let done_tx  = done_tx.clone();

                    handles.push(std::thread::spawn(move || {
                        loop {
                            let dir_path = {
                                match queue.lock() {
                                    Ok(mut dirs) => dirs.pop(),
                                    Err(_) => break,
                                }
                            };
                            let dir_path = match dir_path { Some(p) => p, None => break };
                            if cancel.load(Ordering::SeqCst) { return; }
                            while pause.load(Ordering::SeqCst) {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                                if cancel.load(Ordering::SeqCst) { return; }
                            }
                            let mut accumulated = 0u64;
                            calculate_dir_size_progressive(
                                &dir_path, &dir_path, &cancel, &pause, &tx, &mut accumulated,
                            );
                            let _ = tx.send((dir_path.clone(), accumulated));
                            let _ = done_tx.send(dir_path);
                        }
                    }));
                }
                for handle in handles { let _ = handle.join(); }
            }
        });

        self.dirs_done_receiver = Some(done_rx);
        self.size_receiver = Some(rx);
    }

    pub(crate) fn sort_contents(&mut self) {
        let sort_column   = &self.sort_column;
        let sort_ascending = self.sort_ascending;
        let file_sizes    = &self.file_sizes;
        let current_path  = &self.current_path;

        self.contents.sort_by(|a, b| {
            if a.name.starts_with("[..]") { return std::cmp::Ordering::Less;  }
            if b.name.starts_with("[..]") { return std::cmp::Ordering::Greater; }
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

    /// Returns `true` when `path` lives on a drive that may need to spin up
    /// (HDD, removable USB/SD, or Network) and would therefore block the UI.
    pub(crate) fn is_slow_drive(&self, path: &std::path::Path) -> bool {
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

    pub(crate) fn navigate_to(&mut self, path: PathBuf) {
        if path != self.current_path {
            self.back_history.push(self.current_path.clone());
            self.forward_history.clear();
        }
        self.commit_navigation(path);
    }

    /// Shared "point the UI at `path` and load it, respecting spin-up for slow drives".
    /// History manipulation is handled by the callers; this only does the load.
    pub(crate) fn commit_navigation(&mut self, path: PathBuf) {
        self.current_path = path.clone();
        self.config.last_path = self.current_path.to_string_lossy().to_string();
        self.config.show_date_columns = self
            .show_date_columns
            .iter()
            .map(|(k, v)| (k.to_string_lossy().to_string(), *v))
            .collect();
        self.config.save();

        if self.is_slow_drive(&path) {
            // Do NOT call expand_tree_to here — it calls read_dir_children which
            // blocks the main thread while the disk spins up.  Defer it to the
            // spin-done handler in the update loop.
            self.contents.clear();
            self.cancel_token.store(true, Ordering::SeqCst);
            self.loading_path = Some(path.clone());
            self.save_active_tab();
            let (tx, rx) = std::sync::mpsc::channel::<bool>();
            self.dir_load_receiver = Some(rx);
            std::thread::spawn(move || {
                let ok = path.exists() && path.is_dir();
                let _ = tx.send(ok);
            });
        } else {
            let path_snap = self.current_path.clone();
            self.expand_tree_to(&path_snap);
            self.save_active_tab();
            self.loading_path = None;
            self.dir_load_receiver = None;
            self.refresh_contents();
            self.start_file_watcher();
        }
    }

    pub(crate) fn go_back(&mut self) {
        if let Some(previous) = self.back_history.pop() {
            self.forward_history.push(self.current_path.clone());
            self.commit_navigation(previous);
        }
    }

    pub(crate) fn go_forward(&mut self) {
        if let Some(next) = self.forward_history.pop() {
            self.back_history.push(self.current_path.clone());
            self.commit_navigation(next);
        }
    }

    pub(crate) fn get_breadcrumbs(&self) -> Vec<(PathBuf, String)> {
        let mut breadcrumbs = Vec::new();
        let mut path = self.current_path.clone();

        if let Some(parent) = path.parent() {
            if parent != path {
                let mut components = Vec::new();
                loop {
                    if let Some(file_name) = path.file_name() {
                        if let Some(name_str) = file_name.to_str() {
                            components.push((path.clone(), name_str.to_string()));
                        }
                    }
                    if let Some(parent) = path.parent() {
                        if parent == path { break; }
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

    pub(crate) fn format_path_display(path: &PathBuf) -> String {
        path.to_string_lossy().replace("\\", "/")
    }
}

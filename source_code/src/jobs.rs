//! Background copy/move jobs, the file-system watcher, and Recycle Bin restore.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::channel;

use notify::{RecursiveMode, Watcher, recommended_watcher};

use crate::fs_ops::{spawn_copy_job, read_dir_children, CopyJobState};
use super::RusplorerApp;

#[cfg(windows)]
fn normalize_windows_path_for_compare(path: &std::path::Path) -> String {
    let mut s = path.to_string_lossy().to_string();
    s = s.replace('/', "\\");
    if let Some(rest) = s.strip_prefix("\\\\?\\") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_prefix("\\??\\") {
        s = rest.to_string();
    }
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s.make_ascii_lowercase();
    s
}

#[cfg(windows)]
fn windows_path_eq(a: &std::path::Path, b: &std::path::Path) -> bool {
    normalize_windows_path_for_compare(a) == normalize_windows_path_for_compare(b)
}

#[cfg(windows)]
struct RestoreSummary {
    total: usize,
    restored: usize,
    failed: Vec<PathBuf>,
}

fn normalize_tree_dir_key(path: &std::path::Path) -> String {
    let mut s = path.to_string_lossy().to_string();
    while s.ends_with('\\') || s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) {
        s.make_ascii_lowercase();
    }
    s
}

fn tree_dir_key_eq(a: &std::path::Path, b: &std::path::Path) -> bool {
    normalize_tree_dir_key(a) == normalize_tree_dir_key(b)
}

#[derive(Clone, Copy)]
pub(crate) enum WatchEventKind {
    Modify,
    Structural,
}

pub(crate) struct WatchEvent {
    pub(crate) kind: WatchEventKind,
    pub(crate) path: PathBuf,
}

/// Returns the set of uppercase drive letters touched by a copy job.
/// On non-Windows or UNC paths the set may be empty (no queuing penalty).
fn drives_of(sources: &[PathBuf], dest: &PathBuf) -> std::collections::HashSet<char> {
    use std::path::{Component, Prefix};
    let mut set = std::collections::HashSet::new();
    for p in sources.iter().chain(std::iter::once(dest)) {
        if let Some(Component::Prefix(prefix)) = p.components().next() {
            if let Prefix::Disk(b) = prefix.kind() {
                set.insert((b as char).to_ascii_uppercase());
            }
        }
    }
    set
}

impl RusplorerApp {
    /// Start an async delete-to-recycle-bin job. Returns immediately.
    pub(crate) fn start_delete_job(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() || self.delete_done_receiver.is_some() {
            return;
        }

        let (tx, rx) = channel::<(Vec<PathBuf>, bool)>();
        let (status_tx, status_rx) = channel::<String>();
        let worker_paths = paths.clone();
        std::thread::spawn(move || {
            let _ = status_tx.send("Deletion attempt...".to_string());
            let _ = status_tx.send("Deletion in progress...".to_string());

            let mut ok = crate::fs_ops::delete_to_recycle_bin(&worker_paths);
            if !ok {
                let _ = status_tx.send("Folder is locked. Attempting unlock + delete...".to_string());
                ok = crate::fs_ops::force_unlock_and_delete(&worker_paths);
            }

            let _ = tx.send((worker_paths, ok));
        });

        self.delete_done_receiver = Some(rx);
        self.delete_status_receiver = Some(status_rx);
        self.delete_feedback_msg = Some("Deletion attempt...".to_string());
        self.delete_feedback_until = None;
        self.delete_feedback_is_error = false;
    }

    /// Refresh tree state after removing one or more paths from disk.
    /// This keeps the left panel from showing stale folders after delete.
    pub(crate) fn invalidate_tree_after_delete(&mut self, paths: &[PathBuf]) {
        use std::collections::HashSet;

        if paths.is_empty() {
            return;
        }

        let deleted: HashSet<PathBuf> = paths.iter().cloned().collect();
        let mut parents_to_refresh: HashSet<PathBuf> = HashSet::new();

        for deleted_path in paths {
            let stale_keys: Vec<PathBuf> = self
                .tree_children_cache
                .keys()
                .filter(|cached| cached.as_path().starts_with(deleted_path))
                .cloned()
                .collect();

            for stale in stale_keys {
                self.tree_children_cache.remove(&stale);
                self.tree_expanded.remove(&stale);
            }

            if let Some(parent) = deleted_path.parent() {
                let parent = parent.to_path_buf();
                if !deleted.contains(&parent) {
                    parents_to_refresh.insert(parent);
                }
            }
        }

        for parent in parents_to_refresh {
            let updated = read_dir_children(&parent);
            self.tree_children_cache.insert(parent, updated);
        }
    }

    /// Refresh the cached children list for `dir` in the tree panel.
    ///
    /// We update every cache entry that points to the same directory even if
    /// path spelling differs (case or trailing separator), then ensure an entry
    /// exists for `dir` itself.
    pub(crate) fn refresh_tree_children_for_dir(&mut self, dir: &PathBuf) {
        let updated = read_dir_children(dir);
        let mut matched_any = false;

        let keys: Vec<PathBuf> = self.tree_children_cache.keys().cloned().collect();
        for key in keys {
            if tree_dir_key_eq(&key, dir) {
                self.tree_children_cache.insert(key, updated.clone());
                matched_any = true;
            }
        }

        if !matched_any {
            self.tree_children_cache.insert(dir.clone(), updated);
        }
    }

    /// Start an async background copy/move job.  Returns immediately.
    /// `clear_clipboard` – set `true` for cut-paste so the clipboard is cleared on completion.
    /// `no_undo` – set `true` when called from the undo system itself so the reverse job
    /// does not push another entry onto the undo stack (prevents undo-of-undo chains).
    pub(crate) fn start_copy_job(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        is_move: bool,
        clear_clipboard: bool,
        no_undo: bool,
    ) {
        #[cfg(windows)]
        crate::ole::log_dnd(&format!(
            "StartCopyJob: {} sources is_move={} dest={}",
            sources.len(), is_move, dest.display()
        ));
        let dest_display = dest.to_string_lossy().to_string();
        let state = Arc::new(CopyJobState::new(is_move, dest_display));
        state.clear_clipboard.store(clear_clipboard, Ordering::Relaxed);
        state.no_undo.store(no_undo, Ordering::Relaxed);
        // Record original sources so the undo system can reverse a move.
        *state.original_sources.lock().unwrap() = sources.clone();
        let new_drives = drives_of(&sources, &dest);
        // Queue if any running job touches the same drive(s) to avoid thrashing.
        let conflict = self.copy_job_drives.iter()
            .any(|d| d.intersection(&new_drives).next().is_some());
        if conflict {
            self.copy_pending.push_back((sources, dest, state));
        } else {
            spawn_copy_job(sources, dest, Arc::clone(&state));
            self.copy_job_drives.push(new_drives);
            self.copy_jobs.push(state);
        }
    }

    /// Scan the pending queue and launch every job whose drives are now free.
    pub(crate) fn advance_copy_queue(&mut self) {
        let mut i = 0;
        while i < self.copy_pending.len() {
            let (sources, dest, _) = &self.copy_pending[i];
            let d = drives_of(sources, dest);
            let conflict = self.copy_job_drives.iter()
                .any(|bd| bd.intersection(&d).next().is_some());
            if !conflict {
                let (sources, dest, state) = self.copy_pending.remove(i).unwrap();
                let d = drives_of(&sources, &dest);
                spawn_copy_job(sources, dest, Arc::clone(&state));
                self.copy_job_drives.push(d);
                self.copy_jobs.push(state);
                // Don't increment — the next item has shifted into slot i,
                // and we've added a new busy-drive entry so re-check from i.
            } else {
                i += 1;
            }
        }
    }

    pub(crate) fn start_file_watcher(&mut self) {
        // Signal old watcher to stop
        if let Some(stop_tx) = self.stop_watcher.take() {
            let _ = stop_tx.send(());
        }

        let (tx, rx) = channel();
        let (stop_tx, stop_rx) = channel();
        let current_path = self.current_path.clone();

        let tx = std::sync::Arc::new(std::sync::Mutex::new(tx));

        std::thread::spawn(move || {
            let tx = tx.clone();
            if let Ok(mut watcher) = recommended_watcher(move |res| {
                match res {
                    Ok(notify::event::Event { kind, paths, .. }) => {
                        let event_kind = match kind {
                            notify::event::EventKind::Modify(_) => WatchEventKind::Modify,
                            notify::event::EventKind::Create(_)
                            | notify::event::EventKind::Remove(_) => WatchEventKind::Structural,
                            _ => return,
                        };
                        for path in paths {
                            if let Ok(tx) = tx.lock() {
                                let _ = tx.send(WatchEvent { kind: event_kind, path });
                            }
                        }
                    }
                    _ => {}
                }
            }) {
                match watcher.watch(&current_path, RecursiveMode::NonRecursive) {
                    Ok(_) => { let _ = stop_rx.recv(); }
                    Err(_) => { return; }
                }
            }
        });

        self.watch_receiver = Some(rx);
        self.stop_watcher = Some(stop_tx);
    }

    pub(crate) fn process_file_changes(&mut self) {
        // Smart watcher strategy:
        // - file modify events update only that file's displayed size (cheap)
        // - structural events (create/remove/unknown) trigger a debounced full refresh
        // This avoids restarting directory-size scans every few hundred ms while
        // a large download/copy is appending to a single file.
        if let Some(ref rx) = self.watch_receiver {
            let mut modified_any = false;
            let mut must_recompute_max = false;
            while let Ok(event) = rx.try_recv() {
                let path = event.path;
                if path.parent().map_or(false, |p| p == self.current_path) {
                    match event.kind {
                        WatchEventKind::Modify => {
                            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                                if let Some(entry) = self.contents.iter().find(|e| e.name == name) {
                                    if !entry.is_dir {
                                        let old_size = self.file_sizes.get(&path).copied().unwrap_or(0);
                                        let new_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                                        if old_size != new_size {
                                            self.file_sizes.insert(path.clone(), new_size);
                                            modified_any = true;
                                            if new_size > self.max_file_size || old_size == self.max_file_size {
                                                must_recompute_max = true;
                                            }
                                        }
                                    } else {
                                        self.watch_pending_refresh = Some(std::time::Instant::now());
                                    }
                                } else {
                                    // Unknown path in current folder: keep behavior safe.
                                    self.watch_pending_refresh = Some(std::time::Instant::now());
                                }
                            }
                        }
                        WatchEventKind::Structural => {
                            self.watch_pending_refresh = Some(std::time::Instant::now());
                        }
                    }
                }
            }

            if modified_any {
                if must_recompute_max {
                    let mut max = 0u64;
                    for entry in &self.contents {
                        if entry.name.starts_with("[..]") { continue; }
                        let full_path = self.current_path.join(&entry.name);
                        if let Some(size) = self.file_sizes.get(&full_path) {
                            max = max.max(*size);
                        }
                    }
                    self.max_file_size = max;
                }
                if self.sort_column == crate::types::SortColumn::Size {
                    self.sort_contents();
                }
            }
        }
    }

    /// Call each frame: fires the debounced full refresh after 500 ms of quiet
    /// (only for structural changes, not for regular file growth).
    pub(crate) fn flush_watch_debounce(&mut self) {
        if let Some(t) = self.watch_pending_refresh {
            if t.elapsed().as_millis() >= 500 {
                self.watch_pending_refresh = None;
                self.refresh_contents();
                let updated_children = read_dir_children(&self.current_path.clone());
                self.tree_children_cache.insert(self.current_path.clone(), updated_children);
            }
        }
    }

    /// Restore files from the Windows Recycle Bin by matching against their original paths.
    /// Returns a per-item summary for richer user feedback.
    #[cfg(windows)]
    fn restore_from_recycle_bin(paths: &[PathBuf]) -> RestoreSummary {
        let mut restored = 0usize;
        let total = paths.len();
        let mut failed: Vec<PathBuf> = Vec::new();

        for original_path in paths {
            // Already present at destination: treat as already restored
            // (can happen after partial restore attempts).
            if original_path.exists() {
                restored += 1;
                continue;
            }

            let mut drive = PathBuf::new();
            let mut comp_count = 0;
            for comp in original_path.components() {
                drive.push(comp);
                comp_count += 1;
                if comp_count >= 2 { break; }
            }
            if comp_count < 2 {
                failed.push(original_path.clone());
                continue;
            }
            let recycle_bin = drive.join("$Recycle.Bin");

            let sid_entries = match std::fs::read_dir(&recycle_bin) {
                Ok(e) => e,
                Err(_) => {
                    failed.push(original_path.clone());
                    continue;
                }
            };

            let mut restored_this = false;
            'sid_loop: for sid_entry in sid_entries.flatten() {
                let sid_path = sid_entry.path();
                if !sid_path.is_dir() { continue; }

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

                    if !file_name.starts_with("$I") { continue; }

                    let data = match std::fs::read(&item_path) {
                        Ok(d) => d,
                        Err(_) => continue,
                    };
                    if data.len() < 28 { continue; }

                    let version = i64::from_le_bytes(data[0..8].try_into().unwrap_or([0; 8]));

                    let orig_path_opt: Option<PathBuf> = if version == 1 {
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
                        if windows_path_eq(&orig, original_path) {
                            let r_name = format!("$R{}", &file_name[2..]);
                            let r_path = sid_path.join(&r_name);

                            if r_path.exists() {
                                if let Some(parent) = original_path.parent() {
                                    let _ = std::fs::create_dir_all(parent);
                                }
                                if std::fs::rename(&r_path, original_path).is_ok() {
                                    let _ = std::fs::remove_file(&item_path);
                                    restored += 1;
                                    restored_this = true;
                                    break 'sid_loop;
                                }
                            }
                        }
                    }
                }
            }

            if !restored_this {
                // If this exact item now exists, a previous restore likely recreated it
                // (e.g. via restoring a parent folder). Count as success.
                if original_path.exists() {
                    restored += 1;
                } else {
                    failed.push(original_path.clone());
                }
            }
        }

        RestoreSummary { total, restored, failed }
    }

    /// Apply the topmost undo action in reverse.
    pub(crate) fn apply_undo(&mut self, action: crate::types::UndoAction) {
        use crate::types::UndoAction;
        match action {
            UndoAction::Rename { old_path, new_path } => {
                if let Err(e) = std::fs::rename(&new_path, &old_path) {
                    self.delete_feedback_msg = Some(format!("Undo rename failed: {}", e));
                    self.delete_feedback_until = None;
                    self.delete_feedback_is_error = true;
                }
                // Refresh tree if the item lived in a visible folder.
                if let Some(parent) = new_path.parent() {
                    let updated = crate::fs_ops::read_dir_children(&parent.to_path_buf());
                    self.tree_children_cache.insert(parent.to_path_buf(), updated);
                }
                self.refresh_contents();
            }
            UndoAction::Move { sources, dest } => {
                // Reverse the move: send each file from dest back to its original parent.
                // Group by original parent so each parent gets one copy job (more efficient
                // and mirrors the way the original move was done).
                let mut by_parent: std::collections::HashMap<PathBuf, Vec<PathBuf>> =
                    std::collections::HashMap::new();
                for original in &sources {
                    if let (Some(fname), Some(parent)) =
                        (original.file_name(), original.parent())
                    {
                        let at_dest = dest.join(fname);
                        if at_dest.exists() {
                            by_parent
                                .entry(parent.to_path_buf())
                                .or_default()
                                .push(at_dest);
                        }
                    }
                }
                if by_parent.is_empty() {
                    self.delete_feedback_msg =
                        Some("Undo failed: files no longer found at destination.".to_string());
                    self.delete_feedback_until = None;
                    self.delete_feedback_is_error = true;
                } else {
                    for (parent_dir, files_at_dest) in by_parent {
                        if let Err(e) = std::fs::create_dir_all(&parent_dir) {
                            self.delete_feedback_msg = Some(format!(
                                "Undo move failed creating destination '{}': {}",
                                parent_dir.display(),
                                e
                            ));
                            self.delete_feedback_until = None;
                            self.delete_feedback_is_error = true;
                            continue;
                        }
                        // no_undo=true prevents creating an undo-of-undo entry.
                        self.start_copy_job(files_at_dest, parent_dir, true, false, true);
                    }
                }
                self.refresh_contents();
            }
            UndoAction::Delete { paths } => {
                #[cfg(windows)]
                {
                    let summary = Self::restore_from_recycle_bin(&paths);
                    if summary.restored < summary.total {
                        let first = summary
                            .failed
                            .first()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "unknown item".to_string());
                        self.delete_feedback_msg = Some(format!(
                            "Undo delete incomplete: restored {}/{} item(s). First failure: {}",
                            summary.restored,
                            summary.total,
                            first
                        ));
                        self.delete_feedback_until = None;
                        self.delete_feedback_is_error = true;
                    } else {
                        self.delete_feedback_msg = Some(format!(
                            "Undo delete restored {} item(s).",
                            summary.restored
                        ));
                        self.delete_feedback_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                        self.delete_feedback_is_error = false;
                    }
                }
                #[cfg(not(windows))]
                {
                    self.delete_feedback_msg = Some("Undo delete is only supported on Windows.".to_string());
                    self.delete_feedback_until = None;
                    self.delete_feedback_is_error = true;
                }
                self.refresh_contents();
            }
        }
    }
}

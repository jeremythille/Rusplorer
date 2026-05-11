//! Background copy/move jobs, the file-system watcher, and Recycle Bin restore.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::channel;

use notify::{RecursiveMode, Watcher, recommended_watcher};

use crate::fs_ops::{spawn_copy_job, read_dir_children, CopyJobState};
use super::RusplorerApp;

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

    /// Start an async background copy/move job.  Returns immediately.
    /// `clear_clipboard` – set `true` for cut-paste so the clipboard is cleared on completion.
    pub(crate) fn start_copy_job(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        is_move: bool,
        clear_clipboard: bool,
    ) {
        let dest_display = dest.to_string_lossy().to_string();
        let state = Arc::new(CopyJobState::new(is_move, dest_display));
        state.clear_clipboard.store(clear_clipboard, Ordering::Relaxed);
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
                    Ok(notify::event::Event {
                        kind:
                            notify::event::EventKind::Modify(_)
                            | notify::event::EventKind::Create(_)
                            | notify::event::EventKind::Remove(_),
                        paths,
                        ..
                    }) => {
                        for path in paths {
                            if let Ok(tx) = tx.lock() {
                                let _ = tx.send(path);
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
        // Drain all watcher events without making any filesystem calls (which would
        // block the UI thread during large copies).  Any event for the current
        // directory simply marks that a debounced refresh is needed; the actual
        // disk I/O happens later in flush_watch_debounce → refresh_contents.
        if let Some(ref rx) = self.watch_receiver {
            while let Ok(path) = rx.try_recv() {
                if path.parent().map_or(false, |p| p == self.current_path) {
                    self.watch_pending_refresh = Some(std::time::Instant::now());
                }
            }
        }
    }

    /// Call each frame: fires the debounced file-watcher refresh after 500 ms of quiet.
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
    /// Returns `true` if all items were restored successfully.
    #[cfg(windows)]
    pub(crate) fn restore_from_recycle_bin(paths: &[PathBuf]) -> bool {
        let mut restored = 0usize;
        let total = paths.len();

        for original_path in paths {
            let mut drive = PathBuf::new();
            let mut comp_count = 0;
            for comp in original_path.components() {
                drive.push(comp);
                comp_count += 1;
                if comp_count >= 2 { break; }
            }
            if comp_count < 2 { continue; }
            let recycle_bin = drive.join("$Recycle.Bin");

            let sid_entries = match std::fs::read_dir(&recycle_bin) {
                Ok(e) => e,
                Err(_) => continue,
            };

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
                        if orig == *original_path {
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

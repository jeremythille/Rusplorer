/// File-system helpers — recursive copy/directory-size, tree children enumeration,
/// and background copy/move with progress.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Recursively calculate directory size, sending progressive updates via `tx`.
/// Returns `false` if cancelled.
pub fn calculate_dir_size_progressive(
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
            let _ = tx.send((root_path.to_path_buf(), *accumulated));
            return false;
        }
    };

    for entry in entries.filter_map(|e| e.ok()) {
        if cancel_token.load(Ordering::Relaxed) {
            return false;
        }
        while pause_token.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if cancel_token.load(Ordering::Relaxed) {
                return false;
            }
        }

        let entry_path = entry.path();
        if entry_path.is_dir() {
            calculate_dir_size_progressive(
                &entry_path,
                root_path,
                cancel_token,
                pause_token,
                tx,
                accumulated,
            );
        } else if let Ok(metadata) = entry.metadata() {
            let prev = *accumulated;
            *accumulated += metadata.len();
            // Throttle: send roughly every 64 KB of new data
            if *accumulated >> 16 != prev >> 16 {
                let _ = tx.send((root_path.to_path_buf(), *accumulated));
            }
        }
    }
    true
}

/// List immediate subdirectory children of `path`, sorted case-insensitively.
pub fn read_dir_children(path: &PathBuf) -> Vec<PathBuf> {
    std::fs::read_dir(path)
        .map(|entries| {
            let mut children: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.path())
                .collect();
            children.sort_by(|a, b| {
                let a_name = a.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                let b_name = b.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                a_name.cmp(&b_name)
            });
            children
        })
        .unwrap_or_default()
}

// ── Background copy/move with progress ───────────────────────────────────────

/// Describes a file conflict that requires the user to decide.
#[derive(Clone)]
pub struct ConflictInfo {
    /// Just the filename (for display).
    pub file_name: String,
    pub src_size: u64,
    pub src_modified: Option<std::time::SystemTime>,
    pub dst_size: u64,
    pub dst_modified: Option<std::time::SystemTime>,
}

/// User's response to a conflict prompt.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    /// Replace this one file.
    Overwrite,
    /// Keep the existing file, skip.
    Skip,
    /// Replace all conflicting files in this job without further prompts.
    OverwriteAll,
    /// Skip all conflicting files in this job without further prompts.
    SkipAll,
    /// Abort the whole job.
    Abort,
}

/// Thread-safe state for a background copy/move operation.
/// The worker thread updates atomics; the UI thread reads them every frame.
pub struct CopyJobState {
    /// Files completed so far.
    pub files_done: AtomicUsize,
    /// Total file count (set once before the copy loop starts).
    pub files_total: AtomicUsize,
    /// Bytes written so far across all files.
    pub bytes_copied: AtomicU64,
    /// Total bytes across all source files.
    pub total_bytes: AtomicU64,
    /// Name of the file currently being processed (for display).
    pub current_file: Mutex<String>,
    /// User requested pause.
    pub paused: AtomicBool,
    /// User requested abort.
    pub cancelled: AtomicBool,
    /// Worker finished (success, cancel, or error).
    pub done: AtomicBool,
    /// `true` = move, `false` = copy.
    pub is_move: bool,
    /// Short label for the destination (shown in the progress panel).
    pub dest_display: String,
    /// First error message, if any.
    pub error: Mutex<Option<String>>,
    /// Names of files/dirs successfully placed in the destination.
    pub pasted_names: Mutex<Vec<String>>,
    /// Whether the internal clipboard should be cleared when the job finishes
    /// (cut-paste operations).
    pub clear_clipboard: AtomicBool,
    // ── Conflict resolution (worker ↔ UI handshake) ───────────────────────
    /// Worker sets this when it encounters a conflicting destination file.
    /// UI reads it, shows a dialog, writes `conflict_answer`, clears this.
    pub conflict_query: Mutex<Option<ConflictInfo>>,
    /// UI writes a choice here after the user clicks a dialog button.
    pub conflict_answer: Mutex<Option<ConflictChoice>>,
    /// Files skipped because source and destination were identical.
    pub skipped_identical: Mutex<Vec<String>>,
    /// Set by worker after user chose "Overwrite all".
    pub overwrite_all: AtomicBool,
    /// Set by worker after user chose "Skip all".
    pub skip_all: AtomicBool,
}

impl CopyJobState {
    pub fn new(is_move: bool, dest_display: String) -> Self {
        Self {
            files_done: AtomicUsize::new(0),
            files_total: AtomicUsize::new(0),
            bytes_copied: AtomicU64::new(0),
            total_bytes: AtomicU64::new(0),
            current_file: Mutex::new(String::new()),
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            done: AtomicBool::new(false),
            is_move,
            dest_display,
            error: Mutex::new(None),
            pasted_names: Mutex::new(Vec::new()),
            clear_clipboard: AtomicBool::new(false),
            conflict_query: Mutex::new(None),
            conflict_answer: Mutex::new(None),
            skipped_identical: Mutex::new(Vec::new()),
            overwrite_all: AtomicBool::new(false),
            skip_all: AtomicBool::new(false),
        }
    }
}

/// Count total files and bytes across `sources` (recursively for directories).
fn tally_sources(sources: &[PathBuf]) -> (usize, u64) {
    let mut count = 0usize;
    let mut bytes = 0u64;
    for src in sources {
        if src.is_dir() {
            tally_dir(src, &mut count, &mut bytes);
        } else if let Ok(meta) = src.metadata() {
            count += 1;
            bytes += meta.len();
        }
    }
    (count, bytes)
}

fn tally_dir(dir: &Path, count: &mut usize, bytes: &mut u64) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                tally_dir(&path, count, bytes);
            } else if let Ok(meta) = path.metadata() {
                *count += 1;
                *bytes += meta.len();
            }
        }
    }
}

/// Copy a single file using chunked I/O (256 KB), updating `bytes_copied`.
/// Preserves the source file's timestamps on the destination.
fn copy_file_chunked(
    src: &Path,
    dst: &Path,
    state: &CopyJobState,
) -> std::io::Result<()> {
    use std::io::{Read, Write};

    let src_file = std::fs::File::open(src)?;
    let dst_file = std::fs::File::create(dst)?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, src_file);
    let mut writer = std::io::BufWriter::with_capacity(256 * 1024, dst_file);
    let mut buf = vec![0u8; 256 * 1024];

    loop {
        if state.cancelled.load(Ordering::Relaxed) {
            // Remove partial file on cancel
            drop(writer);
            let _ = std::fs::remove_file(dst);
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
        }
        while state.paused.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if state.cancelled.load(Ordering::Relaxed) {
                drop(reader);
                drop(writer);
                let _ = std::fs::remove_file(dst);
                return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
            }
        }
        let n = reader.read(&mut buf)?;
        if n == 0 { break; }
        writer.write_all(&buf[..n])?;
        state.bytes_copied.fetch_add(n as u64, Ordering::Relaxed);
    }
    writer.flush()?;
    drop(reader);
    drop(writer);

    // Preserve file timestamps
    preserve_file_times(src, dst);

    Ok(())
}

/// Copy modification/creation/access timestamps from `src` to `dst` (Windows).
#[cfg(windows)]
fn preserve_file_times(src: &Path, dst: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::fileapi::{CreateFileW, GetFileTime, SetFileTime, OPEN_EXISTING};
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::shared::minwindef::FILETIME;
    use winapi::um::winnt::{FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, GENERIC_READ};

    unsafe {
        let src_wide: Vec<u16> = src.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let src_h = CreateFileW(
            src_wide.as_ptr(), GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut(),
        );
        if src_h == INVALID_HANDLE_VALUE { return; }

        let mut ct = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut at = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut wt = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        if GetFileTime(src_h, &mut ct, &mut at, &mut wt) == 0 {
            CloseHandle(src_h);
            return;
        }
        CloseHandle(src_h);

        let dst_wide: Vec<u16> = dst.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let dst_h = CreateFileW(
            dst_wide.as_ptr(), FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut(),
        );
        if dst_h == INVALID_HANDLE_VALUE { return; }
        SetFileTime(dst_h, &ct, &at, &wt);
        CloseHandle(dst_h);
    }
}

#[cfg(not(windows))]
fn preserve_file_times(_src: &Path, _dst: &Path) {}

// ── Conflict resolution ───────────────────────────────────────────────────────

enum FileAction { Copy, Skip }

/// Check whether copying `src` to `dst` (which already exists) requires user
/// confirmation, and if so spin-wait until the UI responds.
///
/// Returns `FileAction::Skip` if the files are identical or the user chose to
/// skip/abort; `FileAction::Copy` if we should proceed (overwrite).
fn check_file_conflict(src: &Path, dst: &Path, state: &CopyJobState) -> FileAction {
    if !dst.exists() {
        return FileAction::Copy;
    }

    // Compare size and modification time; treat times within 2 s as equal
    // (FAT32 has 2-second mtime resolution).
    let src_meta = src.metadata().ok();
    let dst_meta = dst.metadata().ok();
    let sizes_match = match (&src_meta, &dst_meta) {
        (Some(s), Some(d)) => s.len() == d.len(),
        _ => false,
    };
    let mtimes_match = match (&src_meta, &dst_meta) {
        (Some(s), Some(d)) => {
            if let (Ok(st), Ok(dt)) = (s.modified(), d.modified()) {
                let diff = if st > dt {
                    st.duration_since(dt).unwrap_or_default()
                } else {
                    dt.duration_since(st).unwrap_or_default()
                };
                diff.as_secs() < 2
            } else {
                false
            }
        }
        _ => false,
    };
    if sizes_match && mtimes_match {
        // Files are identical — skip silently.
        let fname = src.file_name().unwrap_or_default().to_string_lossy().to_string();
        state.skipped_identical.lock().unwrap().push(fname);
        return FileAction::Skip;
    }

    // Apply a standing "overwrite all" / "skip all" choice from this job.
    if state.skip_all.load(Ordering::Relaxed) {
        return FileAction::Skip;
    }
    if state.overwrite_all.load(Ordering::Relaxed) {
        return FileAction::Copy;
    }

    // Need the user to decide — post the query and spin-wait.
    {
        let mut cq = state.conflict_query.lock().unwrap();
        *cq = Some(ConflictInfo {
            file_name: src.file_name().unwrap_or_default().to_string_lossy().to_string(),
            src_size:     src_meta.as_ref().map(|m| m.len()).unwrap_or(0),
            src_modified: src_meta.as_ref().and_then(|m| m.modified().ok()),
            dst_size:     dst_meta.as_ref().map(|m| m.len()).unwrap_or(0),
            dst_modified: dst_meta.as_ref().and_then(|m| m.modified().ok()),
        });
    }

    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if state.cancelled.load(Ordering::Relaxed) {
            *state.conflict_query.lock().unwrap() = None;
            return FileAction::Skip;
        }
        if let Some(choice) = state.conflict_answer.lock().unwrap().take() {
            *state.conflict_query.lock().unwrap() = None;
            match choice {
                ConflictChoice::Overwrite    => return FileAction::Copy,
                ConflictChoice::Skip         => return FileAction::Skip,
                ConflictChoice::OverwriteAll => {
                    state.overwrite_all.store(true, Ordering::Relaxed);
                    return FileAction::Copy;
                }
                ConflictChoice::SkipAll => {
                    state.skip_all.store(true, Ordering::Relaxed);
                    return FileAction::Skip;
                }
                ConflictChoice::Abort => {
                    state.cancelled.store(true, Ordering::Relaxed);
                    return FileAction::Skip;
                }
            }
        }
    }
}

/// Recursively copy a directory, file-by-file with chunked progress updates.
/// Respects conflict resolution via `check_file_conflict`.
fn copy_dir_chunked(
    src: &Path,
    dst: &Path,
    state: &CopyJobState,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        if state.cancelled.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
        }
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_chunked(&path, &dest_path, state)?;
        } else {
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            *state.current_file.lock().unwrap() = name;
            match check_file_conflict(&path, &dest_path, state) {
                FileAction::Skip => { state.files_done.fetch_add(1, Ordering::Relaxed); }
                FileAction::Copy => {
                    copy_file_chunked(&path, &dest_path, state)?;
                    state.files_done.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
    Ok(())
}

/// Main entry point: run a copy/move job on a background thread.
pub fn spawn_copy_job(sources: Vec<PathBuf>, dest: PathBuf, state: Arc<CopyJobState>) {
    std::thread::spawn(move || { run_copy_job(sources, dest, &state); });
}

fn run_copy_job(sources: Vec<PathBuf>, dest: PathBuf, state: &CopyJobState) {
    let (total_files, total_bytes) = tally_sources(&sources);
    state.files_total.store(total_files, Ordering::Relaxed);
    state.total_bytes.store(total_bytes, Ordering::Relaxed);

    for source in &sources {
        if state.cancelled.load(Ordering::Relaxed) { break; }

        let file_name = match source.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        let target = dest.join(&file_name);

        // Same-filesystem move: try rename first (fails if target exists on Windows).
        if state.is_move && source != &target {
            if std::fs::rename(source, &target).is_ok() {
                let (fi, bi) = if source.is_dir() {
                    let mut c = 0usize; let mut b = 0u64;
                    tally_dir(source, &mut c, &mut b); (c, b)
                } else { (1, source.metadata().map(|m| m.len()).unwrap_or(0)) };
                state.files_done.fetch_add(fi, Ordering::Relaxed);
                state.bytes_copied.fetch_add(bi, Ordering::Relaxed);
                state.pasted_names.lock().unwrap()
                    .push(target.file_name().unwrap_or_default().to_string_lossy().to_string());
                continue;
            }
            // rename failed (cross-device or target exists) — fall through
        }

        if source.is_dir() {
            // Directories are merged; per-file conflicts handled inside copy_dir_chunked.
            match copy_dir_chunked(source, &target, state) {
                Ok(()) => {
                    if state.is_move { let _ = std::fs::remove_dir_all(source); }
                    state.pasted_names.lock().unwrap()
                        .push(target.file_name().unwrap_or_default().to_string_lossy().to_string());
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => break,
                Err(e) => {
                    *state.error.lock().unwrap() = Some(format!("{}: {}", file_name.to_string_lossy(), e));
                    break;
                }
            }
        } else {
            *state.current_file.lock().unwrap() = file_name.to_string_lossy().to_string();
            match check_file_conflict(source, &target, state) {
                FileAction::Skip => {
                    state.files_done.fetch_add(1, Ordering::Relaxed);
                    if state.cancelled.load(Ordering::Relaxed) { break; }
                }
                FileAction::Copy => {
                    match copy_file_chunked(source, &target, state) {
                        Ok(()) => {
                            state.files_done.fetch_add(1, Ordering::Relaxed);
                            if state.is_move { let _ = std::fs::remove_file(source); }
                            state.pasted_names.lock().unwrap()
                                .push(target.file_name().unwrap_or_default().to_string_lossy().to_string());
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => break,
                        Err(e) => {
                            *state.error.lock().unwrap() = Some(format!("{}: {}", file_name.to_string_lossy(), e));
                            break;
                        }
                    }
                }
            }
        }
    }
    state.done.store(true, Ordering::SeqCst);
}
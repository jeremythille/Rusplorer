/// File-system helpers — recursive copy/directory-size, tree children enumeration.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Recursively copy `src` directory into `dst`.
pub fn copy_dir_recursive(src: &PathBuf, dst: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
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

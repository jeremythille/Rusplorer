use super::RusplorerApp;
#[cfg(windows)]
use crate::shortcuts::create_lnk_shortcut;
use arboard::Clipboard;
use eframe::egui;
use std::collections::HashSet;
#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;

fn extract_archive_with_7zip(archive_path: &Path, destination: &Path) -> Result<(), String> {
    let archive = archive_path.to_string_lossy();
    let destination = destination.to_string_lossy();
    let output_arg = format!("-o{}", destination);
    let args = ["x", archive.as_ref(), output_arg.as_str()];

    let mut child = std::process::Command::new("C:\\Program Files\\7-Zip\\7z.exe")
        .args(args)
        .spawn()
        .or_else(|_| std::process::Command::new("7z.exe").args(args).spawn())
        .map_err(|error| format!("Could not start 7-Zip: {}", error))?;
    let status = child
        .wait()
        .map_err(|error| format!("Could not wait for 7-Zip: {}", error))?;
    if !status.success() {
        return Err(format!("7-Zip exited with status {}", status));
    }
    Ok(())
}

fn extract_archives(
    archive_paths: &[PathBuf],
    destination_root: &Path,
    separate_folders: bool,
) -> Result<(), String> {
    let mut jobs = Vec::with_capacity(archive_paths.len());
    let mut failures = Vec::new();

    for archive_path in archive_paths {
        let destination = if separate_folders {
            let stem = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy();
            let destination = destination_root.join(stem.as_ref());
            if let Err(error) = std::fs::create_dir_all(&destination) {
                failures.push(format!("{}: {}", archive_path.display(), error));
                continue;
            }
            destination
        } else {
            destination_root.to_path_buf()
        };
        jobs.push((archive_path.clone(), destination));
    }

    let destinations_are_unique = {
        let mut destinations = HashSet::with_capacity(jobs.len());
        jobs.iter().all(|(_, destination)| {
            destinations.insert(destination.to_string_lossy().to_lowercase())
        })
    };

    // Avoid parallel writes when archives share a destination directory.
    let parallelism = if separate_folders && destinations_are_unique {
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .min(jobs.len())
    } else {
        1
    };

    if parallelism > 1 {
        let mut results = Vec::with_capacity(jobs.len());
        std::thread::scope(|scope| {
            for batch in jobs.chunks(parallelism) {
                let handles: Vec<_> = batch
                    .iter()
                    .map(|(archive_path, destination)| {
                        scope.spawn(move || {
                            extract_archive_with_7zip(archive_path, destination)
                        })
                    })
                    .collect();
                for ((archive_path, _), handle) in batch.iter().zip(handles) {
                    results.push((archive_path.clone(), handle.join()));
                }
            }
        });

        for (archive_path, result) in results {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failures.push(format!("{}: {}", archive_path.display(), error));
                }
                Err(_) => failures.push(format!(
                    "{}: extraction worker stopped unexpectedly",
                    archive_path.display()
                )),
            }
        }
    } else {
        for (archive_path, destination) in &jobs {
            if let Err(error) = extract_archive_with_7zip(archive_path, destination) {
                failures.push(format!("{}: {}", archive_path.display(), error));
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        let failure_count = failures.len();
        let failure_summary = failures
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let remaining = failure_count.saturating_sub(3);
        let suffix = if remaining == 0 {
            String::new()
        } else {
            format!("; and {} more", remaining)
        };
        Err(format!(
            "{} of {} archive(s) failed: {}{}",
            failure_count,
            archive_paths.len(),
            failure_summary,
            suffix
        ))
    }
}

impl RusplorerApp {
    fn strip_one_duplicate_extension(name: &str) -> String {
        let Some(last_dot) = name.rfind('.') else {
            return name.to_string();
        };
        if last_dot + 1 >= name.len() {
            return name.to_string();
        }

        let ext = &name[last_dot + 1..];
        let before = &name[..last_dot];
        let Some(prev_dot) = before.rfind('.') else {
            return name.to_string();
        };
        if prev_dot + 1 >= before.len() {
            return name.to_string();
        }

        let prev_ext = &before[prev_dot + 1..];
        if prev_ext.eq_ignore_ascii_case(ext) {
            before.to_string()
        } else {
            name.to_string()
        }
    }

    pub(crate) fn render_dialogs(&mut self, ctx: &egui::Context) {

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
                        self.start_copy_job(files, dest, true, false, false);
                        self.show_drop_menu = false;
                        self.dragged_files.clear();
                    }
                    if ui.button("Copy here").clicked() {
                        let files = self.dragged_files.clone();
                        let dest = self.current_path.clone();
                        self.start_copy_job(files, dest, false, false, false);
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
                    self.start_copy_job(sources, dest, true, false, false);
                    self.selected_entries.clear();
                    self.dnd_right_drop_menu = None;
                }
                Some("copy") => {
                    self.start_copy_job(sources, dest, false, false, false);
                    self.dnd_right_drop_menu = None;
                }
                #[cfg(windows)]
                Some("shortcut") => {
                    let mut failed = Vec::new();
                    for s in &sources {
                        if let Err(e) = create_lnk_shortcut(s, &dest) {
                            failed.push(format!("{}: {}", s.display(), e));
                        }
                    }
                    if !failed.is_empty() {
                        let msg = if failed.len() == 1 {
                            format!("Create shortcut failed: {}", failed[0])
                        } else {
                            format!(
                                "Create shortcut failed for {} item(s). First error: {}",
                                failed.len(),
                                failed[0]
                            )
                        };
                        self.delete_feedback_msg = Some(msg);
                        self.delete_feedback_until = None;
                        self.delete_feedback_is_error = true;
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
            if let Some(entry) = self.context_menu_entry.clone() {
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
                let mut selected_archives: Vec<PathBuf> = self
                    .context_menu_selection
                    .iter()
                    .filter(|path| Self::is_archive(path) && path.is_file())
                    .cloned()
                    .collect();
                if selected_archives.is_empty()
                    && !entry.is_dir
                    && Self::is_archive(&full_path)
                    && full_path.is_file()
                {
                    selected_archives.push(full_path.clone());
                }
                selected_archives.sort();
                let has_multiple_archives = selected_archives.len() > 1;
                let extract_each_label = "Extract each archive to a separate folder";
                let extract_all_label = "Extract all archives here";
                let mut labels: Vec<&str> = vec![
                    "Add to archive",
                    "📋 Copy full path",
                    "Rename",
                    "Delete",
                    "Properties",
                ];
                if entry.is_dir {
                    labels.push("Open in a new tab");
                }
                if entry.is_dir || Self::is_code_file(&full_path) {
                    labels.push("Open with VS Code");
                }
                let is_ps1 = full_path.extension()
                    .map(|e| e.to_ascii_lowercase() == "ps1")
                    .unwrap_or(false);
                if is_ps1 {
                    labels.push("▶ Run in PowerShell");
                }
                let is_font = !entry.is_dir && full_path.extension()
                    .map(|e| { let e = e.to_ascii_lowercase(); e == "ttf" || e == "otf" || e == "ttc" })
                    .unwrap_or(false);
                if is_font {
                    labels.push("Install font");
                }
                if !entry.is_dir {
                    labels.push("Open with\u{2026}");
                    labels.push("Unlock\u{2026}");
                }
                if has_multiple_archives {
                    labels.push(extract_each_label);
                    labels.push(extract_all_label);
                } else if !entry.is_dir && Self::is_archive(&full_path) {
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

                let mut pending_delete: Vec<PathBuf> = Vec::new();

                let area_resp = egui::Area::new(egui::Id::new("ctx_menu"))
                    .fixed_pos(egui::pos2(adj_x, adj_y))
                    .order(egui::Order::Foreground)
                    .interactable(true)
                    .show(ctx, |ui| {
                        egui::Frame {
                            fill: crate::theme::SURFACE_RAISED,
                            stroke: egui::Stroke::new(1.0, crate::theme::BORDER_HOVER),
                            inner_margin: egui::Margin::same(4.0),
                            rounding: egui::Rounding::same(4.0),
                            ..Default::default()
                        }
                        .show(ui, |ui| {
                            ui.set_max_width(menu_w);
                            ui.set_min_width(menu_w);
                            ui.style_mut().spacing.button_padding = egui::vec2(4.0, 2.0);

                            // Open with… (all files)
                            if !entry.is_dir
                                && ui.add_sized([menu_w, 0.0], egui::Button::new("Open with\u{2026}")).clicked()
                            {
                                #[cfg(windows)]
                                {
                                    use winapi::um::shellapi::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_INVOKEIDLIST};
                                    use winapi::um::winuser::SW_SHOW;

                                    // Use the native "openas" verb to reliably launch
                                    // Windows' "Open with" picker.
                                    let verb: Vec<u16> = OsStr::new("openas")
                                        .encode_wide().chain(std::iter::once(0)).collect();
                                    let file: Vec<u16> = OsStr::new(full_path.to_str().unwrap_or(""))
                                        .encode_wide().chain(std::iter::once(0)).collect();
                                    let ok = unsafe {
                                        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
                                        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
                                        info.fMask = SEE_MASK_INVOKEIDLIST;
                                        info.lpVerb = verb.as_ptr();
                                        info.lpFile = file.as_ptr();
                                        info.nShow = SW_SHOW as i32;
                                        ShellExecuteExW(&mut info) != 0
                                    };
                                    if !ok {
                                        self.delete_feedback_msg = Some(
                                            "Open with failed: Windows could not open the app picker.".to_string()
                                        );
                                        self.delete_feedback_until = None;
                                        self.delete_feedback_is_error = true;
                                    }
                                }
                                self.show_context_menu = false;
                                self.context_menu_tree_path = None;
                                self.context_menu_tree_highlight = None;
                            }

                            // Open with VS Code
                            if entry.is_dir
                                && ui.add_sized([menu_w, 0.0], egui::Button::new("Open in a new tab")).clicked()
                            {
                                self.new_tab(Some(full_path.clone()));
                                self.tab_scroll_to_active = true;
                                self.show_context_menu = false;
                                self.context_menu_tree_path = None;
                                self.context_menu_tree_highlight = None;
                            }

                            // Open with VS Code
                            if (entry.is_dir || Self::is_code_file(&full_path))
                                && ui.add_sized([menu_w, 0.0], egui::Button::new("Open with VS Code")).clicked()
                            {
                                #[cfg(windows)]
                                {
                                    use std::os::windows::process::CommandExt;
                                    use std::process::Stdio;

                                    let _ = std::process::Command::new("cmd")
                                        .creation_flags(winapi::um::winbase::CREATE_NO_WINDOW)
                                        .args(["/C", "code", full_path.to_string_lossy().as_ref()])
                                        .stdin(Stdio::null())
                                        .stdout(Stdio::null())
                                        .stderr(Stdio::null())
                                        .spawn();
                                }
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

                            // Install font (.ttf / .otf / .ttc)
                            if is_font
                                && ui.add_sized([menu_w, 0.0], egui::Button::new("Install font")).clicked()
                            {
                                #[cfg(windows)]
                                {
                                    use winapi::um::shellapi::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_INVOKEIDLIST};
                                    use winapi::um::winuser::SW_SHOW;
                                    let verb: Vec<u16> = OsStr::new("install")
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
                                self.context_menu_tree_path = None;
                                self.context_menu_tree_highlight = None;
                            }

                            // Unlock (files only) — find & kill locking processes
                            if !entry.is_dir
                                && ui.add_sized([menu_w, 0.0], egui::Button::new("Unlock\u{2026}")).clicked()
                            {
                                self.unlock_dialog_path = Some(full_path.clone());
                                self.unlock_locking_processes = crate::fs_ops::find_locking_processes(&full_path);
                                self.show_unlock_dialog = true;
                                self.show_context_menu = false;
                                self.context_menu_tree_path = None;
                                self.context_menu_tree_highlight = None;
                            }

                            if has_multiple_archives {
                                if ui
                                    .add_sized(
                                        [menu_w, 0.0],
                                        egui::Button::new(extract_each_label),
                                    )
                                    .clicked()
                                {
                                    self.extract_archive_paths = selected_archives.clone();
                                    self.extract_to_separate_folders = true;
                                    self.show_extract_dialog = true;
                                    self.show_context_menu = false;
                                    self.context_menu_tree_highlight = None;
                                }
                                if ui
                                    .add_sized(
                                        [menu_w, 0.0],
                                        egui::Button::new(extract_all_label),
                                    )
                                    .clicked()
                                {
                                    self.extract_archive_paths = selected_archives.clone();
                                    self.extract_to_separate_folders = false;
                                    self.show_extract_dialog = true;
                                    self.show_context_menu = false;
                                    self.context_menu_tree_highlight = None;
                                }
                            } else if !entry.is_dir
                                && Self::is_archive(&full_path)
                                && ui.add_sized([menu_w, 0.0], egui::Button::new(extract_label.as_str())).clicked()
                            {
                                self.extract_archive_paths = vec![full_path.clone()];
                                self.extract_to_separate_folders = true;
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
                                self.rename_focus_pending = true;
                                self.show_context_menu = false;
                                self.context_menu_tree_highlight = None;
                            }

                            // Delete → Recycle Bin
                            if !entry.name.starts_with("[..]")
                                && ui.add_sized([menu_w, 0.0],
                                    egui::Button::new(egui::RichText::new("Delete")
                                        .color(egui::Color32::from_rgb(220, 50, 50)))
                                ).clicked()
                            {
                                pending_delete = if !self.context_menu_selection.is_empty() {
                                    self.context_menu_selection.clone()
                                } else {
                                    vec![full_path.clone()]
                                };
                                self.show_context_menu = false;
                                self.context_menu_tree_highlight = None;
                            }
                            if ui.add_sized([menu_w, 0.0], egui::Button::new("Properties")).clicked() {
                                let targets = if !self.context_menu_selection.is_empty() {
                                    self.context_menu_selection.clone()
                                } else {
                                    vec![full_path.clone()]
                                };

                                if targets.len() > 1 {
                                    self.multi_properties_selection = targets;
                                    self.show_multi_properties_dialog = true;
                                } else {
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
                                }
                                self.show_context_menu = false;
                                self.context_menu_tree_highlight = None;
                            }

                        });
                    });

                // Store actual rendered size for next-frame clamping
                self.context_menu_size = area_resp.response.rect.size();

                // Process pending delete (set inside closure; executed out here to avoid &mut self conflict)
                if !pending_delete.is_empty() {
                    self.start_delete_job(pending_delete);
                }
            }

            // Close context menu if clicked elsewhere
            if ctx.input(|i| i.pointer.primary_clicked() || i.key_pressed(egui::Key::Escape)) {
                self.show_context_menu = false;
                // Don't wipe context_menu_tree_path here when the rename dialog was just
                // opened by this very click — the rename logic needs it to build old_path.
                if !self.show_rename_dialog {
                    self.context_menu_tree_path = None;
                    self.context_menu_tree_highlight = None;
                    self.context_menu_selection.clear();
                }
            }
        }

        // ── Background context menu (right-click on empty space) ─────────────
        if self.show_bg_context_menu {
            let undo_label: Option<String> = self.undo_stack.last()
                .map(|a| format!("↩  Undo {}", a.label()));
            let can_undo = undo_label.is_some();
            // Pre-compute required width from button labels
            let btn_padding = 8.0 + 8.0;
            let font_id = egui::TextStyle::Button.resolve(&ctx.style());
            let mut bg_labels = vec!["📁  New folder", "📄  New text file", "↻  Refresh"];
            let undo_label_str;  // lifetime anchor
            if let Some(ref lbl) = undo_label {
                undo_label_str = lbl.as_str();
                bg_labels.push(undo_label_str);
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
                        fill: crate::theme::SURFACE_RAISED,
                        stroke: egui::Stroke::new(1.0, crate::theme::BORDER_HOVER),
                        inner_margin: egui::Margin::same(4.0),
                        rounding: egui::Rounding::same(4.0),
                        ..Default::default()
                    }
                    .show(ui, |ui| {
                        ui.set_max_width(menu_w);
                        ui.set_min_width(menu_w);
                        ui.style_mut().spacing.button_padding = egui::vec2(4.0, 2.0);

                        if ui.add_sized([menu_w, 0.0], egui::Button::new("📁  New folder")).clicked() {
                            // Open rename dialog directly so the user types the name
                            self.new_folder_mode = true;
                            self.rename_buffer = String::new();
                            self.rename_ext = String::new();
                            self.rename_show_ext = false;
                            self.context_menu_entry = Some(crate::types::FileEntry {
                                name: String::new(),
                                is_dir: true,
                                size: 0,
                                modified: None,
                            });
                            self.show_rename_dialog = true;
                            self.rename_focus_pending = true;
                            self.show_bg_context_menu = false;
                        }
                        if ui.add_sized([menu_w, 0.0], egui::Button::new("📄  New text file")).clicked() {
                            self.new_item_is_dir = false;
                            self.new_item_name_buffer = "New file.txt".to_string();
                            self.show_new_item_dialog = true;
                            self.new_item_focus_pending = true;
                            self.show_bg_context_menu = false;
                        }
                        // Thin separator line
                        let (line_rect, _) = ui.allocate_exact_size(egui::vec2(menu_w, 1.0), egui::Sense::hover());
                        ui.painter().rect_filled(line_rect, 0.0, egui::Color32::from_gray(160));
                        ui.add_space(2.0);
                        if ui.add_sized([menu_w, 0.0], egui::Button::new("↻  Refresh")).clicked() {
                            self.refresh_contents();
                            self.show_bg_context_menu = false;
                        }
                        if can_undo {
                            // Separator
                            let (line_rect2, _) = ui.allocate_exact_size(egui::vec2(menu_w, 1.0), egui::Sense::hover());
                            ui.painter().rect_filled(line_rect2, 0.0, egui::Color32::from_gray(160));
                            ui.add_space(2.0);
                            let lbl = undo_label.as_deref().unwrap_or("↩  Undo");
                            if ui.add_sized([menu_w, 0.0], egui::Button::new(lbl)).clicked() {
                                self.show_undo_dialog = true;
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
            let archive_paths = self.extract_archive_paths.clone();
            let separate_folders = self.extract_to_separate_folders;
            let destination_root = self.current_path.clone();
            let (done_tx, done_rx) = channel();

            std::thread::spawn(move || {
                let _ = done_tx.send(extract_archives(
                    &archive_paths,
                    &destination_root,
                    separate_folders,
                ));
            });

            self.extract_done_receiver = Some(done_rx);
        }

        // Rename dialog
        // ── Modal backdrop ────────────────────────────────────────────────
        if self.show_rename_dialog || self.show_new_item_dialog
            || self.show_archive_dialog
            || self.show_save_session_dialog || self.show_unlock_dialog
            || self.show_undo_dialog
        {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::PanelResizeLine,
                egui::Id::new("modal_backdrop"),
            ));
            painter.rect_filled(
                ctx.screen_rect(),
                0.0,
                egui::Color32::from_black_alpha(110),
            );
        }

        // ── New folder / New file dialog ──────────────────────────────────
        if self.show_new_item_dialog {
            let is_clipboard_image = self.pending_clipboard_image.is_some();
            let title = if is_clipboard_image {
                "Save clipboard image"
            } else if self.new_item_is_dir {
                "New folder"
            } else {
                "New text file"
            };
            let mut close_dialog = false;
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(if is_clipboard_image { "File name:" } else { "Name:" });
                    let resp = ui.text_edit_singleline(&mut self.new_item_name_buffer);

                    // Auto-focus only once when the dialog opens.
                    if self.new_item_focus_pending {
                        resp.request_focus();
                        self.new_item_focus_pending = false;
                    }

                    let confirmed = resp.lost_focus();

                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() || confirmed {
                            let name = self.new_item_name_buffer.trim().to_string();
                            if !name.is_empty() {
                                let target = self.current_path.join(&name);
                                let mut created = false;
                                if self.new_item_is_dir {
                                    match std::fs::create_dir(&target) {
                                        Ok(()) => {
                                            created = true;
                                            // Refresh tree cache so the new folder appears immediately.
                                            let dir = self.current_path.clone();
                                            self.refresh_tree_children_for_dir(&dir);
                                        }
                                        Err(e) => {
                                            self.delete_feedback_msg = Some(format!(
                                                "Create folder failed: {}",
                                                e
                                            ));
                                            self.delete_feedback_until = None;
                                            self.delete_feedback_is_error = true;
                                        }
                                    }
                                } else if let Some(image) = self.pending_clipboard_image.as_ref() {
                                    match image.save(&target) {
                                        Ok(()) => created = true,
                                        Err(e) => {
                                            self.delete_feedback_msg = Some(format!(
                                                "Save clipboard image failed: {}",
                                                e
                                            ));
                                            self.delete_feedback_until = None;
                                            self.delete_feedback_is_error = true;
                                        }
                                    }
                                } else {
                                    match std::fs::File::create(&target) {
                                        Ok(_) => {
                                            created = true;
                                        }
                                        Err(e) => {
                                            self.delete_feedback_msg = Some(format!(
                                                "Create file failed: {}",
                                                e
                                            ));
                                            self.delete_feedback_until = None;
                                            self.delete_feedback_is_error = true;
                                        }
                                    }
                                }
                                if created {
                                    self.refresh_contents();
                                    // Select the newly created item
                                    self.selected_entries.clear();
                                    self.selected_entries.insert(name);
                                    close_dialog = true;
                                }
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            close_dialog = true;
                        }
                    });
                });
            if close_dialog {
                self.show_new_item_dialog = false;
                self.pending_clipboard_image = None;
            }
        }

        if self.show_rename_dialog {
            if let Some(entry) = self.context_menu_entry.clone() {
                let entry_name = entry.name.clone();
                let has_ext = !self.rename_ext.is_empty() && !self.new_folder_mode;
                let mut close_dialog = false;
                let mut do_rename = false;
                let window_title = if self.new_folder_mode { "New folder" } else { "Rename" };
                egui::Window::new(window_title)
                    .collapsible(false)
                    .resizable(false)
                    .fixed_size(egui::vec2(320.0, 0.0))
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        // Consume ESC before the TextEdit can swallow it
                        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                            close_dialog = true;
                        }

                        if self.new_folder_mode {
                            ui.label("Folder name:");
                        } else {
                            ui.label(format!("Renaming: {}", &entry_name));
                        }
                        ui.add_space(4.0);

                        // Text field + clear button on same row.
                        ui.horizontal(|ui| {
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut self.rename_buffer)
                                    .desired_width(268.0)
                            );
                            if self.rename_focus_pending {
                                response.request_focus();
                                self.rename_focus_pending = false;
                            }
                            if ui.add_sized([24.0, 0.0], egui::Button::new("✕")).clicked() {
                                self.rename_buffer.clear();
                            }
                        });

                        // Confirm rename only on Enter explicitly.
                        if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !close_dialog {
                            do_rename = true;
                        }

                        // Extension toggle (only shown for files that have an extension)
                        if has_ext {
                            let prev = self.rename_show_ext;
                            ui.checkbox(&mut self.rename_show_ext,
                                format!("Show extension ({})", self.rename_ext));
                            if self.rename_show_ext != prev {
                                if self.rename_show_ext {
                                    let ext = self.rename_ext.clone();
                                    self.rename_buffer.push_str(&ext);
                                } else {
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
                            if ui.button("OK").clicked() {
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
                        if self.new_folder_mode {
                            let target = self.current_path.join(&stem);
                            match std::fs::create_dir(&target) {
                                Ok(()) => {
                                    let dir = self.current_path.clone();
                                    self.refresh_tree_children_for_dir(&dir);
                                    self.refresh_contents();
                                    self.selected_entries.clear();
                                    self.selected_entries.insert(stem);
                                }
                                Err(e) => {
                                    self.delete_feedback_msg = Some(format!(
                                        "Create folder failed: {}",
                                        e
                                    ));
                                    self.delete_feedback_until = None;
                                    self.delete_feedback_is_error = true;
                                    do_rename = false;
                                }
                            }
                        } else {
                            let mut final_name = if self.rename_show_ext || !has_ext {
                                stem
                            } else {
                                format!("{}{}", stem, self.rename_ext)
                            };
                            final_name = Self::strip_one_duplicate_extension(&final_name);
                            // Use context_menu_tree_path when renaming from the tree panel.
                            let old_path = self.context_menu_tree_path
                                .clone()
                                .unwrap_or_else(|| self.current_path.join(&entry_name));
                            let parent = old_path.parent()
                                .map(|p| p.to_path_buf())
                                .unwrap_or_else(|| self.current_path.clone());
                            let new_path = parent.join(&final_name);
                            match std::fs::rename(&old_path, &new_path) {
                                Ok(()) => {
                                    // Record undo action before cache/UI updates.
                                    self.undo_stack.push(crate::types::UndoAction::Rename {
                                        old_path: old_path.clone(),
                                        new_path: new_path.clone(),
                                    });
                                    // Refresh tree cache: update parent's children list (rename
                                    // changed its contents), and drop the now-gone old path entry.
                                    self.refresh_tree_children_for_dir(&parent);
                                    self.tree_children_cache.remove(&old_path);
                                    self.context_menu_tree_path = None;
                                    self.refresh_contents();

                                    // Keep renamed item selected when it belongs to the current view.
                                    if parent == self.current_path {
                                        self.selected_entries.clear();
                                        self.selected_entries.insert(final_name);
                                    }
                                }
                                Err(e) => {
                                    // Surface the OS error message so the user knows why
                                    // the rename didn't happen (e.g. file open in Excel).
                                    self.delete_feedback_msg = Some(
                                        format!("Rename failed: {}", e)
                                    );
                                    self.delete_feedback_until = None;
                                    self.delete_feedback_is_error = true;
                                    // Keep dialog open so the user can try a different name
                                    // or close it manually.
                                    do_rename = false;
                                }
                            }

                        }
                    }
                    self.new_folder_mode = false;
                    if do_rename {
                        self.show_rename_dialog = false;
                    }
                } else if close_dialog {
                    self.new_folder_mode = false;
                    self.show_rename_dialog = false;
                    self.context_menu_tree_path = None;
                }
            }
        }

        if self.show_multi_properties_dialog {
            let mut close_dialog = false;
            let mut total_size = 0u64;
            let mut files_count = 0usize;
            let mut dirs_count = 0usize;
            let mut pending_count = 0usize;

            for path in &self.multi_properties_selection {
                if path.is_dir() {
                    dirs_count += 1;
                } else {
                    files_count += 1;
                }
                if let Some(sz) = self.file_sizes.get(path) {
                    total_size = total_size.saturating_add(*sz);
                } else {
                    pending_count += 1;
                }
            }

            egui::Window::new("Properties")
                .collapsible(false)
                .resizable(false)
                .fixed_size(egui::vec2(420.0, 0.0))
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Selection: {} items",
                        self.multi_properties_selection.len()
                    ));
                    ui.label(format!("Files: {}", files_count));
                    ui.label(format!("Folders: {}", dirs_count));
                    ui.label(format!(
                        "Total size: {}",
                        Self::format_file_size(total_size)
                    ));
                    if pending_count > 0 {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} item(s) still computing size",
                                pending_count
                            ))
                            .color(egui::Color32::GRAY),
                        );
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Close").clicked() {
                            close_dialog = true;
                        }
                    });
                });

            if close_dialog
                || ctx.input(|i| i.key_pressed(egui::Key::Escape))
            {
                self.show_multi_properties_dialog = false;
                self.multi_properties_selection.clear();
            }
        }

        // Extraction status strip — shown at bottom while running, disappears when done
        if self.show_extract_dialog {
            let archive_label = if self.extract_archive_paths.len() == 1 {
                self.extract_archive_paths[0]
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            } else {
                format!("{} archives", self.extract_archive_paths.len())
            };

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
                            format!("Extracting {}…", archive_label))
                            .color(egui::Color32::WHITE).size(12.0));
                    });
                });
        }

        // Delete feedback strip (in-progress and success/error).
        if let Some(msg) = self.delete_feedback_msg.clone() {
            let in_progress = self.delete_done_receiver.is_some();
            let still_visible = match self.delete_feedback_until {
                Some(until) => std::time::Instant::now() <= until,
                None => true,
            };

            if still_visible {
                let fill = if self.delete_feedback_is_error {
                    egui::Color32::from_rgb(140, 40, 40)
                } else if in_progress {
                    egui::Color32::from_rgb(40, 80, 140)
                } else {
                    egui::Color32::from_rgb(40, 110, 60)
                };

                let toast_w = ctx.screen_rect().width().min(520.0).max(260.0);
                let mut message_chars = msg.chars();
                let mut display_msg = message_chars.by_ref().take(180).collect::<String>();
                if message_chars.next().is_some() {
                    display_msg.push('…');
                }
                let text_width = (toast_w - 110.0).max(100.0);

                egui::Area::new(egui::Id::new("delete_feedback_strip_area"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -32.0))
                    .interactable(true)
                    .show(ctx, |ui| {
                        let mut dismiss = false;
                        ui.set_width(toast_w);

                        egui::Frame::none()
                            .fill(fill)
                            .inner_margin(egui::Margin::symmetric(6.0, 4.0))
                            .rounding(egui::Rounding::same(6.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    if in_progress {
                                        ui.spinner();
                                    }
                                    ui.add_sized(
                                        [text_width, 0.0],
                                        egui::Label::new(
                                            egui::RichText::new(display_msg)
                                                .color(egui::Color32::WHITE)
                                                .size(12.0),
                                        )
                                        .wrap(true)
                                        .sense(egui::Sense::hover()),
                                    );
                                    if ui.small_button("Dismiss").clicked() {
                                        dismiss = true;
                                    }
                                });
                            });

                        if dismiss {
                            self.delete_feedback_msg = None;
                            self.delete_feedback_until = None;
                            if in_progress {
                                self.delete_feedback_suppressed = true;
                            }
                        }
                    });
                ctx.request_repaint();
            } else {
                self.delete_feedback_msg = None;
                self.delete_feedback_until = None;
            }
        }

        // ── Undo history dialog ───────────────────────────────────────────
        if self.show_undo_dialog {
            let mut close_dialog = false;
            // Collect up to 5 most-recent actions (most recent first) by index
            let stack_len = self.undo_stack.len();
            let show_count = stack_len.min(5);
            // Indices into undo_stack, from most-recent (stack_len-1) downward
            let indices: Vec<usize> = (0..show_count)
                .map(|i| stack_len - 1 - i)
                .collect();
            // Build labels before borrowing self mutably
            let descriptions: Vec<String> = indices.iter()
                .map(|&i| self.undo_stack[i].description())
                .collect();

            let mut apply_index: Option<usize> = None; // index into undo_stack to apply

            egui::Window::new("Undo history")
                .collapsible(false)
                .resizable(false)
                .min_width(340.0)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                        close_dialog = true;
                    }

                    ui.label("Click an action to undo it:");
                    ui.add_space(6.0);

                    for (pos, (&stack_idx, desc)) in indices.iter().zip(descriptions.iter()).enumerate() {
                        let label = format!("↩  {}", desc);
                        let btn_color = if pos == 0 {
                            egui::Color32::from_rgb(60, 130, 210)
                        } else {
                            egui::Color32::from_rgb(80, 110, 160)
                        };
                        let resp = ui.add_sized(
                            [ui.available_width(), 0.0],
                            egui::Button::new(
                                egui::RichText::new(&label).color(egui::Color32::WHITE)
                            ).fill(btn_color),
                        );
                        if resp.clicked() {
                            apply_index = Some(stack_idx);
                            close_dialog = true;
                        }
                        ui.add_space(2.0);
                    }

                    if stack_len > 5 {
                        ui.add_space(2.0);
                        ui.label(egui::RichText::new(format!("…and {} more", stack_len - 5))
                            .color(egui::Color32::GRAY).italics());
                    }

                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(4.0);
                    if ui.button("Cancel").clicked() {
                        close_dialog = true;
                    }
                });

            if let Some(idx) = apply_index {
                let action = self.undo_stack.remove(idx);
                self.apply_undo(action);
            }
            if close_dialog {
                self.show_undo_dialog = false;
            }
        }

        // ── Unlock file dialog ────────────────────────────────────────────
        if self.show_unlock_dialog {
            let path_str = self.unlock_dialog_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            let mut close_dialog = false;
            let mut kill_and_unlock = false;

            egui::Window::new("\u{1F513} Unlock file")
                .collapsible(false)
                .resizable(false)
                .min_width(300.0)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(format!("File: {}", path_str));
                    ui.add_space(6.0);

                    if self.unlock_locking_processes.is_empty() {
                        ui.label("\u{2705} This file is not currently locked.");
                        ui.add_space(4.0);
                        if ui.button("Close").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Escape))
                        {
                            close_dialog = true;
                        }
                    } else {
                        ui.label("Locked by:");
                        for (pid, name) in &self.unlock_locking_processes {
                            ui.label(format!("  \u{2022} {} (PID {})", name, pid));
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button(egui::RichText::new("Kill all & Unlock")
                                .color(egui::Color32::from_rgb(220, 50, 50))).clicked()
                            {
                                kill_and_unlock = true;
                                close_dialog = true;
                            }
                            if ui.button("Cancel").clicked()
                                || ui.input(|i| i.key_pressed(egui::Key::Escape))
                            {
                                close_dialog = true;
                            }
                        });
                    }
                });

            if kill_and_unlock {
                #[cfg(windows)]
                {
                    use winapi::um::processthreadsapi::{OpenProcess, TerminateProcess};
                    use winapi::um::handleapi::CloseHandle;
                    use winapi::um::winnt::PROCESS_TERMINATE;
                    for (pid, _) in &self.unlock_locking_processes {
                        unsafe {
                            let handle = OpenProcess(PROCESS_TERMINATE, 0, *pid);
                            if !handle.is_null() {
                                TerminateProcess(handle, 1);
                                CloseHandle(handle);
                            }
                        }
                    }
                }
            }

            if close_dialog {
                self.show_unlock_dialog = false;
                self.unlock_dialog_path = None;
                self.unlock_locking_processes.clear();
            }
        }
    }
}

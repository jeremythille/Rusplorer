use super::RusplorerApp;
use eframe::egui;
use arboard::Clipboard;
use std::sync::mpsc::channel;
use std::path::PathBuf;
#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use crate::shortcuts::create_lnk_shortcut;

impl RusplorerApp {
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
                    labels.push("\u{1F513} Unlock\u{2026}");
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

                let mut pending_delete: Vec<PathBuf> = Vec::new();

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

                            // Open with… (all files)
                            if !entry.is_dir
                                && ui.add_sized([menu_w, 0.0], egui::Button::new("Open with\u{2026}")).clicked()
                            {
                                #[cfg(windows)]
                                {
                                    // rundll32 OpenAs_RunDLL always shows the picker,
                                    // even when the file has no registered association.
                                    let _ = std::process::Command::new("rundll32.exe")
                                        .args(["shell32.dll,OpenAs_RunDLL", full_path.to_string_lossy().as_ref()])
                                        .spawn();
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
                                && ui.add_sized([menu_w, 0.0], egui::Button::new("\u{1F513} Unlock\u{2026}")).clicked()
                            {
                                self.unlock_dialog_path = Some(full_path.clone());
                                self.unlock_locking_processes = crate::fs_ops::find_locking_processes(&full_path);
                                self.show_unlock_dialog = true;
                                self.show_context_menu = false;
                                self.context_menu_tree_path = None;
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

                // Process pending delete (set inside closure; executed out here to avoid &mut self conflict)
                if !pending_delete.is_empty() {
                    if crate::fs_ops::delete_to_recycle_bin(&pending_delete) {
                        self.last_deleted_paths = pending_delete;
                        self.selected_entries.clear();
                        self.refresh_contents();
                    }
                }
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
            let mut bg_labels = vec!["📁  New folder", "📄  New text file", "↻  Refresh"];
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
                        if ui.add_sized([menu_w, 0.0], egui::Button::new("↻  Refresh")).clicked() {
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
        // ── Modal backdrop ────────────────────────────────────────────────
        if self.show_rename_dialog || self.show_new_item_dialog
            || self.show_archive_dialog
            || self.show_save_session_dialog || self.show_unlock_dialog
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

                    let confirmed = resp.lost_focus();

                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() || confirmed {
                            let name = self.new_item_name_buffer.trim().to_string();
                            if !name.is_empty() {
                                let target = self.current_path.join(&name);
                                if self.new_item_is_dir {
                                    let _ = std::fs::create_dir(&target);
                                    // Invalidate tree cache so the new folder appears in the tree
                                    self.tree_children_cache.remove(&self.current_path);
                                } else {
                                    let _ = std::fs::File::create(&target);
                                }
                                self.refresh_contents();
                                // Select the newly created item
                                self.selected_entries.clear();
                                self.selected_entries.insert(name);
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
                let has_ext = !self.rename_ext.is_empty() && !self.new_folder_mode;
                let mut close_dialog = false;
                let mut do_rename = false;
                let window_title = if self.new_folder_mode { "New folder" } else { "Rename" };
                egui::Window::new(window_title)
                    .collapsible(false)
                    .resizable(false)
                    .min_width(280.0)
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

                        // Text field + clear button on same row
                        // Return the TextEdit response so we can detect Enter via lost_focus
                        let text_response = ui.horizontal(|ui| {
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut self.rename_buffer)
                                    .desired_width(ui.available_width() - 28.0)
                            );
                            response.request_focus();
                            if ui.add_sized([24.0, 0.0], egui::Button::new("✕")).clicked() {
                                self.rename_buffer.clear();
                            }
                            response
                        }).inner;

                        // TextEdit loses focus on Enter; just check lost_focus()
                        if text_response.lost_focus() {
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
                            let _ = std::fs::create_dir(&target);
                            self.tree_children_cache.remove(&self.current_path);
                            self.refresh_contents();
                            self.selected_entries.clear();
                            self.selected_entries.insert(stem);
                        } else {
                            let final_name = if self.rename_show_ext || !has_ext {
                                stem
                            } else {
                                format!("{}{}", stem, self.rename_ext)
                            };
                            let old_path = self.current_path.join(&entry_name);
                            let new_path = self.current_path.join(&final_name);
                            let _ = std::fs::rename(&old_path, &new_path);
                            // Invalidate tree cache for the parent and the old path
                            self.tree_children_cache.remove(&self.current_path);
                            self.tree_children_cache.remove(&old_path);
                            self.refresh_contents();

                        }
                    }
                    self.new_folder_mode = false;
                    self.show_rename_dialog = false;
                } else if close_dialog {
                    self.new_folder_mode = false;
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

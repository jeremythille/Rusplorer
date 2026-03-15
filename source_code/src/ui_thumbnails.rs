use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use crate::types::{FileEntry, FileAction};
use super::RusplorerApp;

impl RusplorerApp {
    pub(crate) fn render_thumbnails(
        &mut self,
        ui: &mut egui::Ui,
        filtered_entries: &[FileEntry],
    ) -> bool {
        let ctx = ui.ctx().clone();
        let mut entry_right_clicked = false;
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

                                    // Filename label with extension colour coding
                                    let dark_mode = ui.visuals().dark_mode;
                                    let name_display = Self::truncate_name(&entry.name, CELL_W - 4.0, ui);
                                    let label_color = if is_selected {
                                        egui::Color32::WHITE
                                    } else if dark_mode {
                                        egui::Color32::from_gray(210)
                                    } else {
                                        egui::Color32::from_gray(70)
                                    };
                                    let job = Self::name_layout_job(&name_display, entry.is_dir, label_color, false, dark_mode);
                                    let galley = ctx.fonts(|f| f.layout_job(job));
                                    let label_pos = egui::pos2(
                                        cell_rect.center().x - galley.size().x * 0.5,
                                        cell_rect.min.y + THUMB + 8.0,
                                    );
                                    p.galley(label_pos, galley, label_color);
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
                                // Cross-check: egui may believe a button is held due to
                                // stale state after a blocking DoDragDrop call.  Only
                                // trust egui if the hardware agrees.
                                let any_btn = {
                                    let egui_any = primary_down || secondary_down;
                                    #[cfg(windows)] {
                                        if egui_any {
                                            use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
                                            let hw_lmb = unsafe { GetAsyncKeyState(0x01) } & (0x8000u16 as i16) != 0;
                                            let hw_rmb = unsafe { GetAsyncKeyState(0x02) } & (0x8000u16 as i16) != 0;
                                            hw_lmb || hw_rmb
                                        } else { false }
                                    }
                                    #[cfg(not(windows))] { egui_any }
                                };
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
        entry_right_clicked
    }
}

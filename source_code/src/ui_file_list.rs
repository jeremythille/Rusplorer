use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use crate::types::{FileEntry, SortColumn, FileAction};
#[cfg(windows)]
use crate::shortcuts::resolve_lnk;
use super::RusplorerApp;

impl RusplorerApp {
    pub(crate) fn render_file_list(
        &mut self,
        ui: &mut egui::Ui,
        filtered_entries: &[FileEntry],
    ) -> (bool, bool) {
        let ctx = ui.ctx().clone();
        let mut entry_right_clicked = false;
        let mut sort_changed = false;
        // ── TABLE (list view) ────────────────────────────────────────────────
        // Table with proper column alignment
        let show_dates = self
            .show_date_columns
            .get(&self.current_path)
            .copied()
            .unwrap_or(false);

        // Pre-compute time values once per frame (avoids per-row Windows API calls)
        let now = SystemTime::now();
        #[cfg(windows)]
        let tz_bias_secs: i64 = {
            use winapi::um::timezoneapi::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
            let mut tzi: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
            let tz_id = unsafe { GetTimeZoneInformation(&mut tzi) };
            let is_dst = tz_id == 2;
            (tzi.Bias + if is_dst { tzi.DaylightBias } else { tzi.StandardBias }) as i64 * 60
        };
        #[cfg(not(windows))]
        let tz_bias_secs: i64 = 0;

        // Same-frame DnD detection for file-table entries
        if self.dnd_active {
            let cursor = ctx.input(|i| i.pointer.hover_pos());
            if let Some(pos) = cursor {
                if let Some(found) = self.entry_rects.iter().find_map(|(name, rect)| {
                    if rect.contains(pos) {
                        let is_parent = name.starts_with("[..]");
                        let full = if is_parent {
                            self.current_path.parent()?.to_path_buf()
                        } else {
                            self.current_path.join(name)
                        };
                        let is_dir = is_parent || full.is_dir();
                        let is_source = self.dnd_sources.contains(&full);
                        if is_dir && !is_source { Some(full) } else { None }
                    } else {
                        None
                    }
                }) {
                    self.dnd_drop_target = Some(found);
                }
            }
        }

        // Clear rect map for this frame
        self.entry_rects.clear();
        self.any_button_hovered = false;

        let row_height = 18.0;

        // Measure actual text widths for tight columns
        let font_id = egui::TextStyle::Body.resolve(ui.style());

        // Find the widest size label in current contents
        let max_size_str = self
            .contents
            .iter()
            .filter_map(|entry| {
                if entry.name.starts_with("[..]") {
                    None
                } else {
                    let full_path = self.current_path.join(&entry.name);
                    self.file_sizes
                        .get(&full_path)
                        .map(|size| Self::format_file_size(*size))
                        .or(Some(if entry.is_dir {
                            "0 B".to_string()
                        } else {
                            "...".to_string()
                        }))
                }
            })
            .max_by_key(|s| s.len())
            .unwrap_or_else(|| "0 B".to_string());

        let size_text_width = ui.fonts(|f| {
            f.layout_no_wrap(max_size_str, font_id.clone(), egui::Color32::WHITE)
                .size()
                .x
        });

        // Check if any directories are still computing
        let has_computing = self.contents.iter().any(|entry| {
            if entry.is_dir && !entry.name.starts_with("[..]") {
                let full_path = self.current_path.join(&entry.name);
                !self.dirs_done.contains(&full_path)
            } else {
                false
            }
        });

        let hourglass_width = if has_computing {
            ui.fonts(|f| {
                f.layout_no_wrap("⏳".to_string(), font_id.clone(), egui::Color32::WHITE)
                    .size()
                    .x
            })
        } else {
            0.0
        };

        let date_text_width = if show_dates {
            ui.fonts(|f| {
                f.layout_no_wrap(
                    "2026-02-17 14:30".to_string(),
                    font_id.clone(),
                    egui::Color32::WHITE,
                )
                .size()
                .x
            })
        } else {
            0.0
        };

        // Calculate exact column widths from available space
        let available = ui.available_width();
        let size_col_w = size_text_width + hourglass_width + 7.0; // text + spinner (if any) + padding
        let date_col_w = if show_dates {
            date_text_width + 20.0
        } else {
            18.0
        }; // +20 for X button + padding
        let name_col_w = (available - size_col_w - date_col_w - 15.0).max(50.0);

        let num_rows = filtered_entries.len();

        // Consume a pending type-to-select scroll request.
        let scroll_to = self.type_select_scroll.take();

        let mut table_builder = TableBuilder::new(ui)
            .striped(true)
            .resizable(false)
            .vscroll(true)
            .drag_to_scroll(false)
            .max_scroll_height(f32::INFINITY)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(name_col_w).clip(true))
            .column(Column::exact(size_col_w))
            .column(Column::exact(date_col_w));
        if let Some(row) = scroll_to {
            table_builder = table_builder.scroll_to_row(row, Some(egui::Align::Center));
        }

        table_builder
            .header(row_height, |mut header| {
                // Name header
                header.col(|ui| {
                    let arrow = if self.sort_column == SortColumn::Name {
                        if self.sort_ascending { " ↑" } else { " ↓" }
                    } else {
                        ""
                    };
                    let text = format!("Name{}", arrow);
                    if ui
                        .add_sized(
                            ui.available_size(),
                            egui::Button::new(egui::RichText::new(&text).strong()),
                        )
                        .clicked()
                    {
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
                        if self.sort_ascending { " ↑" } else { " ↓" }
                    } else {
                        ""
                    };
                    let text = format!("Size{}", arrow);
                    if ui
                        .add_sized(
                            ui.available_size(),
                            egui::Button::new(egui::RichText::new(&text).strong()),
                        )
                        .clicked()
                    {
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
                            if ui
                                .small_button("X")
                                .on_hover_text("Hide date column")
                                .clicked()
                            {
                                self.show_date_columns
                                    .insert(self.current_path.clone(), false);
                                if self.sort_column == SortColumn::Date {
                                    self.sort_column = SortColumn::Name;
                                    self.sort_ascending = true;
                                }
                                sort_changed = true;
                            }
                            let arrow = if self.sort_column == SortColumn::Date {
                                if self.sort_ascending { " ↑" } else { " ↓" }
                            } else {
                                ""
                            };
                            let text = format!("Date{}", arrow);
                            if ui
                                .add_sized(
                                    egui::vec2(ui.available_width(), ui.available_height()),
                                    egui::Button::new(egui::RichText::new(&text).strong()),
                                )
                                .clicked()
                            {
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
                        if ui
                            .small_button("📅")
                            .on_hover_text("Show date column")
                            .clicked()
                        {
                            self.show_date_columns
                                .insert(self.current_path.clone(), true);
                            self.sort_column = SortColumn::Date;
                            self.sort_ascending = false;
                            sort_changed = true;
                        }
                    }
                });
            })
            .body(|body| {
                body.rows(row_height, num_rows, |mut row| {
                    let entry = &filtered_entries[row.index()];

                    let is_selected = self.selected_entries.contains(&entry.name);
                    let is_in_clipboard = self
                        .clipboard_files
                        .contains(&self.current_path.join(&entry.name));
                    let full_path = self.current_path.join(&entry.name);
                    let is_computing = entry.is_dir
                        && !entry.name.starts_with("[..]")
                        && !self.dirs_done.contains(&full_path);

                    let size_label = if entry.name.starts_with("[..]") {
                        String::new()
                    } else {
                        match self.file_sizes.get(&full_path) {
                            Some(size) => Self::format_file_size(*size),
                            None => {
                                if entry.is_dir {
                                    "0 B".to_string()
                                } else {
                                    "...".to_string()
                                }
                            }
                        }
                    };

                    // Determine if this folder is a drop target
                    let entry_abs = if entry.name.starts_with("[..]") {
                        self.current_path.parent().map(|p| p.to_path_buf())
                    } else {
                        Some(self.current_path.join(&entry.name))
                    };
                    let is_drop_target = self.dnd_active
                        && entry.is_dir
                        && entry_abs.as_ref() == self.dnd_drop_target_prev.as_ref();

                    // Name column
                    row.col(|ui| {
                            let col_width = ui.available_width();
                            let text_color = ui.visuals().text_color();
                            let dark_mode = ui.visuals().dark_mode;
                            // Truncate name before the extension so it fits the column.
                            // Subtract button_padding (egui adds it on each side) so the rendered
                            // button never overflows and gets pixel-clipped (which would hide the ext).
                            let btn_pad = ui.style().spacing.button_padding.x;
                            let display_name = Self::truncate_name(&entry.name, col_width - 2.0 * btn_pad - 4.0, ui);

                            let button = if is_drop_target {
                                egui::Button::new(
                                    Self::name_layout_job(&display_name, entry.is_dir, egui::Color32::WHITE, false, dark_mode)
                                )
                                .fill(egui::Color32::from_rgb(80, 200, 80))
                                .frame(false)
                            } else if is_selected && is_in_clipboard {
                                egui::Button::new(
                                    Self::name_layout_job(&display_name, entry.is_dir, egui::Color32::WHITE, true, dark_mode)
                                )
                                .fill(egui::Color32::from_rgb(100, 150, 255))
                                .frame(false)
                            } else if is_selected {
                                egui::Button::new(
                                    Self::name_layout_job(&display_name, entry.is_dir, egui::Color32::WHITE, false, dark_mode)
                                )
                                .fill(egui::Color32::from_rgb(100, 150, 255))
                                .frame(false)
                            } else if is_in_clipboard && entry.is_dir {
                                egui::Button::new(
                                    Self::name_layout_job(&display_name, true, egui::Color32::from_gray(20), true, dark_mode)
                                )
                                .fill(egui::Color32::from_rgb(255, 245, 150))
                                .frame(false)
                            } else if is_in_clipboard {
                                egui::Button::new(
                                    Self::name_layout_job(&display_name, entry.is_dir, text_color, true, dark_mode)
                                )
                                .frame(false)
                            } else if entry.name.starts_with("[..]") {
                                egui::Button::new(&display_name)
                                    .fill(egui::Color32::TRANSPARENT)
                                    .frame(false)
                            } else if entry.is_dir {
                                egui::Button::new(
                                    Self::name_layout_job(&display_name, true, egui::Color32::from_gray(20), false, dark_mode)
                                )
                                .fill(egui::Color32::from_rgb(255, 245, 150))
                                .frame(false)
                            } else {
                                egui::Button::new(
                                    Self::name_layout_job(&display_name, false, text_color, false, dark_mode)
                                )
                                .frame(false)
                            };

                            let button = button.sense(egui::Sense::click_and_drag());
                            let response = ui.horizontal(|ui| ui.add(button)).inner;

                            // Show the full name in a tooltip when the name was truncated.
                            if display_name != entry.name && response.hovered() {
                                egui::show_tooltip_text(
                                    ui.ctx(),
                                    egui::Id::new("name_tooltip").with(&entry.name),
                                    &entry.name,
                                );
                            }

                            // For DnD drop-target detection use the full row width, not just
                            // the name-button width (which is as narrow as the text).
                            // Hovering anywhere on the row — name, size, or date column —
                            // should be sufficient to identify it as a drop target.
                            let full_row_rect = egui::Rect::from_min_size(
                                response.rect.min,
                                egui::vec2(name_col_w + size_col_w + date_col_w, response.rect.height()),
                            );
                            self.entry_rects.insert(entry.name.clone(), full_row_rect);
                            // Keep hover feedback broad, but only the name cell should start
                            // a drag. Otherwise drawing a rectangle in empty row space starts
                            // an unintended file drag.
                            let cursor_over_name = ui.input(|i| {
                                i.pointer
                                    .hover_pos()
                                    .map_or(false, |p| response.rect.contains(p))
                            });
                            if cursor_over_name || response.hovered() {
                                self.any_button_hovered = true;
                            }

                            // Drag-and-drop: raw pointer state detection
                            // (avoids egui's drag_started_by/dragged_by which desync
                            //  after the blocking DoDragDrop OLE call)
                            let primary_down = ui.input(|i| i.pointer.primary_down());
                            let secondary_down = ui.input(|i| i.pointer.button_down(egui::PointerButton::Secondary));
                            // Cross-check: egui may believe a button is held due to
                            // stale state after a blocking DoDragDrop call.  Only
                            // trust egui if the hardware agrees.
                            let any_btn_down = {
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

                            // Detect new press on this entry
                            if cursor_over_name
                                && any_btn_down
                                && !self.dnd_active
                                && self.dnd_suppress == 0
                                && !self.is_dragging_selection
                                && self.dnd_start_pos.is_none()
                                && !entry.name.starts_with("[..]")
                                && !self.ole_drag_in_active.load(Ordering::SeqCst)
                            {
                                self.dnd_start_pos = ui.input(|i| i.pointer.hover_pos());
                                self.dnd_drag_entry = Some(entry.name.clone());
                                self.dnd_is_right_click = secondary_down;
                            }

                            // Clear stale press when pointer is released without triggering a drag
                            if !self.dnd_active && !any_btn_down {
                                self.dnd_start_pos = None;
                                self.dnd_drag_entry = None;
                                self.dnd_is_right_click = false;
                            }

                            // Activate DnD when dragged far enough with button held
                            let is_this_entry_drag = any_btn_down
                                && self.dnd_drag_entry.as_deref() == Some(&entry.name);
                            if is_this_entry_drag
                                && !self.dnd_active
                                && !self.is_dragging_selection
                                && !entry.name.starts_with("[..]")
                            {
                                if let Some(start) = self.dnd_start_pos {
                                    if let Some(current) = ui.input(|i| i.pointer.hover_pos()) {
                                        if start.distance(current) > 5.0 {
                                            // Start drag
                                            if self.selected_entries.contains(&entry.name) {
                                                // Drag all selected entries
                                                self.dnd_sources = self
                                                    .selected_entries
                                                    .iter()
                                                    .map(|n| self.current_path.join(n))
                                                    .collect();
                                            } else {
                                                // Drag just this entry
                                                self.dnd_sources = vec![self.current_path.join(&entry.name)];
                                                self.selected_entries.clear();
                                                self.selected_entries.insert(entry.name.clone());
                                            }
                                            let count = self.dnd_sources.len();
                                            self.dnd_label = if count == 1 {
                                                if entry.is_dir {
                                                    format!("📁 {}", &entry.name)
                                                } else {
                                                    format!("📄 {}", &entry.name)
                                                }
                                            } else {
                                                format!("📦 {} items", count)
                                            };
                                            self.dnd_active = true;
                                            // Show move/copy hint in label for right-click drag
                                            if self.dnd_is_right_click {
                                                self.dnd_label = format!("{}  [Move / Copy / Shortcut]", self.dnd_label);
                                            }
                                        }
                                    }
                                }
                            }

                            if response.clicked() {
                                let is_ctrl = ui.input(|i| i.modifiers.ctrl);
                                let is_shift = ui.input(|i| i.modifiers.shift);
                                if is_shift {
                                    // Range select: from last_clicked_entry to this entry
                                    if let Some(ref anchor) = self.last_clicked_entry {
                                        let anchor_idx = filtered_entries.iter().position(|e| e.name == *anchor);
                                        let click_idx = filtered_entries.iter().position(|e| e.name == entry.name);
                                        if let (Some(a), Some(b)) = (anchor_idx, click_idx) {
                                            let lo = a.min(b);
                                            let hi = a.max(b);
                                            if !is_ctrl {
                                                self.selected_entries.clear();
                                            }
                                            for i in lo..=hi {
                                                let name = &filtered_entries[i].name;
                                                if !name.starts_with("[..]") {
                                                    self.selected_entries.insert(name.clone());
                                                }
                                            }
                                        }
                                    } else {
                                        // No anchor yet — treat as normal click
                                        self.selected_entries.clear();
                                        self.selected_entries.insert(entry.name.clone());
                                        self.last_clicked_entry = Some(entry.name.clone());
                                    }
                                    // Don't update anchor on shift-click (allows extending)
                                } else if is_ctrl {
                                    if self.selected_entries.contains(&entry.name) {
                                        self.selected_entries.remove(&entry.name);
                                    } else {
                                        self.selected_entries.insert(entry.name.clone());
                                    }
                                    self.last_clicked_entry = Some(entry.name.clone());
                                } else {
                                    self.selected_entries.clear();
                                    self.selected_entries.insert(entry.name.clone());
                                    self.last_clicked_entry = Some(entry.name.clone());
                                }
                            }

                            // Use raw pointer-position check instead of response.secondary_clicked()
                            // because secondary_clicked() relies on hovered() which returns false
                            // when a Foreground Area (bg context menu) overlaps this entry.
                            let raw_secondary = !self.dnd_is_right_click
                                && self.dnd_suppress == 0
                                && ctx.input(|i| i.pointer.secondary_released())
                                && ctx.input(|i| {
                                    i.pointer.hover_pos()
                                        .map_or(false, |p| response.rect.contains(p))
                                });

                            if raw_secondary {
                                // Select the right-clicked entry if not already part of selection
                                if !self.selected_entries.contains(&entry.name) {
                                    self.selected_entries.clear();
                                    self.selected_entries.insert(entry.name.clone());
                                }
                                // Snapshot the selection NOW before any click-through can clear it
                                self.context_menu_selection = self
                                    .selected_entries
                                    .iter()
                                    .map(|n| self.current_path.join(n))
                                    .collect();
                                self.show_context_menu = true;
                                self.show_bg_context_menu = false;
                                self.context_menu_entry = Some(entry.clone());
                                self.context_menu_tree_path = None; // file list: use current_path + name
                                self.context_menu_position =
                                    ui.input(|i| i.pointer.hover_pos().unwrap_or_default());
                                entry_right_clicked = true;
                            }

                            if response.double_clicked() {
                                if entry.name.starts_with("[..]") {
                                    self.selected_action = Some(FileAction::GoToParent);
                                } else if entry.is_dir {
                                    let new_path = self.current_path.join(&entry.name);
                                    self.selected_action = Some(FileAction::OpenDir(new_path));
                                } else {
                                    let full_path = self.current_path.join(&entry.name);
                                    // Resolve .lnk shortcuts
                                    #[cfg(windows)]
                                    let resolved = if entry.name
                                        .to_lowercase()
                                        .ends_with(".lnk")
                                    {
                                        resolve_lnk(&full_path)
                                    } else {
                                        None
                                    };
                                    #[cfg(not(windows))]
                                    let resolved: Option<PathBuf> = None;
                                    if let Some(target) = resolved {
                                        if target.is_dir() {
                                            self.selected_action =
                                                Some(FileAction::OpenDir(target));
                                        } else {
                                            let _ = std::process::Command::new("explorer")
                                                .arg(&target)
                                                .spawn();
                                        }
                                    } else {
                                        let _ = std::process::Command::new("explorer")
                                            .arg(&full_path)
                                            .spawn();
                                    }
                                }
                            }

                            // Draw size bar at bottom of cell
                            if !entry.name.starts_with("[..]") {
                                if let Some(size) = self.file_sizes.get(&full_path) {
                                    let bar_width = if self.max_file_size > 0 {
                                        (*size as f32 / self.max_file_size as f32) * col_width
                                    } else {
                                        0.0
                                    };
                                    let bar_rect = egui::Rect::from_min_size(
                                        egui::pos2(
                                            response.rect.left(),
                                            response.rect.bottom() - 2.0,
                                        ),
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
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if !size_label.is_empty() {
                                        let size_text = if is_in_clipboard {
                                            egui::RichText::new(&size_label).weak().italics()
                                        } else {
                                            egui::RichText::new(&size_label).weak()
                                        };
                                        ui.label(size_text);
                                    }
                                    if is_computing {
                                        if self.is_focused {
                                            // Animated hourglass while computing
                                            let spinner_chars = ['⏳', '⌛'];
                                            let time = ui.input(|i| i.time);
                                            let idx =
                                                ((time * 2.0) as usize) % spinner_chars.len();
                                            ui.label(spinner_chars[idx].to_string());
                                            ctx.request_repaint();
                                        } else {
                                            // Static hourglass when paused (window unfocused)
                                            ui.label("⏳");
                                        }
                                    }
                                },
                            );
                        });

                        // Date column - right aligned, tight
                        row.col(|ui| {
                            if show_dates && !entry.name.starts_with("[..]") {
                                let date_text = if let Some(modified) = entry.modified {
                                    Self::format_modified_time(modified, tz_bias_secs)
                                } else {
                                    String::new()
                                };
                                if !date_text.is_empty() {
                                    // Paint age-based background color
                                    let is_future = entry.modified
                                        .map(|m| m > now)
                                        .unwrap_or(false);
                                    if let Some(modified) = entry.modified {
                                        let bg = Self::age_color(modified, now);
                                        ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let text_color = if is_future {
                                                egui::Color32::WHITE
                                            } else {
                                                egui::Color32::from_rgb(60, 60, 60)
                                            };
                                            let label = if is_in_clipboard {
                                                egui::RichText::new(&date_text).color(text_color).italics()
                                            } else {
                                                egui::RichText::new(&date_text).color(text_color)
                                            };
                                            ui.label(label);
                                        },
                                    );
                                }
                            }
                        });
                    });
                });
        (entry_right_clicked, sort_changed)
    }
}

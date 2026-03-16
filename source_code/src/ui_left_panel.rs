use eframe::egui;
use std::path::PathBuf;
use crate::tree::render_tree_node;
use crate::types::FileEntry;
use super::RusplorerApp;

impl RusplorerApp {
    pub(crate) fn render_left_panel(&mut self, ctx: &egui::Context) -> Option<PathBuf> {
        let mut nav_from_panel: Option<PathBuf> = None;
        egui::SidePanel::left("left_panel")
            .exact_width(self.left_panel_width)
            .resizable(false)
            .show(ctx, |ui| {
                // ── Favorites ────────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⭐ Favorites"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new("+").small().frame(false))
                            .on_hover_text("Add current folder to favorites")
                            .clicked()
                        {
                            if !self.favorites.contains(&self.current_path) {
                                self.favorites.push(self.current_path.clone());
                                self.favorites.sort_by(|a, b| {
                                    let a_name = a.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_else(|| a.to_string_lossy().to_lowercase().into());
                                    let b_name = b.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_else(|| b.to_string_lossy().to_lowercase().into());
                                    a_name.cmp(&b_name)
                                });
                                self.config.favorites = self
                                    .favorites
                                    .iter()
                                    .map(|f| f.to_string_lossy().to_string())
                                    .collect();
                                self.config.save();
                            }
                        }
                    });
                });

                let mut remove_fav: Option<usize> = None;
                for (i, fav) in self.favorites.iter().enumerate() {
                    let name = fav
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| fav.to_string_lossy().to_string());
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        let is_cur = *fav == self.current_path;
                        let label = egui::RichText::new(&name)
                            .small()
                            .color(if is_cur { egui::Color32::WHITE } else { egui::Color32::BLACK });
                        let btn = if is_cur {
                            egui::Button::new(label).fill(egui::Color32::from_rgb(100, 150, 255)).frame(false)
                        } else {
                            egui::Button::new(label).fill(egui::Color32::from_rgb(255, 245, 150)).frame(false)
                        };
                        if ui
                            .add(btn)
                            .on_hover_text(Self::format_path_display(fav))
                            .clicked()
                        {
                            nav_from_panel = Some(fav.clone());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(egui::Button::new("×").small().frame(false)).clicked() {
                                remove_fav = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = remove_fav {
                    self.favorites.remove(i);
                    self.config.favorites = self
                        .favorites
                        .iter()
                        .map(|f| f.to_string_lossy().to_string())
                        .collect();
                    self.config.save();
                }

                // Visual separator between favorites and folder tree
                ui.add_space(8.0);
                let (separator_rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
                ui.painter().rect_filled(separator_rect, 0.0, egui::Color32::from_gray(128));
                ui.add_space(8.0);

                // ── Folder tree ──────────────────────────────────────────
                let dnd_active = self.dnd_active;
                let dnd_drop_target = self.dnd_drop_target_prev.clone(); // use prev for display
                let dnd_sources: Vec<PathBuf> = self.dnd_sources.clone();
                let mut tree_hovered_drop: Option<PathBuf> = None;

                // Use a child_ui with a strict clip rect so the tree scroll
                // area cannot paint over the favorites section above.
                let tree_available_h = ui.available_height();
                egui::ScrollArea::vertical()
                    .id_source("tree_scroll")
                    .auto_shrink([false, false])
                    .max_height(tree_available_h)
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.set_max_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let drives: Vec<PathBuf> = self
                            .available_drives
                            .iter()
                            .map(PathBuf::from)
                            .collect();
                        let mut tree_right_clicked: Option<(PathBuf, egui::Pos2)> = None;
                        for drive in &drives {
                            render_tree_node(
                                ui,
                                drive,
                                &mut self.tree_expanded,
                                &mut self.tree_children_cache,
                                &mut nav_from_panel,
                                &self.current_path.clone(),
                                0,
                                dnd_active,
                                &dnd_sources,
                                &dnd_drop_target,
                                &mut tree_hovered_drop,
                                &mut tree_right_clicked,
                                &self.context_menu_tree_highlight.clone(),
                            );
                        }
                        if let Some((rclick_path, rclick_pos)) = tree_right_clicked {
                            let name = rclick_path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| rclick_path.to_string_lossy().to_string());
                            self.show_context_menu = true;
                            self.show_bg_context_menu = false;
                            self.context_menu_entry = Some(FileEntry {
                                name,
                                is_dir: true,
                                size: 0,
                                modified: None,
                            });
                            // Override current_path so the context menu resolves the full path
                            // by joining current_path + name. Since rclick_path already IS the
                            // full path, set current_path to its parent temporarily — or better,
                            // store the full path directly.
                            self.context_menu_tree_path = Some(rclick_path.clone());
                            self.context_menu_tree_highlight = Some(rclick_path.clone());
                            self.context_menu_position = rclick_pos;
                            // Snapshot: just this one tree path
                            self.context_menu_selection = vec![rclick_path];
                        }
                    });
                if let Some(target) = tree_hovered_drop {
                    self.dnd_drop_target = Some(target);
                }
            });
        nav_from_panel
    }
}

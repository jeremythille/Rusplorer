#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::collections::HashMap;
use arboard::Clipboard;
use serde::{Deserialize, Serialize};

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Rusplorer",
        options,
        Box::new(|cc| {
            let mut style = (*cc.egui_ctx.style()).clone();
            // Set 11pt font size for all text styles
            for (_, font_id) in &mut style.text_styles {
                font_id.size = 11.0;
            }
            style.spacing.button_padding = egui::vec2(2.0, 0.0);
            style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 10);
            style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
            style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
            cc.egui_ctx.set_style(style);
            Box::new(RusplorerApp::default())
        }),
    )
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Config {
    last_path: String,
}

impl Config {
    fn path() -> PathBuf {
        let exe_path = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("rusplorer.exe"));
        let mut config_path = exe_path.parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf();
        config_path.push("rusplorer.config.json");
        config_path
    }

    fn load() -> Self {
        if let Ok(content) = std::fs::read_to_string(Self::path()) {
            if let Ok(config) = serde_json::from_str(&content) {
                return config;
            }
        }
        Config {
            last_path: "C:\\".to_string(),
        }
    }

    fn save(&self) {
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), content);
        }
    }
}

struct RusplorerApp {
    current_path: PathBuf,
    contents: Vec<FileEntry>,
    selected_action: Option<FileAction>,
    back_history: Vec<PathBuf>,
    forward_history: Vec<PathBuf>,
    available_drives: Vec<String>,
    file_sizes: HashMap<PathBuf, u64>,
    size_receiver: Option<Receiver<(PathBuf, u64)>>,
    dragged_files: Vec<PathBuf>,
    show_drop_menu: bool,
    drop_menu_position: egui::Pos2,
    is_right_click_drag: bool,
    config: Config,
    max_file_size: u64,
}

#[derive(Clone)]
enum FileAction {
    OpenDir(PathBuf),
    GoToParent,
}

#[derive(Clone)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: u64,
}

impl Default for RusplorerApp {
    fn default() -> Self {
        let available_drives = Self::list_drives();
        let config = Config::load();
        let start_path = PathBuf::from(&config.last_path);
        let current_path = if start_path.exists() {
            start_path
        } else {
            PathBuf::from("C:\\")
        };

        let mut app = Self {
            current_path,
            contents: Vec::new(),
            selected_action: None,
            back_history: Vec::new(),
            forward_history: Vec::new(),
            available_drives,
            file_sizes: HashMap::new(),
            size_receiver: None,
            dragged_files: Vec::new(),
            show_drop_menu: false,
            drop_menu_position: egui::Pos2::ZERO,
            is_right_click_drag: false,
            config,
            max_file_size: 0,
        };
        app.refresh_contents();
        app
    }
}

impl RusplorerApp {
    fn list_drives() -> Vec<String> {
        let mut drives = Vec::new();
        
        // Check drives A through Z
        for letter in b'A'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            if PathBuf::from(&drive).exists() {
                drives.push(drive);
            }
        }
        
        drives
    }

    fn refresh_contents(&mut self) {
        self.contents.clear();
        self.file_sizes.clear();
        self.max_file_size = 0;

        // Add parent directory option
        if let Some(parent) = self.current_path.parent() {
            if parent != self.current_path {
                self.contents.push(FileEntry {
                    name: "[..] Parent Directory".to_string(),
                    is_dir: true,
                    size: 0,
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
                    FileEntry { name, is_dir, size: 0 }
                })
                .collect();

            items.sort_by(|a, b| {
                match (a.is_dir, b.is_dir) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => a.name.cmp(&b.name),
                }
            });

            self.contents.extend(items);
        }

        // Start background thread to load file sizes
        let current_path = self.current_path.clone();
        let (tx, rx) = channel();
        
        std::thread::spawn(move || {
            if let Ok(entries) = std::fs::read_dir(&current_path) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if !path.is_dir() {
                        if let Ok(metadata) = entry.metadata() {
                            let _ = tx.send((path, metadata.len()));
                        }
                    }
                }
            }
        });

        self.size_receiver = Some(rx);
    }

    fn navigate_to(&mut self, path: PathBuf) {
        if path.exists() && path.is_dir() {
            // Only add to history if it's different from current path
            if path != self.current_path {
                self.back_history.push(self.current_path.clone());
                self.forward_history.clear(); // Clear forward history on new navigation
            }
            self.current_path = path;
            
            // Save the current path to config
            self.config.last_path = self.current_path.to_string_lossy().to_string();
            self.config.save();
            
            self.refresh_contents();
        }
    }

    fn go_back(&mut self) {
        if let Some(previous) = self.back_history.pop() {
            self.forward_history.push(self.current_path.clone());
            self.current_path = previous;
            self.refresh_contents();
        }
    }

    fn go_forward(&mut self) {
        if let Some(next) = self.forward_history.pop() {
            self.back_history.push(self.current_path.clone());
            self.current_path = next;
            self.refresh_contents();
        }
    }

    fn get_breadcrumbs(&self) -> Vec<(PathBuf, String)> {
        let mut breadcrumbs = Vec::new();
        let mut path = self.current_path.clone();

        // Skip the drive letter, we only want the path components
        if let Some(parent) = path.parent() {
            if parent != path {
                // Get all path components except the drive
                let mut components = Vec::new();
                loop {
                    if let Some(file_name) = path.file_name() {
                        if let Some(name_str) = file_name.to_str() {
                            components.push((path.clone(), name_str.to_string()));
                        }
                    }
                    if let Some(parent) = path.parent() {
                        if parent == path {
                            break; // We've reached the root (drive letter)
                        }
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

    fn format_path_display(path: &PathBuf) -> String {
        path.to_string_lossy().replace("\\", "/")
    }

    fn format_file_size(bytes: u64) -> String {
        const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
        let mut size = bytes as f64;
        let mut unit_index = 0;

        while size >= 1024.0 && unit_index < UNITS.len() - 1 {
            size /= 1024.0;
            unit_index += 1;
        }

        if unit_index == 0 {
            format!("{} {}", bytes, UNITS[0])
        } else {
            format!("{:.1} {}", size, UNITS[unit_index])
        }
    }

    fn copy_files(sources: &[PathBuf], dest: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        for source in sources {
            if source.is_dir() {
                copy_dir_recursive(source, &dest.join(source.file_name().unwrap()))?;
            } else {
                let file_name = source.file_name().unwrap();
                std::fs::copy(source, dest.join(file_name))?;
            }
        }
        Ok(())
    }

    fn move_files(sources: &[PathBuf], dest: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        for source in sources {
            let file_name = source.file_name().unwrap();
            std::fs::rename(source, dest.join(file_name))?;
        }
        Ok(())
    }
}

fn copy_dir_recursive(src: &PathBuf, dst: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
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

impl eframe::App for RusplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Handle drag and drop
        ctx.input(|i| {
            let dropped_files = &i.raw.dropped_files;
            if !dropped_files.is_empty() {
                self.dragged_files = dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect();
                if !self.dragged_files.is_empty() {
                    // Check if it's a right-click drag (we'll detect this by checking pointer events)
                    self.is_right_click_drag = i.pointer.button_down(egui::PointerButton::Secondary);
                    self.show_drop_menu = self.is_right_click_drag;
                    
                    // Left click defaults to move
                    if !self.is_right_click_drag {
                        let files = self.dragged_files.clone();
                        let dest = self.current_path.clone();
                        std::thread::spawn(move || {
                            let _ = RusplorerApp::move_files(&files, &dest);
                        });
                        self.dragged_files.clear();
                        // Schedule refresh for next frame
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }
        });

        // Receive file sizes from background thread
        if let Some(ref rx) = self.size_receiver {
            while let Ok((path, size)) = rx.try_recv() {
                self.file_sizes.insert(path, size);
                if size > self.max_file_size {
                    self.max_file_size = size;
                }
            }
        }

        // Handle mouse buttons 4 and 5 (back/forward)
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::PointerButton { button, pressed, .. } = event {
                    if *pressed {
                        match button {
                            egui::PointerButton::Extra1 => {
                                // Mouse button 4 (back)
                                self.go_back();
                            }
                            egui::PointerButton::Extra2 => {
                                // Mouse button 5 (forward)
                                self.go_forward();
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        // Handle keyboard shortcuts for back/forward
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) && ctx.input(|i| i.modifiers.alt) {
            self.go_back();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) && ctx.input(|i| i.modifiers.alt) {
            self.go_forward();
        }

        // Handle pending actions
        if let Some(action) = self.selected_action.take() {
            match action {
                FileAction::GoToParent => {
                    if let Some(parent) = self.current_path.parent() {
                        self.navigate_to(parent.to_path_buf());
                    }
                }
                FileAction::OpenDir(path) => {
                    self.navigate_to(path);
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // Drive selector
            let mut selected_drive: Option<PathBuf> = None;
            ui.horizontal(|ui| {
                ui.label("Drive:");
                for drive in &self.available_drives {
                    let current_drive = self.current_path.to_string_lossy();
                    let is_current = current_drive.starts_with(drive);
                    
                    if ui.selectable_label(is_current, drive).clicked() {
                        selected_drive = Some(PathBuf::from(drive));
                    }
                }
            });
            
            // Handle drive selection
            if let Some(drive) = selected_drive {
                self.navigate_to(drive);
            }

            ui.separator();

            // Breadcrumbs
            let breadcrumbs = self.get_breadcrumbs();
            let mut navigate_to_path: Option<PathBuf> = None;
            
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = [5.0, 5.0].into();
                
                for (i, (path, name)) in breadcrumbs.iter().enumerate() {
                    let is_last = i == breadcrumbs.len() - 1;
                    
                    if i > 0 {
                        ui.vertical(|ui| {
                            ui.add_space(5.0);
                            ui.label("/");
                        });
                    }
                    
                    if is_last {
                        // Current directory - not clickable, just plain text
                        ui.vertical(|ui| {
                            ui.add_space(5.0);
                            ui.label(name);
                        });
                    } else {
                        // Parent directories - clickable pills
                        let button = egui::Button::new(egui::RichText::new(name).color(egui::Color32::BLACK))
                            .fill(egui::Color32::from_rgb(255, 245, 150))
                            .frame(true);
                        if ui.add(button).clicked() {
                            navigate_to_path = Some(path.clone());
                        }
                    }
                }
                
                // Copy path button
                if ui.button("📋").on_hover_text("Copy full path").clicked() {
                    if let Ok(mut clipboard) = Clipboard::new() {
                        let path_display = Self::format_path_display(&self.current_path);
                        let _ = clipboard.set_text(path_display);
                    }
                }
            });

            if let Some(path) = navigate_to_path {
                self.navigate_to(path);
            }

            ui.separator();

            // Navigation buttons
            ui.horizontal(|ui| {
                let back_enabled = !self.back_history.is_empty();
                if ui.add_enabled(back_enabled, egui::Button::new("◀ Back")).clicked() {
                    self.go_back();
                }

                let forward_enabled = !self.forward_history.is_empty();
                if ui.add_enabled(forward_enabled, egui::Button::new("Forward ▶")).clicked() {
                    self.go_forward();
                }

                if ui.button("🔄 Refresh").clicked() {
                    self.refresh_contents();
                }
            });

            ui.separator();

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = [0.0, -1.0].into();
                    for entry in self.contents.clone() {
                        let icon = if entry.is_dir { "📁" } else { "📄" };
                        let name_label = format!("{} {}", icon, entry.name);
                        
                        let size_label = if entry.is_dir {
                            String::new()
                        } else {
                            // Try to get size from cache, show loading if not ready
                            let full_path = self.current_path.join(&entry.name);
                            match self.file_sizes.get(&full_path) {
                                Some(size) => Self::format_file_size(*size),
                                None => "...".to_string(),
                            }
                        };

                        let button = if entry.is_dir {
                            // Light yellow background for folders
                            egui::Button::new(&name_label)
                                .fill(egui::Color32::from_rgb(255, 245, 150))
                        } else {
                            // Default styling for files
                            egui::Button::new(&name_label)
                        };

                        let clicked = ui.horizontal(|ui| {
                            let button_response = ui.add(button);
                            
                            // Add space to push size to the right
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if !size_label.is_empty() {
                                    ui.label(egui::RichText::new(&size_label).weak());
                                }
                            });
                            
                            button_response.clicked()
                        }).inner;

                        // Draw size bar underneath if file has a size
                        if !entry.is_dir && !size_label.is_empty() && size_label != "..." {
                            let full_path = self.current_path.join(&entry.name);
                            if let Some(size) = self.file_sizes.get(&full_path) {
                                let bar_width = if self.max_file_size > 0 {
                                    (*size as f32 / self.max_file_size as f32) * ui.available_width()
                                } else {
                                    0.0
                                };

                                let bar_rect = egui::Rect::from_min_size(
                                    ui.cursor().min + egui::vec2(0.0, -2.0),
                                    egui::vec2(bar_width, 1.0),
                                );
                                ui.painter().rect_filled(
                                    bar_rect,
                                    0.0,
                                    egui::Color32::from_rgb(100, 150, 255),
                                );
                                ui.allocate_space(egui::vec2(ui.available_width(), 2.0));
                            }
                        }

                        if clicked {
                            if entry.name.starts_with("[..]") {
                                self.selected_action = Some(FileAction::GoToParent);
                            } else if entry.is_dir {
                                let new_path = self.current_path.join(&entry.name);
                                self.selected_action = Some(FileAction::OpenDir(new_path));
                            }
                        }
                    }
                });
        });

        // Drop menu context window
        if self.show_drop_menu && !self.dragged_files.is_empty() {
            egui::Window::new("Copy or Move?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(format!("{} item(s) dropped", self.dragged_files.len()));
                    
                    ui.horizontal(|ui| {
                        if ui.button("Copy").clicked() {
                            let files = self.dragged_files.clone();
                            let dest = self.current_path.clone();
                            std::thread::spawn(move || {
                                let _ = RusplorerApp::copy_files(&files, &dest);
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            });
                            self.show_drop_menu = false;
                            self.dragged_files.clear();
                        }
                        
                        if ui.button("Move").clicked() {
                            let files = self.dragged_files.clone();
                            let dest = self.current_path.clone();
                            std::thread::spawn(move || {
                                let _ = RusplorerApp::move_files(&files, &dest);
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            });
                            self.show_drop_menu = false;
                            self.dragged_files.clear();
                        }

                        if ui.button("Cancel").clicked() {
                            self.show_drop_menu = false;
                            self.dragged_files.clear();
                        }
                    });
                });
        }

        // Refresh contents periodically to catch updates from background threads
        if self.dragged_files.is_empty() && !self.show_drop_menu {
            // Let the file watcher pick up changes
        }

        // Only repaint if sizes are still being loaded or user is interacting
        if self.size_receiver.is_some() {
            ctx.request_repaint();
        } else {
            // No active operations, repaint at a lower rate
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::collections::HashMap;

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Rusplorer",
        options,
        Box::new(|_cc| Box::new(RusplorerApp::default())),
    )
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
        let mut app = Self {
            current_path: PathBuf::from("C:\\"),
            contents: Vec::new(),
            selected_action: None,
            back_history: Vec::new(),
            forward_history: Vec::new(),
            available_drives,
            file_sizes: HashMap::new(),
            size_receiver: None,
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
}

impl eframe::App for RusplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Receive file sizes from background thread
        if let Some(ref rx) = self.size_receiver {
            while let Ok((path, size)) = rx.try_recv() {
                self.file_sizes.insert(path, size);
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
            ui.heading("📁 Rusplorer");
            
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
                    for entry in self.contents.clone() {
                        let icon = if entry.is_dir { "📁" } else { "📄" };
                        let name_label = format!("{} {}", icon, entry.name);
                        
                        let size_label = if entry.is_dir {
                            String::new()
                        } else {
                            // Try to get size from cache, show loading if not ready
                            let full_path = self.current_path.join(&entry.name);
                            match self.file_sizes.get(&full_path) {
                                Some(size) => format!("{} bytes", size),
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
    }
}

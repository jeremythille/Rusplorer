use eframe::egui;
use std::path::PathBuf;

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
        let mut app = Self {
            current_path: PathBuf::from("C:\\"),
            contents: Vec::new(),
            selected_action: None,
        };
        app.refresh_contents();
        app
    }
}

impl RusplorerApp {
    fn refresh_contents(&mut self) {
        self.contents.clear();

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

        // List directory contents
        if let Ok(entries) = std::fs::read_dir(&self.current_path) {
            let mut items: Vec<_> = entries
                .filter_map(|e| e.ok())
                .map(|e| {
                    let path = e.path();
                    let name = e.file_name().to_string_lossy().to_string();
                    let is_dir = path.is_dir();
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    FileEntry { name, is_dir, size }
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
    }

    fn navigate_to(&mut self, path: PathBuf) {
        if path.exists() && path.is_dir() {
            self.current_path = path;
            self.refresh_contents();
        }
    }
}

impl eframe::App for RusplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
            ui.label(format!("Path: {}", self.current_path.display()));

            ui.separator();

            if ui.button("🔄 Refresh").clicked() {
                self.refresh_contents();
            }

            ui.separator();

            for entry in self.contents.clone() {
                let icon = if entry.is_dir { "📁" } else { "📄" };
                let label = if entry.is_dir {
                    format!("{} {}", icon, entry.name)
                } else {
                    format!("{} {}  ({} bytes)", icon, entry.name, entry.size)
                };

                if ui.button(&label).clicked() {
                    if entry.name.starts_with("[..]") {
                        self.selected_action = Some(FileAction::GoToParent);
                    } else if entry.is_dir {
                        let new_path = self.current_path.join(&entry.name);
                        self.selected_action = Some(FileAction::OpenDir(new_path));
                    }
                }
            }
        });
    }
}

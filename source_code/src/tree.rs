/// Tree panel rendering helpers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::fs_ops::read_dir_children;

/// Recursively renders one node (folder) in the left-panel directory tree.
///
/// Caller is responsible for building the initial `children_cache` and for
/// acting on the returned `nav` / `hovered_drop` / `tree_right_clicked` values
/// after the full tree has been drawn.
#[allow(clippy::too_many_arguments)]
pub fn render_tree_node(
    ui: &mut egui::Ui,
    path: &PathBuf,
    expanded: &mut HashSet<PathBuf>,
    children_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
    nav: &mut Option<PathBuf>,
    current_path: &PathBuf,
    depth: usize,
    dnd_active: bool,
    dnd_sources: &[PathBuf],
    dnd_drop_target: &Option<PathBuf>,
    hovered_drop: &mut Option<PathBuf>,
    tree_right_clicked: &mut Option<(PathBuf, egui::Pos2)>,
    highlight_path: &Option<PathBuf>,
) {
    let is_expanded = expanded.contains(path);
    let display_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().trim_end_matches(|c| c == '\\' || c == '/').to_string());

    let indent = depth as f32 * 10.0;
    let max_w = ui.available_width();
    let is_current = path == current_path;
    let is_ancestor = !is_current && current_path.ancestors().any(|a| a == path.as_path());
    let is_tree_highlighted = highlight_path.as_ref() == Some(path);

    // Truncate display name to fit available width (prevents layout overflow)
    let font_id = if is_ancestor || is_current || is_tree_highlighted {
        egui::FontId::new(11.0, egui::FontFamily::Name("Bold".into()))
    } else {
        egui::FontId::new(11.0, egui::FontFamily::Proportional)
    };
    let btn_width = max_w - indent - 4.0; // padding
    let truncated_name = {
        let fonts = ui.fonts(|f| f.clone());
        let full_w = fonts
            .layout_no_wrap(display_name.clone(), font_id.clone(), egui::Color32::WHITE)
            .size()
            .x;
        if full_w <= btn_width || btn_width <= 0.0 {
            display_name.clone()
        } else {
            let ellipsis = "…";
            let mut truncated = display_name.clone();
            while !truncated.is_empty() {
                truncated.pop();
                let candidate = format!("{}{}", truncated, ellipsis);
                let w = fonts
                    .layout_no_wrap(candidate.clone(), font_id.clone(), egui::Color32::WHITE)
                    .size()
                    .x;
                if w <= btn_width {
                    break;
                }
            }
            format!("{}…", truncated)
        }
    };

    let is_tree_drop_target = dnd_active
        && dnd_drop_target.as_ref() == Some(path)
        && !is_current;

    let response = ui.allocate_ui_with_layout(
        egui::vec2(max_w, 16.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_max_width(max_w);
            ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()));
            if indent > 0.0 {
                ui.add_space(indent);
            }
            let text_color = if is_current || is_tree_drop_target {
                egui::Color32::WHITE
            } else if is_ancestor {
                // Yellow/orange ancestor background — force dark text regardless of theme
                egui::Color32::from_gray(20)
            } else if is_tree_highlighted {
                // Light blue highlight — force dark text regardless of theme
                egui::Color32::from_gray(20)
            } else {
                // Yellow background — force dark text regardless of dark mode
                egui::Color32::from_gray(20)
            };
            let base_text = egui::RichText::new(&truncated_name).color(text_color);
            let label_text = if is_ancestor || is_current || is_tree_highlighted {
                base_text.font(font_id.clone())
            } else {
                base_text
            };
            let button = if is_tree_drop_target {
                egui::Button::new(label_text)
                    .fill(egui::Color32::from_rgb(80, 200, 80))
                    .frame(false)
            } else if is_tree_highlighted {
                egui::Button::new(label_text)
                    .fill(egui::Color32::from_rgb(180, 200, 255))
                    .frame(false)
            } else if is_current {
                egui::Button::new(label_text)
                    .fill(egui::Color32::from_rgb(100, 150, 255))
                    .frame(false)
            } else if is_ancestor {
                egui::Button::new(label_text)
                    .fill(egui::Color32::from_rgb(255, 200, 60))
                    .frame(false)
            } else {
                egui::Button::new(label_text)
                    .fill(egui::Color32::from_rgb(255, 245, 150))
                    .frame(false)
            };
            ui.add(button)
        },
    );

    // Detect drag-and-drop hover using raw rect (response.inner.hovered() is
    // suppressed while a mouse button is held).
    let is_valid_drop = dnd_active && !is_current && !dnd_sources.contains(path);
    if is_valid_drop {
        if let Some(pos) = ui.ctx().input(|i| i.pointer.hover_pos()) {
            if response.inner.rect.contains(pos) {
                *hovered_drop = Some(path.clone());
            }
        }
    }

    let was_secondary_clicked = response.inner.secondary_clicked();
    if response
        .inner
        .on_hover_text({
            let s = path.to_string_lossy().replace("\\", "/");
            s.trim_end_matches('/').to_string()
        })
        .clicked()
    {
        // Toggle expand / collapse
        if is_expanded {
            expanded.remove(path);
        } else {
            expanded.insert(path.clone());
            if !children_cache.contains_key(path) {
                let children = read_dir_children(path);
                children_cache.insert(path.clone(), children);
            }
        }
        // Only navigate if this isn't already the current folder —
        // navigate_to would re-expand everything and undo a collapse.
        if !is_current {
            *nav = Some(path.clone());
        }
    }

    // Right-click on a tree node → context menu
    if was_secondary_clicked {
        let pos = ui.input(|i| i.pointer.hover_pos().unwrap_or_default());
        *tree_right_clicked = Some((path.clone(), pos));
    }

    if is_expanded {
        if let Some(children) = children_cache.get(path).cloned() {
            for child in &children {
                render_tree_node(
                    ui,
                    child,
                    expanded,
                    children_cache,
                    nav,
                    current_path,
                    depth + 1,
                    dnd_active,
                    dnd_sources,
                    dnd_drop_target,
                    hovered_drop,
                    tree_right_clicked,
                    highlight_path,
                );
            }
        }
    }
}

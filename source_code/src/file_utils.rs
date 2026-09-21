//! Pure helper methods on `RusplorerApp` for file-type detection, name
//! formatting, extension colour-coding, and thumbnail layout.
//!
//! All functions are stateless / static (no `&self`) so they could be plain
//! free functions, but keeping them as associated functions on `RusplorerApp`
//! avoids changing any call site.

use eframe::egui;
use std::path::PathBuf;
use std::time::SystemTime;

use super::RusplorerApp;

impl RusplorerApp {
    pub(crate) fn is_code_file(path: &PathBuf) -> bool {
        if let Some(ext) = path.extension() {
            let ext_str = ext.to_string_lossy().to_lowercase();
            matches!(
                ext_str.as_str(),
                "rs" | "js"
                    | "ts"
                    | "jsx"
                    | "tsx"
                    | "py"
                    | "java"
                    | "c"
                    | "cpp"
                    | "h"
                    | "hpp"
                    | "cs"
                    | "go"
                    | "rb"
                    | "php"
                    | "html"
                    | "css"
                    | "scss"
                    | "json"
                    | "xml"
                    | "yaml"
                    | "yml"
                    | "toml"
                    | "md"
                    | "txt"
                    | "sh"
                    | "bat"
                    | "ps1"
                    | "sql"
                    | "vue"
                    | "svelte"
            )
        } else {
            false
        }
    }

    pub(crate) fn is_archive(path: &PathBuf) -> bool {
        if let Some(ext) = path.extension() {
            let ext_str = ext.to_string_lossy().to_lowercase();
            matches!(
                ext_str.as_str(),
                "7z" | "zip" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "iso"
            )
        } else {
            false
        }
    }

    /// Returns true if the file extension indicates an image that can be thumbnailed.
    pub(crate) fn is_image_file(name: &str) -> bool {
        matches!(
            name.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref(),
            Some("jpg" | "jpeg" | "png" | "bmp" | "gif" | "webp")
        )
    }

    /// Returns true if the file extension indicates a video.
    pub(crate) fn is_video_file(name: &str) -> bool {
        matches!(
            name.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref(),
            Some("mp4" | "avi" | "mkv" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "ogv")
        )
    }

    pub(crate) fn format_file_size(bytes: u64) -> String {
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

    /// Format a `SystemTime` as a local-time string.
    /// `tz_bias_secs` is the UTC offset (computed once per frame, not per row).
    pub(crate) fn format_modified_time(time: SystemTime, tz_bias_secs: i64) -> String {
        use std::time::UNIX_EPOCH;
        let Ok(dur) = time.duration_since(UNIX_EPOCH) else {
            return String::new();
        };

        let local_secs = dur.as_secs() as i64 - tz_bias_secs;
        if local_secs < 0 {
            return String::new();
        }
        let secs = local_secs as u64;

        let time_of_day = secs % 86400;
        let hour   = time_of_day / 3600;
        let minute = (time_of_day % 3600) / 60;

        // Euclidean algorithm for Gregorian calendar (Hinnant, public domain)
        let z = (secs / 86400) as i64 + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe/1460 + doe/36524 - doe/146096) / 365;
        let y   = yoe + era * 400;
        let doy = doe - (365*yoe + yoe/4 - yoe/100);
        let mp  = (5*doy + 2) / 153;
        let d   = doy - (153*mp + 2)/5 + 1;
        let m   = if mp < 10 { mp + 3 } else { mp - 9 };
        let y   = if m <= 2 { y + 1 } else { y };

        format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, hour, minute)
    }

    /// Returns a color for a file extension group, adapted to light vs dark mode.
    /// Returns `None` for unknown / unclassified extensions (use default text color).
    pub(crate) fn ext_color(ext_lower: &str, dark_mode: bool) -> Option<egui::Color32> {
        // Color pairs: (dark-mode, light-mode)
        macro_rules! col {
            ($rd:expr,$gd:expr,$bd:expr, $rl:expr,$gl:expr,$bl:expr) => {
                if dark_mode {
                    egui::Color32::from_rgb($rd, $gd, $bd)
                } else {
                    egui::Color32::from_rgb($rl, $gl, $bl)
                }
            };
        }
        match ext_lower {
            // ── Executables & scripts ──────────────────────────────────────────
            ".exe" | ".msi" | ".com" | ".appx" | ".msix" =>
                Some(col!(100, 160, 255,  30,  80, 200)),   // blue
            ".bat" | ".cmd" | ".ps1" | ".psm1" | ".psd1" | ".vbs" | ".wsf" | ".sh" =>
                Some(col!(130, 180, 255,  50, 100, 200)),   // softer blue
            // ── Libraries & system ────────────────────────────────────────────
            ".dll" | ".sys" | ".ocx" | ".drv" | ".ax" =>
                Some(col!(160, 210, 255,  60, 130, 210)),   // light blue
            // ── Videos ───────────────────────────────────────────────────────
            ".mp4" | ".mkv" | ".avi" | ".mov" | ".wmv" | ".flv"
            | ".webm" | ".m4v" | ".m2ts" | ".vob" | ".mpg" | ".mpeg" | ".f4v" | ".3gp" | ".ogv" =>
                Some(col!( 80, 200,  90,  20, 130,  40)),   // green
            // ── Audio ────────────────────────────────────────────────────────
            ".mp3" | ".wav" | ".flac" | ".aac" | ".ogg" | ".wma" | ".m4a"
            | ".opus" | ".aiff" | ".ape" =>
                Some(col!( 50, 200, 200,   0, 130, 140)),   // teal / cyan
            // ── Images ───────────────────────────────────────────────────────
            ".jpg" | ".jpeg" | ".png" | ".bmp" | ".gif" | ".webp"
            | ".svg" | ".ico" | ".tiff" | ".tif" | ".heic" | ".avif" | ".raw"
            | ".cr2" | ".nef" | ".arw" =>
                Some(col!(255, 165,  50,  190,  90,   0)),  // amber / orange
            // ── Archives ─────────────────────────────────────────────────────
            ".zip" | ".7z" | ".rar" | ".tar" | ".gz" | ".bz2" | ".xz"
            | ".zst" | ".cab" | ".iso" | ".img" | ".dmg" | ".tgz" | ".lz4" =>
                Some(col!(200, 100, 230,  130,  30, 170)),  // purple
            // ── Documents ────────────────────────────────────────────────────
            ".pdf" =>
                Some(col!(255,  90,  80,  190,  30,  20)),  // red
            ".doc" | ".docx" | ".odt" | ".rtf" =>
                Some(col!(130, 200, 255,  30, 100, 200)),   // Word blue
            ".xls" | ".xlsx" | ".ods" | ".csv" =>
                Some(col!( 80, 210, 120,  20, 140,  60)),   // Excel green
            ".ppt" | ".pptx" | ".odp" =>
                Some(col!(255, 140,  80,  200,  70,  10)),  // PowerPoint orange
            ".txt" | ".md" | ".rst" | ".log" | ".nfo" =>
                Some(col!(180, 180, 180,  100, 100, 100)),  // gray
            // ── Code & markup ─────────────────────────────────────────────────
            ".rs" | ".c" | ".cpp" | ".cc" | ".cxx" | ".h" | ".hpp"
            | ".cs" | ".java" | ".go" | ".swift" | ".kt" | ".kts"
            | ".py" | ".rb" | ".php" | ".pl" | ".lua"
            | ".js" | ".ts" | ".jsx" | ".tsx" | ".vue" | ".svelte"
            | ".html" | ".htm" | ".css" | ".scss" | ".sass" | ".less"
            | ".sql" | ".r" | ".m" | ".f" | ".f90" | ".zig" | ".nim"
            | ".dart" | ".ex" | ".exs" | ".clj" | ".cljs" | ".scala"
            | ".elm" | ".erl" | ".hrl" | ".hs" | ".lhs" =>
                Some(col!(255, 140,  60,  160,  70,   0)),  // code orange
            // ── Config / data ─────────────────────────────────────────────────
            ".json" | ".toml" | ".yaml" | ".yml" | ".xml" | ".ini"
            | ".cfg" | ".conf" | ".env" | ".properties" | ".plist"
            | ".reg" | ".desktop" | ".service" =>
                Some(col!(160, 220, 160,  40, 130,  40)),   // muted green
            // ── Fonts ─────────────────────────────────────────────────────────
            ".ttf" | ".otf" | ".woff" | ".woff2" | ".eot" =>
                Some(col!(230, 160, 230,  150,  40, 150)),  // magenta
            // ── Everything else → no color override ───────────────────────────
            _ => None,
        }
    }

    /// Build a `LayoutJob` for a file-list entry name.
    /// For files (non-directory), the extension is rendered in **bold** with a
    /// type-based color; the stem uses the regular proportional font.
    /// Directories and the parent "[..]" entry are rendered as plain text.
    /// Truncate `name` so it fits within `max_px` pixels at font size 11px.
    /// If truncation is needed, ellipsis is inserted before the extension:
    ///   "long file name.mp4" → "long fil….mp4"
    pub(crate) fn truncate_name(name: &str, max_px: f32, ui: &egui::Ui) -> String {
        let font_id = egui::FontId::new(11.0, egui::FontFamily::Proportional);
        let measure = |s: &str| -> f32 {
            ui.ctx().fonts(|f| {
                f.layout_no_wrap(s.to_string(), font_id.clone(), egui::Color32::WHITE).size().x
            })
        };
        if measure(name) <= max_px {
            return name.to_string();
        }
        // Split stem and extension.
        let (stem, ext) = if name.starts_with("[..]") {
            (name, "")
        } else {
            match name.rfind('.').filter(|&p| p > 0) {
                Some(p) => (&name[..p], &name[p..]),
                None    => (name, ""),
            }
        };
        let ellipsis = "…";
        // Remove chars from the end of stem until it fits.
        let mut truncated = stem.to_string();
        loop {
            let candidate = format!("{truncated}{ellipsis}{ext}");
            if measure(&candidate) <= max_px || truncated.is_empty() {
                return candidate;
            }
            // Pop one char (handle multi-byte correctly).
            truncated.pop();
        }
    }

    pub(crate) fn name_layout_job(
        name: &str,
        is_dir: bool,
        color: egui::Color32,
        italics: bool,
        dark_mode: bool,
    ) -> egui::text::LayoutJob {
        const SIZE: f32 = 11.0;
        let regular = egui::FontId::new(SIZE, egui::FontFamily::Proportional);
        let bold    = egui::FontId::new(SIZE, egui::FontFamily::Name("Bold".into()));
        let fmt_reg = egui::text::TextFormat { font_id: regular.clone(), color, italics, ..Default::default() };

        let mut job = egui::text::LayoutJob::default();

        let is_parent = name.starts_with("[..]");
        if is_dir || is_parent {
            job.append(name, 0.0, fmt_reg);
        } else {
            // Find the last dot that is not at position 0 (hidden-file "." prefix)
            let dot_pos = name.rfind('.').filter(|&p| p > 0);
            match dot_pos {
                Some(p) => {
                    let ext_lower = name[p..].to_lowercase();
                    // Use type color only when the entry is not highlighted (selected/clipboard).
                    // When it IS highlighted the background is already colored, so use
                    // the caller-supplied color (usually WHITE) for readability.
                    let ext_color = if color == egui::Color32::WHITE {
                        color // highlighted — keep white for contrast
                    } else {
                        Self::ext_color(&ext_lower, dark_mode).unwrap_or(color)
                    };
                    let fmt_stem = egui::text::TextFormat { font_id: regular.clone(), color: ext_color, italics, ..Default::default() };
                    let fmt_bold = egui::text::TextFormat { font_id: bold, color: ext_color, italics, ..Default::default() };
                    job.append(&name[..p], 0.0, fmt_stem);
                    job.append(&name[p..], 0.0, fmt_bold);
                }
                None => {
                    job.append(name, 0.0, fmt_reg);
                }
            }
        }
        job
    }

    /// Returns a background color for the date column based on how old the file is.
    /// Violet = timestamp is in the future (invalid/dashcam clock drift).
    /// Light green (very recent) → darker green → light orange → orange (>1 week).
    pub(crate) fn age_color(modified: SystemTime, now: SystemTime) -> egui::Color32 {
        // Future timestamp — device clock is wrong (e.g. dashcam with bad RTC).
        if modified > now {
            return egui::Color32::from_rgb(140, 80, 200);
        }
        let age = now
            .duration_since(modified)
            .map(|d| d.as_secs_f64())
            .unwrap_or(f64::MAX);

        // (age_threshold_secs, r, g, b)
        const STOPS: &[(f64, u8, u8, u8)] = &[
            (0.0,          200, 240, 200),  // light green  — just now
            (300.0,        150, 218, 150),  // green        — 5 min
            (3_600.0,       90, 180,  90),  // medium green — 1 hour
            (86_400.0,     180, 210, 130),  // yellow-green — 1 day
            (604_800.0,    255, 200, 140),  // light orange — 1 week
        ];
        const ORANGE: egui::Color32 = egui::Color32::from_rgb(255, 175, 100);

        if age >= 604_800.0 {
            return ORANGE;
        }
        for w in STOPS.windows(2) {
            let (t0, r0, g0, b0) = w[0];
            let (t1, r1, g1, b1) = w[1];
            if age <= t1 {
                let t = ((age - t0) / (t1 - t0)) as f32;
                let lerp = |a: u8, b: u8| (a as f32 + t * (b as f32 - a as f32)).round() as u8;
                return egui::Color32::from_rgb(lerp(r0, r1), lerp(g0, g1), lerp(b0, b1));
            }
        }
        ORANGE
    }
}

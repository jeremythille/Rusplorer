#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Thin wrapper: all app logic is in lib.rs for fast incremental builds.
// This file only contains panic hooks and the main() entry point.
// Changes here trigger a fast relink, not full recompilation.

fn main() -> Result<(), eframe::Error> {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("PANIC: {}\n", info);
        let log_path = std::env::current_exe()
            .ok()
            .map(|p| p.with_file_name("rusplorer_error.log"))
            .unwrap_or_else(|| std::path::PathBuf::from("rusplorer_error.log"));
        let _ = std::fs::write(&log_path, &msg);
        #[cfg(windows)]
        unsafe {
            use std::ffi::CString;
            let title = CString::new("Rusplorer crashed").unwrap_or_default();
            let body = CString::new(format!("Rusplorer encountered a fatal error.\nDetails:\n{}", log_path.display())).unwrap_or_default();
            winapi::um::winuser::MessageBoxA(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), winapi::um::winuser::MB_OK | winapi::um::winuser::MB_ICONERROR);
        }
    }));

    let result = rusplorer::run_app();
    if let Err(ref e) = result {
        let msg = format!("eframe error: {:?}\n", e);
        let log_path = std::env::current_exe()
            .ok()
            .map(|p| p.with_file_name("rusplorer_error.log"))
            .unwrap_or_else(|| std::path::PathBuf::from("rusplorer_error.log"));
        let _ = std::fs::write(&log_path, &msg);
        #[cfg(windows)]
        unsafe {
            use std::ffi::CString;
            let title = CString::new("Rusplorer failed to start").unwrap_or_default();
            let body = CString::new(format!("Could not initialize graphics.\nDetails:\n{}", log_path.display())).unwrap_or_default();
            winapi::um::winuser::MessageBoxA(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), winapi::um::winuser::MB_OK | winapi::um::winuser::MB_ICONERROR);
        }
    }
    result
}

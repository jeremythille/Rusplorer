/// Windows clipboard helpers — copy/cut files to/from the system clipboard.

use std::path::PathBuf;

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use winapi::um::shellapi::DragQueryFileW;
#[cfg(windows)]
use winapi::um::winuser::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
    OpenClipboard, SetClipboardData,
};

/// Copy files to Windows clipboard in HDROP format so they can be pasted in Explorer.
/// When `is_cut` is true, also sets the Preferred DropEffect to MOVE so that other
/// applications (including other Rusplorer instances) know this is a cut operation.
#[cfg(windows)]
pub fn copy_files_to_clipboard(files: &[PathBuf], is_cut: bool) -> Result<(), Box<dyn std::error::Error>> {
    use winapi::um::winuser::CF_HDROP;

    // DROPFILES structure: 20 bytes total
    // offset 0:  pFiles (DWORD) - offset to file list = 20
    // offset 4:  pt.x (LONG)
    // offset 8:  pt.y (LONG)
    // offset 12: fNC (BOOL)
    // offset 16: fWide (BOOL) - must be 1 for Unicode

    // Build the wide-char file list: each path null-terminated, double-null at end
    let mut wide_chars: Vec<u16> = Vec::new();
    for file in files {
        let path_str = file.to_string_lossy();
        let wide: Vec<u16> = OsStr::new(path_str.as_ref())
            .encode_wide()
            .chain(std::iter::once(0u16))
            .collect();
        wide_chars.extend_from_slice(&wide);
    }
    wide_chars.push(0u16); // Final double-null terminator

    let dropfiles_size: usize = 20; // sizeof(DROPFILES)
    let file_data_size = wide_chars.len() * 2; // bytes for wide chars
    let total_size = dropfiles_size + file_data_size;

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("Failed to open clipboard".into());
        }

        if EmptyClipboard() == 0 {
            CloseClipboard();
            return Err("Failed to empty clipboard".into());
        }

        let hglobal = winapi::um::winbase::GlobalAlloc(
            winapi::um::winbase::GMEM_MOVEABLE | winapi::um::winbase::GMEM_ZEROINIT,
            total_size,
        );
        if hglobal.is_null() {
            CloseClipboard();
            return Err("Failed to allocate global memory".into());
        }

        let ptr = winapi::um::winbase::GlobalLock(hglobal) as *mut u8;
        if ptr.is_null() {
            winapi::um::winbase::GlobalFree(hglobal);
            CloseClipboard();
            return Err("Failed to lock global memory".into());
        }

        // Write DROPFILES structure
        let pfiles: u32 = 20;
        std::ptr::copy_nonoverlapping(&pfiles as *const u32 as *const u8, ptr, 4);
        let fwide: u32 = 1;
        std::ptr::copy_nonoverlapping(&fwide as *const u32 as *const u8, ptr.add(16), 4);

        // Write file paths after DROPFILES structure
        std::ptr::copy_nonoverlapping(
            wide_chars.as_ptr() as *const u8,
            ptr.add(dropfiles_size),
            file_data_size,
        );

        winapi::um::winbase::GlobalUnlock(hglobal);

        if SetClipboardData(CF_HDROP, hglobal as *mut winapi::ctypes::c_void).is_null() {
            winapi::um::winbase::GlobalFree(hglobal);
            CloseClipboard();
            return Err("Failed to set clipboard data".into());
        }

        // Write Preferred DropEffect so other processes know cut vs copy.
        // DROPEFFECT_MOVE = 2, DROPEFFECT_COPY = 1
        let drop_effect_name: Vec<u16> = OsStr::new("Preferred DropEffect")
            .encode_wide()
            .chain(std::iter::once(0u16))
            .collect();
        let cf_drop_effect = winapi::um::winuser::RegisterClipboardFormatW(drop_effect_name.as_ptr());
        if cf_drop_effect != 0 {
            let hmem = winapi::um::winbase::GlobalAlloc(
                winapi::um::winbase::GMEM_MOVEABLE | winapi::um::winbase::GMEM_ZEROINIT,
                4,
            );
            if !hmem.is_null() {
                let p = winapi::um::winbase::GlobalLock(hmem) as *mut u32;
                if !p.is_null() {
                    *p = if is_cut { 2u32 } else { 1u32 }; // DROPEFFECT_MOVE / COPY
                    winapi::um::winbase::GlobalUnlock(hmem);
                    SetClipboardData(cf_drop_effect, hmem as *mut winapi::ctypes::c_void);
                }
            }
        }

        CloseClipboard();
    }

    Ok(())
}

/// Read the Preferred DropEffect from the Windows clipboard.
/// Returns true if the clipboard indicates a CUT (move) operation.
/// Must be called while the clipboard is NOT already open.
#[cfg(windows)]
pub fn read_clipboard_drop_effect_is_cut() -> bool {
    unsafe {
        let drop_effect_name: Vec<u16> = OsStr::new("Preferred DropEffect")
            .encode_wide()
            .chain(std::iter::once(0u16))
            .collect();
        let cf_drop_effect = winapi::um::winuser::RegisterClipboardFormatW(drop_effect_name.as_ptr());
        if cf_drop_effect == 0 {
            return false;
        }
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        let result = {
            let hdata = GetClipboardData(cf_drop_effect);
            if hdata.is_null() {
                false
            } else {
                let p = winapi::um::winbase::GlobalLock(hdata as *mut winapi::ctypes::c_void) as *const u32;
                let is_move = if !p.is_null() {
                    (*p & 2) != 0 // DROPEFFECT_MOVE = 2
                } else {
                    false
                };
                winapi::um::winbase::GlobalUnlock(hdata as *mut winapi::ctypes::c_void);
                is_move
            }
        };
        CloseClipboard();
        result
    }
}

/// Read files from Windows clipboard in HDROP format.
#[cfg(windows)]
pub fn read_files_from_clipboard() -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    use winapi::um::winuser::CF_HDROP;

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("Failed to open clipboard".into());
        }

        // Check if clipboard has HDROP format
        if IsClipboardFormatAvailable(CF_HDROP) == 0 {
            CloseClipboard();
            return Ok(Vec::new()); // No files in clipboard
        }

        let hglobal = GetClipboardData(CF_HDROP);
        if hglobal.is_null() {
            CloseClipboard();
            return Err("Failed to get clipboard data".into());
        }

        // Query the number of files
        let file_count = DragQueryFileW(hglobal as *mut _, 0xFFFFFFFF, std::ptr::null_mut(), 0);

        let mut files = Vec::new();
        for i in 0..file_count {
            let path_len = DragQueryFileW(hglobal as *mut _, i, std::ptr::null_mut(), 0);

            let mut buffer: Vec<u16> = vec![0; (path_len + 1) as usize];
            DragQueryFileW(
                hglobal as *mut _,
                i,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            );

            let path_str = String::from_utf16_lossy(&buffer[..path_len as usize]);
            files.push(PathBuf::from(path_str));
        }

        CloseClipboard();
        Ok(files)
    }
}

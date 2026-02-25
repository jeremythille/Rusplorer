#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use arboard::Clipboard;
use eframe::egui;
use egui_extras::{Column, TableBuilder};
use notify::recommended_watcher;
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::SystemTime;

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use winapi::um::shellapi::{
    DragQueryFileW, FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, SHFILEOPSTRUCTW, SHFileOperationW,
};
#[cfg(windows)]
use winapi::um::winuser::{
    CloseClipboard, EmptyClipboard, GetAsyncKeyState, GetClipboardData, IsClipboardFormatAvailable,
    OpenClipboard, SetClipboardData,
};

/// Copy files to Windows clipboard in HDROP format so they can be pasted in Explorer
#[cfg(windows)]
fn copy_files_to_clipboard(files: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
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
        // pFiles = 20 (offset to file data)
        let pfiles: u32 = 20;
        std::ptr::copy_nonoverlapping(&pfiles as *const u32 as *const u8, ptr, 4);
        // pt.x = 0 (offset 4, already zeroed)
        // pt.y = 0 (offset 8, already zeroed)
        // fNC = 0  (offset 12, already zeroed)
        // fWide = 1 (offset 16)
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

        CloseClipboard();
    }

    Ok(())
}

/// Read files from Windows clipboard in HDROP format
#[cfg(windows)]
fn read_files_from_clipboard() -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
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
            // Get the length of the file path
            let path_len = DragQueryFileW(hglobal as *mut _, i, std::ptr::null_mut(), 0);

            // Allocate buffer and get the file path
            let mut buffer: Vec<u16> = vec![0; (path_len + 1) as usize];
            DragQueryFileW(
                hglobal as *mut _,
                i,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            );

            // Convert to PathBuf
            let path_str = String::from_utf16_lossy(&buffer[..path_len as usize]);
            files.push(PathBuf::from(path_str));
        }

        CloseClipboard();
        Ok(files)
    }
}

/// Initiate an OLE drag-and-drop of the given files out to other applications (e.g. Explorer).
/// Blocks until the user drops or cancels.  Returns `true` when the target performed a *move*
/// (so we should refresh our listing).
/// `right_button`: if true, tracks MK_RBUTTON instead of MK_LBUTTON.
#[cfg(windows)]
fn ole_drag_files_out(files: &[PathBuf], right_button: bool) -> bool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{implement, HRESULT};
    use windows::Win32::Foundation::{BOOL, E_NOTIMPL, S_OK};
    use windows::Win32::System::Com::{
        IDataObject, IDataObject_Impl, FORMATETC, STGMEDIUM,
        TYMED_HGLOBAL,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GLOBAL_ALLOC_FLAGS,
    };
    use windows::Win32::System::Ole::{
        DoDragDrop, IDropSource, IDropSource_Impl, DROPEFFECT, DROPEFFECT_COPY,
        DROPEFFECT_MOVE, DROPEFFECT_NONE,
    };
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;

    const CF_HDROP_RAW: u16 = 15;
    const MK_LBUTTON: u32 = 0x0001;
    const MK_RBUTTON: u32 = 0x0002;
    const DRAGDROP_S_DROP: HRESULT = HRESULT(0x00040100_i32);
    const DRAGDROP_S_CANCEL: HRESULT = HRESULT(0x00040101_i32);
    const DRAGDROP_S_USEDEFAULTCURSORS: HRESULT = HRESULT(0x00040102_i32);

    let track_button: u32 = if right_button { MK_RBUTTON } else { MK_LBUTTON };

    // ── IDropSource ──────────────────────────────────────────────────────
    #[implement(IDropSource)]
    struct DropSource {
        button_mask: u32,
    }

    impl IDropSource_Impl for DropSource_Impl {
        fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
            if fescapepressed.as_bool() {
                DRAGDROP_S_CANCEL
            } else if grfkeystate.0 & self.button_mask == 0 {
                DRAGDROP_S_DROP
            } else {
                S_OK
            }
        }
        fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    // ── IDataObject (CF_HDROP only) ──────────────────────────────────────
    #[implement(IDataObject)]
    struct HdropData {
        blob: Vec<u8>,
    }

    impl IDataObject_Impl for HdropData_Impl {
        fn GetData(
            &self,
            pformatetcin: *const FORMATETC,
        ) -> windows::core::Result<STGMEDIUM> {
            unsafe {
                let fmt = &*pformatetcin;
                if fmt.cfFormat != CF_HDROP_RAW {
                    return Err(windows::core::Error::from_hresult(E_NOTIMPL));
                }
                let hmem = GlobalAlloc(
                    GLOBAL_ALLOC_FLAGS(0x0042), // GMEM_MOVEABLE | GMEM_ZEROINIT
                    self.blob.len(),
                )?;
                let ptr = GlobalLock(hmem) as *mut u8;
                if ptr.is_null() {
                    return Err(windows::core::Error::from_hresult(E_NOTIMPL));
                }
                std::ptr::copy_nonoverlapping(self.blob.as_ptr(), ptr, self.blob.len());
                let _ = GlobalUnlock(hmem);
                let mut medium: STGMEDIUM = std::mem::zeroed();
                medium.tymed = TYMED_HGLOBAL.0 as u32;
                medium.u.hGlobal = hmem;
                Ok(medium)
            }
        }
        fn GetDataHere(
            &self,
            _: *const FORMATETC,
            _: *mut STGMEDIUM,
        ) -> windows::core::Result<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            unsafe {
                if (*pformatetc).cfFormat == CF_HDROP_RAW {
                    S_OK
                } else {
                    HRESULT(0x80040064_u32 as i32) // DV_E_FORMATETC
                }
            }
        }
        fn GetCanonicalFormatEtc(
            &self,
            _: *const FORMATETC,
            _: *mut FORMATETC,
        ) -> HRESULT {
            E_NOTIMPL
        }
        fn SetData(
            &self,
            _: *const FORMATETC,
            _: *const STGMEDIUM,
            _: BOOL,
        ) -> windows::core::Result<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn EnumFormatEtc(
            &self,
            dwdirection: u32,
        ) -> windows::core::Result<windows::Win32::System::Com::IEnumFORMATETC> {
            use windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc;
            if dwdirection != 1 { // DATADIR_GET
                return Err(windows::core::Error::from_hresult(E_NOTIMPL));
            }
            let fmt = FORMATETC {
                cfFormat: CF_HDROP_RAW,
                ptd: std::ptr::null_mut(),
                dwAspect: 1, // DVASPECT_CONTENT
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            };
            unsafe { SHCreateStdEnumFmtEtc(&[fmt]) }
        }
        fn DAdvise(
            &self,
            _: *const FORMATETC,
            _: u32,
            _: Option<&windows::Win32::System::Com::IAdviseSink>,
        ) -> windows::core::Result<u32> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn EnumDAdvise(
            &self,
        ) -> windows::core::Result<windows::Win32::System::Com::IEnumSTATDATA> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
    }

    // ── Build HDROP blob ─────────────────────────────────────────────────
    let mut wide_chars: Vec<u16> = Vec::new();
    for file in files {
        let wide: Vec<u16> = OsStr::new(file.as_os_str())
            .encode_wide()
            .chain(std::iter::once(0u16))
            .collect();
        wide_chars.extend_from_slice(&wide);
    }
    wide_chars.push(0u16); // double-null terminator

    let dropfiles_size: usize = 20;
    let file_data_size = wide_chars.len() * 2;
    let total_size = dropfiles_size + file_data_size;

    let mut blob = vec![0u8; total_size];
    blob[0..4].copy_from_slice(&20u32.to_le_bytes());   // pFiles
    blob[16..20].copy_from_slice(&1u32.to_le_bytes());  // fWide
    unsafe {
        std::ptr::copy_nonoverlapping(
            wide_chars.as_ptr() as *const u8,
            blob.as_mut_ptr().add(dropfiles_size),
            file_data_size,
        );
    }

    // ── Perform OLE drag ─────────────────────────────────────────────────
    let data_obj: IDataObject = HdropData { blob }.into();
    let source: IDropSource = DropSource { button_mask: track_button }.into();
    let mut effect = DROPEFFECT_NONE;
    let hr = unsafe {
        DoDragDrop(
            &data_obj,
            &source,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
            &mut effect,
        )
    };
    hr == DRAGDROP_S_DROP && effect == DROPEFFECT_MOVE
}

/// Create a Windows .lnk shortcut pointing at `target` inside `dest_dir`.
#[cfg(windows)]
fn create_lnk_shortcut(target: &PathBuf, dest_dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::Interface;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::PCWSTR;

    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "shortcut".to_string());
    let lnk_path = dest_dir.join(format!("{}.lnk", stem));

    let target_wide: Vec<u16> = OsStr::new(target).encode_wide().chain(std::iter::once(0)).collect();
    let lnk_wide: Vec<u16> = OsStr::new(&lnk_path).encode_wide().chain(std::iter::once(0)).collect();

    unsafe {
        let coin_hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let shell_link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell_link.SetPath(PCWSTR(target_wide.as_ptr()))?;
            let persist_file: IPersistFile = shell_link.cast()?;
            persist_file.Save(PCWSTR(lnk_wide.as_ptr()), true)?;
            Ok(())
        })();
        if coin_hr.is_ok() {
            CoUninitialize();
        }
        result
    }
}

/// Resolve a Windows .lnk shortcut file to its target path
#[cfg(windows)]
fn resolve_lnk(path: &Path) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::Interface;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, STGM,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::PCWSTR;

    let wide_path: Vec<u16> = OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let coin_hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| -> Option<PathBuf> {
            let shell_link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
            let persist_file: IPersistFile = shell_link.cast().ok()?;
            persist_file
                .Load(PCWSTR(wide_path.as_ptr()), STGM(0))
                .ok()?;
            let mut buf = [0u16; 261];
            // SLGP_RAWPATH = 0x4
            shell_link
                .GetPath(
                    &mut buf,
                    std::ptr::null_mut(),
                    0x4u32,
                )
                .ok()?;
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let target = String::from_utf16_lossy(&buf[..len]);
            if target.is_empty() { None } else { Some(PathBuf::from(target)) }
        })();
        if coin_hr.is_ok() {
            CoUninitialize();
        }
        result
    }
}

/// Recursively calculate directory size, sending updates progressively
fn calculate_dir_size_progressive(
    path: &Path,
    root_path: &Path,
    cancel_token: &Arc<AtomicBool>,
    pause_token: &Arc<AtomicBool>,
    tx: &std::sync::mpsc::Sender<(PathBuf, u64)>,
    accumulated: &mut u64,
) -> bool {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => {
            // Permission denied or other error - send accumulated size so far
            let _ = tx.send((root_path.to_path_buf(), *accumulated));
            return false;
        }
    };

    for entry in entries.filter_map(|e| e.ok()) {
        // Check cancellation every iteration
        if cancel_token.load(Ordering::Relaxed) {
            return false;
        }
        // Check pause and sleep if paused
        while pause_token.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if cancel_token.load(Ordering::Relaxed) {
                return false;
            }
        }

        let entry_path = entry.path();
        if entry_path.is_dir() {
            calculate_dir_size_progressive(
                &entry_path,
                root_path,
                cancel_token,
                pause_token,
                tx,
                accumulated,
            );
        } else if let Ok(metadata) = entry.metadata() {
            *accumulated += metadata.len();
            // Send update every time we add file size
            let _ = tx.send((root_path.to_path_buf(), *accumulated));
        }
    }
    true
}

/// Parse a Windows GUID string like "{DA9C62FD-3F94-400B-87B5-A43B9EB6C70D}" into a GUID struct.
#[cfg(windows)]
fn parse_guid(s: &str) -> Option<windows::core::GUID> {
    let s = s.trim_matches(|c| c == '{' || c == '}');
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 { return None; }
    let data1 = u32::from_str_radix(parts[0], 16).ok()?;
    let data2 = u16::from_str_radix(parts[1], 16).ok()?;
    let data3 = u16::from_str_radix(parts[2], 16).ok()?;
    let d3    = u16::from_str_radix(parts[3], 16).ok()?;
    let d4    = u64::from_str_radix(parts[4], 16).ok()?;
    Some(windows::core::GUID {
        data1, data2, data3,
        data4: [
            (d3 >> 8) as u8, (d3 & 0xFF) as u8,
            ((d4 >> 40) & 0xFF) as u8, ((d4 >> 32) & 0xFF) as u8,
            ((d4 >> 24) & 0xFF) as u8, ((d4 >> 16) & 0xFF) as u8,
            ((d4 >> 8)  & 0xFF) as u8, ( d4        & 0xFF) as u8,
        ],
    })
}

/// Look up the registry for a virtual desktop named "Rusplorer" and return its GUID.
#[cfg(windows)]
fn find_rusplorer_desktop_guid() -> Option<windows::core::GUID> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let desktops = hkcu
        .open_subkey(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\VirtualDesktops\Desktops")
        .ok()?;
    for key_name in desktops.enum_keys().flatten() {
        if let Ok(subkey) = desktops.open_subkey(&key_name) {
            let name: String = subkey.get_value("Name").unwrap_or_default();
            if name == "Rusplorer" {
                return parse_guid(&key_name);
            }
        }
    }
    None
}

/// Move own window to the "Rusplorer" virtual desktop.
/// Uses the public IVirtualDesktopManager COM API — works in-process (no E_ACCESSDENIED).
/// Returns true if the move succeeded OR if no "Rusplorer" desktop exists (no point retrying).
#[cfg(windows)]
fn try_move_to_rusplorer_desktop() -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize,
        CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::IVirtualDesktopManager;

    let desktop_guid = match find_rusplorer_desktop_guid() {
        Some(g) => g,
        None => return true, // No "Rusplorer" desktop — nothing to do, stop retrying
    };

    let wide_title: Vec<u16> = OsStr::new("Rusplorer")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let hwnd_raw = winapi::um::winuser::FindWindowW(std::ptr::null(), wide_title.as_ptr());
        if hwnd_raw.is_null() {
            return false; // Window not visible yet — retry later
        }
        let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);

        let coin_hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // CLSID_VirtualDesktopManager = {AA509086-5CA9-4C25-8F95-589D3C07B48A}
        const CLSID_VDM: windows::core::GUID = windows::core::GUID {
            data1: 0xAA509086,
            data2: 0x5CA9,
            data3: 0x4C25,
            data4: [0x8F, 0x95, 0x58, 0x9D, 0x3C, 0x07, 0xB4, 0x8A],
        };

        let result = (|| -> Option<bool> {
            let mgr: IVirtualDesktopManager =
                CoCreateInstance(&CLSID_VDM, None, CLSCTX_LOCAL_SERVER).ok()?;
            mgr.MoveWindowToDesktop(hwnd, &desktop_guid).ok()?;
            Some(true)
        })()
        .unwrap_or(false);

        if coin_hr.is_ok() {
            CoUninitialize();
        }
        result
    }
}

/// Returns the IDropTarget COM object (must be kept alive for the duration of the session).
/// Returns None if registration failed.
#[cfg(windows)]
fn register_ole_drop_target(
    hwnd_raw: *mut std::ffi::c_void,
    sender: std::sync::mpsc::Sender<Vec<PathBuf>>,
    right_click_sender: std::sync::mpsc::Sender<Vec<PathBuf>>,
) -> Option<windows::Win32::System::Ole::IDropTarget> {
    use windows::core::implement;
    use windows::Win32::Foundation::{HWND, POINTL, S_OK};
    use windows::Win32::System::Com::{IDataObject, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::{
        IDropTarget, IDropTarget_Impl, RegisterDragDrop, RevokeDragDrop,
        DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
    };
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;

    const CF_HDROP_RAW: u16 = 15;
    const MK_RBUTTON: u32 = 0x0002;

    #[implement(IDropTarget)]
    struct DropTarget {
        sender: std::sync::mpsc::Sender<Vec<PathBuf>>,
        right_click_sender: std::sync::mpsc::Sender<Vec<PathBuf>>,
        last_key_state: std::cell::Cell<u32>,
    }

    impl IDropTarget_Impl for DropTarget_Impl {
        fn DragEnter(
            &self,
            pdataobj: Option<&IDataObject>,
            grfkeystate: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            self.last_key_state.set(grfkeystate.0);
            unsafe {
                let ok = if let Some(obj) = pdataobj {
                    let fmt = FORMATETC {
                        cfFormat: CF_HDROP_RAW,
                        ptd: std::ptr::null_mut(),
                        dwAspect: 1,
                        lindex: -1,
                        tymed: TYMED_HGLOBAL.0 as u32,
                    };
                    obj.QueryGetData(&fmt) == S_OK
                } else {
                    false
                };
                *pdweffect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
            }
            Ok(())
        }

        fn DragOver(
            &self,
            grfkeystate: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            self.last_key_state.set(grfkeystate.0);
            unsafe { *pdweffect = DROPEFFECT_COPY; }
            Ok(())
        }

        fn DragLeave(&self) -> windows::core::Result<()> {
            Ok(())
        }

        fn Drop(
            &self,
            pdataobj: Option<&IDataObject>,
            _grfkeystate: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            unsafe {
                *pdweffect = DROPEFFECT_NONE;
                let obj = match pdataobj { Some(o) => o, None => return Ok(()) };
                let fmt = FORMATETC {
                    cfFormat: CF_HDROP_RAW,
                    ptd: std::ptr::null_mut(),
                    dwAspect: 1,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                };
                let medium = match obj.GetData(&fmt) { Ok(m) => m, Err(_) => return Ok(()) };
                let hmem = medium.u.hGlobal;
                let locked = GlobalLock(hmem) as *const u8;
                if locked.is_null() { return Ok(()); }

                // Parse DROPFILES: pFiles (u32) at offset 0, fWide (u32) at offset 16
                let pfiles = std::ptr::read_unaligned(locked as *const u32) as usize;
                let fwide  = std::ptr::read_unaligned(locked.add(16) as *const u32);
                let mut files: Vec<PathBuf> = Vec::new();
                if fwide != 0 {
                    let mut ptr = locked.add(pfiles) as *const u16;
                    loop {
                        let start = ptr;
                        let mut len = 0usize;
                        while *ptr != 0 { ptr = ptr.add(1); len += 1; }
                        if len == 0 { break; }
                        let s = String::from_utf16_lossy(
                            std::slice::from_raw_parts(start, len));
                        files.push(PathBuf::from(s));
                        ptr = ptr.add(1);
                    }
                }
                let _ = GlobalUnlock(hmem);

                if !files.is_empty() {
                    // If right-button was held during drag, send to right_click channel
                    if self.last_key_state.get() & MK_RBUTTON != 0 {
                        let _ = self.right_click_sender.send(files);
                    } else {
                        let _ = self.sender.send(files);
                    }
                    *pdweffect = DROPEFFECT_COPY;
                }
            }
            Ok(())
        }
    }

    let drop_target: IDropTarget = DropTarget {
        sender,
        right_click_sender,
        last_key_state: std::cell::Cell::new(0),
    }.into();
    unsafe {
        let hwnd = HWND(hwnd_raw);
        // Remove any existing drop target (winit registers its own)
        let _ = RevokeDragDrop(hwnd);
        if RegisterDragDrop(hwnd, &drop_target).is_ok() {
            Some(drop_target)
        } else {
            None
        }
    }
}

fn main() -> Result<(), eframe::Error> {
    // Initialise OLE on the main thread so DoDragDrop works
    #[cfg(windows)]
    unsafe {
        let _ = windows::Win32::System::Ole::OleInitialize(None);
    }

    // Parse optional session file from CLI: rusplorer.exe [session.rsess]
    let session: Option<SessionData> = std::env::args()
        .nth(1)
        .and_then(|arg| SessionData::load_from_file(std::path::Path::new(&arg)));

    let mut options = eframe::NativeOptions::default();
    options.viewport.inner_size = session
        .as_ref()
        .and_then(|s| s.window_size)
        .map(|[w, h]| egui::vec2(w, h))
        .or(Some(egui::vec2(660.0, 600.0)));
    options.viewport.position = session
        .as_ref()
        .and_then(|s| s.window_pos)
        .map(|[x, y]| egui::pos2(x, y));
    options.viewport.icon = {
        let icon_bytes = include_bytes!("../Logo/Rustplorer logo.png");
        let image = image::load_from_memory(icon_bytes).expect("Failed to load icon");
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        Some(std::sync::Arc::new(egui::IconData {
            rgba: rgba.into_raw(),
            width,
            height,
        }))
    };
    let is_dev = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_lowercase()))
        .map(|name| name.contains("dev"))
        .unwrap_or(false);
    let window_title = if is_dev { "Rusplorer (dev)" } else { "Rusplorer" };

    eframe::run_native(
        window_title,
        options,
        Box::new(|cc| {
            // Embed Iosevka Aile Regular + Bold (subsetted) at compile time
            let mut fonts = egui::FontDefinitions::default();

            fonts.font_data.insert(
                "IosevkaAile-Regular".to_owned(),
                egui::FontData::from_static(include_bytes!("fonts/IosevkaAile-Regular.ttf")),
            );
            fonts.font_data.insert(
                "IosevkaAile-Bold".to_owned(),
                egui::FontData::from_static(include_bytes!("fonts/IosevkaAile-Bold.ttf")),
            );
            // Replace the default proportional font with Iosevka Aile Regular
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "IosevkaAile-Regular".to_owned());
            // Register Bold under a named family used in the tree
            fonts
                .families
                .entry(egui::FontFamily::Name("Bold".into()))
                .or_default()
                .insert(0, "IosevkaAile-Bold".to_owned());

            cc.egui_ctx.set_fonts(fonts);

            let mut style = (*cc.egui_ctx.style()).clone();
            // Set 11pt font size for all text styles
            for (_, font_id) in &mut style.text_styles {
                font_id.size = 11.0;
            }
            style.spacing.button_padding = egui::vec2(2.0, 0.0);
            style.visuals.widgets.hovered.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.hovered.bg_fill =
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 10);
            style.visuals.widgets.active.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, egui::Color32::DARK_GRAY);
            style.visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
            style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
            cc.egui_ctx.set_style(style);
            Box::new(RusplorerApp::new(session))
        }),
    )
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
enum SortColumn {
    Name,
    Size,
    Date,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Config {
    last_path: String,
    show_date_columns: HashMap<String, bool>,
    #[serde(default = "default_sort_column")]
    sort_column: SortColumn,
    #[serde(default = "default_sort_ascending")]
    sort_ascending: bool,
    #[serde(default)]
    favorites: Vec<String>,
}

fn default_sort_column() -> SortColumn {
    SortColumn::Name
}
fn default_sort_ascending() -> bool {
    true
}

impl Config {
    fn path() -> PathBuf {
        let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rusplorer.exe"));
        let mut config_path = exe_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
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
            show_date_columns: HashMap::new(),
            sort_column: SortColumn::Name,
            sort_ascending: true,
            favorites: Vec::new(),
        }
    }

    fn save(&self) {
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), content);
        }
    }
}

/// Snapshot of the current browsing session that can be saved to a `.rsess`
/// file and restored by passing the file as a CLI argument.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct SessionData {
    tabs: Vec<TabState>,
    active_tab: usize,
    #[serde(default)]
    window_pos: Option<[f32; 2]>,
    #[serde(default)]
    window_size: Option<[f32; 2]>,
}

impl SessionData {
    fn save_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| e.to_string())
            .and_then(|content| std::fs::write(path, content).map_err(|e| e.to_string()))
    }

    fn load_from_file(path: &std::path::Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
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
    cancel_token: Arc<AtomicBool>,
    pause_token: Arc<AtomicBool>,
    dragged_files: Vec<PathBuf>,
    show_drop_menu: bool,
    #[allow(dead_code)]
    drop_menu_position: egui::Pos2,
    is_right_click_drag: bool,
    config: Config,
    max_file_size: u64,
    is_focused: bool,
    filter: String,
    #[allow(dead_code)]
    file_watcher: Option<notify::RecommendedWatcher>,
    watch_receiver: Option<Receiver<PathBuf>>,
    stop_watcher: Option<Sender<()>>,
    show_context_menu: bool,
    context_menu_entry: Option<FileEntry>,
    context_menu_position: egui::Pos2,
    show_rename_dialog: bool,
    rename_buffer: String,
    selected_entries: HashSet<String>,
    show_archive_dialog: bool,
    archive_type: usize,      // 0 = 7z, 1 = zip
    compression_level: usize, // 0 = store, 1 = medium, 2 = high
    archive_name_buffer: String,
    files_to_archive: Vec<PathBuf>,
    archive_done_receiver: Option<Receiver<String>>,
    show_extract_dialog: bool,
    extract_archive_path: PathBuf,
    extract_done_receiver: Option<Receiver<()>>,
    clipboard_files: Vec<PathBuf>,
    clipboard_mode: Option<ClipboardMode>,
    prev_ctrl_c_down: bool,
    prev_ctrl_v_down: bool,
    prev_ctrl_x_down: bool,
    prev_del_down: bool,
    selection_drag_start: Option<egui::Pos2>,
    selection_drag_current: Option<egui::Pos2>,
    entry_rects: HashMap<String, egui::Rect>,
    is_dragging_selection: bool,
    selection_before_drag: HashSet<String>,
    any_button_hovered: bool,
    // Internal drag-and-drop
    dnd_active: bool,
    dnd_sources: Vec<PathBuf>,
    dnd_label: String,
    dnd_start_pos: Option<egui::Pos2>,
    dnd_drag_entry: Option<String>,  // entry name when pointer was pressed (raw tracking)
    dnd_drop_target: Option<PathBuf>,
    dnd_drop_target_prev: Option<PathBuf>, // previous frame's value, used for color display
    dnd_is_right_click: bool,
    dnd_suppress: bool, // suppress new drag detection until all buttons are released
    // Pending right-click drop menu: (sources, destination, screen position)
    dnd_right_drop_menu: Option<(Vec<PathBuf>, PathBuf, egui::Pos2)>,
    dirs_done: HashSet<PathBuf>,
    dirs_done_receiver: Option<Receiver<PathBuf>>,
    show_date_columns: HashMap<PathBuf, bool>,
    sort_column: SortColumn,
    sort_ascending: bool,
    // Left panel
    favorites: Vec<PathBuf>,
    tree_expanded: HashSet<PathBuf>,
    tree_children_cache: HashMap<PathBuf, Vec<PathBuf>>,
    left_panel_width: f32,
    right_panel_width: f32,
    prev_left_panel_width: f32,
    // Tabs
    tabs: Vec<TabState>,
    active_tab: usize,
    // Virtual desktop placement on startup
    startup_vd_done: bool,
    startup_vd_attempts: u8,
    // OLE drop-in channel: Explorer → Rusplorer
    ole_drop_receiver: Option<std::sync::mpsc::Receiver<Vec<PathBuf>>>,
    ole_drop_sender: Option<std::sync::mpsc::Sender<Vec<PathBuf>>>,
    ole_rclick_drop_receiver: Option<std::sync::mpsc::Receiver<Vec<PathBuf>>>,
    ole_rclick_drop_sender: Option<std::sync::mpsc::Sender<Vec<PathBuf>>>,
    drop_target_registered: bool,
    // Keep the COM IDropTarget alive for the lifetime of the app
    #[cfg(windows)]
    _ole_drop_target: Option<windows::Win32::System::Ole::IDropTarget>,
    #[cfg(not(windows))]
    _ole_drop_target: Option<()>,
    // Save-session dialog
    show_save_session_dialog: bool,
    save_session_filename: String,
    save_session_status: Option<String>,
}

#[derive(Clone)]
enum FileAction {
    OpenDir(PathBuf),
    GoToParent,
}

#[derive(Clone, Debug, Copy, PartialEq)]
enum ClipboardMode {
    Copy,
    Cut,
}

#[derive(Clone)]
struct FileEntry {
    name: String,
    is_dir: bool,
    #[allow(dead_code)]
    size: u64,
    modified: Option<SystemTime>,
}

/// Per-tab browsing state.  Lightweight: only stores what needs to be
/// preserved across tab switches.  Everything else (computed sizes, watcher,
/// selection, etc.) is rebuilt on switch via `refresh_contents()`.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct TabState {
    path: PathBuf,
    back_history: Vec<PathBuf>,
    forward_history: Vec<PathBuf>,
    filter: String,
    sort_column: SortColumn,
    sort_ascending: bool,
}

impl TabState {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            back_history: Vec::new(),
            forward_history: Vec::new(),
            filter: String::new(),
            sort_column: SortColumn::Name,
            sort_ascending: true,
        }
    }

    /// Short display label: last path component, or drive letter.
    fn label(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.to_string_lossy().to_string())
    }
}

impl Default for RusplorerApp {
    fn default() -> Self {
        Self::new(None)
    }
}

impl RusplorerApp {
    fn new(session: Option<SessionData>) -> Self {
        let available_drives = Self::list_drives();
        let config = Config::load();
        let start_path = PathBuf::from(&config.last_path);
        let current_path = if start_path.exists() {
            start_path
        } else {
            PathBuf::from("C:\\")
        };
        let show_date_columns: HashMap<PathBuf, bool> = config
            .show_date_columns
            .iter()
            .map(|(k, v)| (PathBuf::from(k), *v))
            .collect();
        let sort_column = config.sort_column.clone();
        let (ole_tx, ole_rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        let (ole_rc_tx, ole_rc_rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        let sort_ascending = config.sort_ascending;
        let mut favorites: Vec<PathBuf> = config.favorites.iter().map(PathBuf::from).collect();
        favorites.sort_by(|a, b| {
            let a_name = a.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_else(|| a.to_string_lossy().to_lowercase().into());
            let b_name = b.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_else(|| b.to_string_lossy().to_lowercase().into());
            a_name.cmp(&b_name)
        });

        let mut app = Self {
            current_path,
            contents: Vec::new(),
            selected_action: None,
            back_history: Vec::new(),
            forward_history: Vec::new(),
            available_drives,
            file_sizes: HashMap::new(),
            size_receiver: None,
            cancel_token: Arc::new(AtomicBool::new(false)),
            pause_token: Arc::new(AtomicBool::new(false)),
            dragged_files: Vec::new(),
            show_drop_menu: false,
            drop_menu_position: egui::Pos2::ZERO,
            is_right_click_drag: false,
            config,
            max_file_size: 0,
            is_focused: true,
            filter: String::new(),
            file_watcher: None,
            watch_receiver: None,
            stop_watcher: None,
            show_context_menu: false,
            context_menu_entry: None,
            context_menu_position: egui::Pos2::ZERO,
            show_rename_dialog: false,
            rename_buffer: String::new(),
            selected_entries: HashSet::new(),
            show_archive_dialog: false,
            archive_type: 0,
            compression_level: 2,
            archive_name_buffer: String::new(),
            files_to_archive: Vec::new(),
            archive_done_receiver: None,
            show_extract_dialog: false,
            extract_archive_path: PathBuf::new(),
            extract_done_receiver: None,
            clipboard_files: Vec::new(),
            clipboard_mode: None,
            prev_ctrl_c_down: false,
            prev_ctrl_v_down: false,
            prev_ctrl_x_down: false,
            prev_del_down: false,
            selection_drag_start: None,
            selection_drag_current: None,
            entry_rects: HashMap::new(),
            is_dragging_selection: false,
            selection_before_drag: HashSet::new(),
            any_button_hovered: false,
            dnd_active: false,
            dnd_sources: Vec::new(),
            dnd_label: String::new(),
            dnd_start_pos: None,
            dnd_drag_entry: None,
            dnd_drop_target: None,
            dnd_drop_target_prev: None,
            dnd_is_right_click: false,
            dnd_suppress: false,
            dnd_right_drop_menu: None,
            dirs_done: HashSet::new(),
            dirs_done_receiver: None,
            show_date_columns,
            sort_column,
            sort_ascending,
            favorites,
            tree_expanded: HashSet::new(),
            tree_children_cache: HashMap::new(),
            left_panel_width: 150.0,
            right_panel_width: 0.0,
            prev_left_panel_width: 0.0,
            tabs: Vec::new(), // populated below
            active_tab: 0,
            startup_vd_done: false,
            startup_vd_attempts: 0,
            ole_drop_receiver: Some(ole_rx),
            ole_drop_sender: Some(ole_tx),
            ole_rclick_drop_receiver: Some(ole_rc_rx),
            ole_rclick_drop_sender: Some(ole_rc_tx),
            drop_target_registered: false,
            _ole_drop_target: None,
            show_save_session_dialog: false,
            save_session_filename: String::new(),
            save_session_status: None,
        };

        // Pre-expand the tree down to the current folder so it's visible on startup.
        // Walk every ancestor (including current_path itself) from root downward.
        let ancestors: Vec<PathBuf> = app.current_path.ancestors().map(|p| p.to_path_buf()).collect();
        for ancestor in ancestors.into_iter().rev() {
            let children = read_dir_children(&ancestor);
            app.tree_children_cache.insert(ancestor.clone(), children);
            app.tree_expanded.insert(ancestor);
        }

        // Initialise tabs — from session if provided, otherwise single tab at current path
        if let Some(sess) = session {
            if !sess.tabs.is_empty() {
                app.tabs = sess.tabs;
                app.active_tab = sess.active_tab.min(app.tabs.len().saturating_sub(1));
                // Set current_path from the active tab
                app.current_path = app.tabs[app.active_tab].path.clone();
                // Expand tree to the active tab's path
                let ancestors: Vec<PathBuf> = app.current_path.ancestors().map(|p| p.to_path_buf()).collect();
                for ancestor in ancestors.into_iter().rev() {
                    let children = read_dir_children(&ancestor);
                    app.tree_children_cache.insert(ancestor.clone(), children);
                    app.tree_expanded.insert(ancestor);
                }
            } else {
                app.tabs.push(TabState::new(app.current_path.clone()));
            }
        } else {
            app.tabs.push(TabState {
                path: app.current_path.clone(),
                back_history: Vec::new(),
                forward_history: Vec::new(),
                filter: app.filter.clone(),
                sort_column: app.sort_column.clone(),
                sort_ascending: app.sort_ascending,
            });
        }

        app.refresh_contents();
        app.start_file_watcher();
        app
    }
}

impl RusplorerApp {
    /// Snapshot the current tabs into a `SessionData` and write it to `path`.
    fn save_session_to_file(&mut self, path: &std::path::Path, ctx: &egui::Context) -> Result<(), String> {
        self.save_active_tab();
        let (window_pos, window_size) = ctx.input(|i| {
            let vp = i.viewport();
            let pos = vp.outer_rect.map(|r| [r.min.x, r.min.y]);
            let size = vp.inner_rect.map(|r| [r.width(), r.height()]);
            (pos, size)
        });
        let data = SessionData {
            tabs: self.tabs.clone(),
            active_tab: self.active_tab,
            window_pos,
            window_size,
        };
        data.save_to_file(path)
    }

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

    /// Collapse the entire tree, then expand only the ancestors of `path`.
    /// This ensures unrelated drives/folders are hidden after every navigation.
    fn expand_tree_to(&mut self, path: &PathBuf) {
        self.tree_expanded.clear();
        let ancestors: Vec<PathBuf> = path.ancestors().map(|p| p.to_path_buf()).collect();
        for ancestor in ancestors.into_iter().rev() {
            if !self.tree_children_cache.contains_key(&ancestor) {
                let children = read_dir_children(&ancestor);
                self.tree_children_cache.insert(ancestor.clone(), children);
            }
            self.tree_expanded.insert(ancestor);
        }
    }

    // ── Tab helpers ────────────────────────────────────────────────────

    /// Save the current browsing state into the active tab.
    fn save_active_tab(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.path = self.current_path.clone();
            tab.back_history = self.back_history.clone();
            tab.forward_history = self.forward_history.clone();
            tab.filter = self.filter.clone();
            tab.sort_column = self.sort_column.clone();
            tab.sort_ascending = self.sort_ascending;
        }
    }

    /// Restore per-tab state from the given tab index into the app fields
    /// and refresh directory contents + watcher.
    fn restore_tab(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get(index) {
            self.current_path = tab.path.clone();
            self.back_history = tab.back_history.clone();
            self.forward_history = tab.forward_history.clone();
            self.filter = tab.filter.clone();
            self.sort_column = tab.sort_column.clone();
            self.sort_ascending = tab.sort_ascending;
            self.selected_entries.clear();

            // Collapse everything unrelated, expand only ancestors of new path
            let path_snap = self.current_path.clone();
            self.expand_tree_to(&path_snap);

            self.refresh_contents();
            self.start_file_watcher();
        }
    }

    /// Switch to a different tab index.
    fn switch_to_tab(&mut self, index: usize) {
        if index == self.active_tab || index >= self.tabs.len() {
            return;
        }
        self.save_active_tab();
        self.active_tab = index;
        self.restore_tab(index);
    }

    /// Open a new tab.  Clones the current path by default.
    fn new_tab(&mut self, path: Option<PathBuf>) {
        self.save_active_tab();
        let tab_path = path.unwrap_or_else(|| self.current_path.clone());
        self.tabs.push(TabState::new(tab_path));
        self.active_tab = self.tabs.len() - 1;
        self.restore_tab(self.active_tab);
    }

    /// Close the tab at `index`.  Won't close the last remaining tab.
    fn close_tab(&mut self, index: usize) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.tabs.remove(index);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if index < self.active_tab {
            self.active_tab -= 1;
        } else if index == self.active_tab {
            // We removed the active tab — restore whichever tab is now at this index
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
            self.restore_tab(self.active_tab);
            return;
        }
        // No restore needed here — active tab didn't change identity
    }

    fn refresh_contents(&mut self) {
        // Cancel any running background computation
        self.cancel_token.store(true, Ordering::SeqCst);
        self.cancel_token = Arc::new(AtomicBool::new(false));

        self.contents.clear();
        self.file_sizes.clear();
        self.max_file_size = 0;
        self.dirs_done.clear();

        // Add parent directory option
        if let Some(parent) = self.current_path.parent() {
            if parent != self.current_path {
                self.contents.push(FileEntry {
                    name: "[..] Parent Directory".to_string(),
                    is_dir: true,
                    size: 0,
                    modified: None,
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
                    let modified = e.metadata().ok().and_then(|m| m.modified().ok());
                    FileEntry {
                        name,
                        is_dir,
                        size: 0,
                        modified,
                    }
                })
                .collect();

            items.sort_by(|a, b| {
                match (a.is_dir, b.is_dir) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => {
                        let ord = match self.sort_column {
                            SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                            SortColumn::Date => a.modified.cmp(&b.modified),
                            SortColumn::Size => std::cmp::Ordering::Equal, // will be re-sorted when sizes arrive
                        };
                        if self.sort_ascending {
                            ord
                        } else {
                            ord.reverse()
                        }
                    }
                }
            });

            self.contents.extend(items);
        }

        // Collect paths for background processing
        let mut file_paths: Vec<PathBuf> = Vec::new();
        let mut dir_paths: Vec<PathBuf> = Vec::new();
        for entry in &self.contents {
            if entry.name.starts_with("[..]") {
                continue;
            }
            let full_path = self.current_path.join(&entry.name);
            if entry.is_dir {
                dir_paths.push(full_path);
            } else {
                file_paths.push(full_path);
            }
        }

        // Start background thread to load file and folder sizes
        let cancel_token = self.cancel_token.clone();
        let pause_token = self.pause_token.clone();
        let (tx, rx) = channel();
        let (done_tx, done_rx) = channel::<PathBuf>();

        std::thread::spawn(move || {
            // First: send all file sizes immediately (fast)
            for path in file_paths {
                if cancel_token.load(Ordering::SeqCst) {
                    return;
                }
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let _ = tx.send((path, size));
            }

            // Then: compute directory sizes in parallel
            let num_threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(dir_paths.len().max(1));

            if !dir_paths.is_empty() {
                let work_queue = std::sync::Arc::new(std::sync::Mutex::new(dir_paths));
                let mut handles = Vec::new();

                for _ in 0..num_threads {
                    let queue = work_queue.clone();
                    let cancel = cancel_token.clone();
                    let pause = pause_token.clone();
                    let tx = tx.clone();
                    let done_tx = done_tx.clone();

                    handles.push(std::thread::spawn(move || {
                        loop {
                            let dir_path = {
                                match queue.lock() {
                                    Ok(mut dirs) => dirs.pop(),
                                    Err(_) => break,
                                }
                            };

                            let dir_path = match dir_path {
                                Some(p) => p,
                                None => break,
                            };

                            if cancel.load(Ordering::SeqCst) {
                                return;
                            }
                            while pause.load(Ordering::SeqCst) {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                                if cancel.load(Ordering::SeqCst) {
                                    return;
                                }
                            }

                            let mut accumulated = 0u64;
                            calculate_dir_size_progressive(
                                &dir_path,
                                &dir_path,
                                &cancel,
                                &pause,
                                &tx,
                                &mut accumulated,
                            );
                            // Always send final size (handles empty dirs and permission errors)
                            let _ = tx.send((dir_path.clone(), accumulated));
                            // Signal this directory is done computing
                            let _ = done_tx.send(dir_path);
                        }
                    }));
                }

                for handle in handles {
                    let _ = handle.join();
                }
            }
        });

        self.dirs_done_receiver = Some(done_rx);

        self.size_receiver = Some(rx);
    }

    fn sort_contents(&mut self) {
        let sort_column = &self.sort_column;
        let sort_ascending = self.sort_ascending;
        let file_sizes = &self.file_sizes;
        let current_path = &self.current_path;

        self.contents.sort_by(|a, b| {
            // Parent directory always first
            if a.name.starts_with("[..]") {
                return std::cmp::Ordering::Less;
            }
            if b.name.starts_with("[..]") {
                return std::cmp::Ordering::Greater;
            }

            // Dirs always before files
            match (a.is_dir, b.is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => {
                    let ord = match sort_column {
                        SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                        SortColumn::Size => {
                            let sa = file_sizes
                                .get(&current_path.join(&a.name))
                                .copied()
                                .unwrap_or(0);
                            let sb = file_sizes
                                .get(&current_path.join(&b.name))
                                .copied()
                                .unwrap_or(0);
                            sa.cmp(&sb)
                        }
                        SortColumn::Date => a.modified.cmp(&b.modified),
                    };
                    if sort_ascending { ord } else { ord.reverse() }
                }
            }
        });
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
            self.config.show_date_columns = self
                .show_date_columns
                .iter()
                .map(|(k, v)| (k.to_string_lossy().to_string(), *v))
                .collect();
            self.config.save();

            self.refresh_contents();
            // Restart watcher for the new directory
            self.start_file_watcher();

            // Collapse everything unrelated, expand only ancestors of new path
            let path_snap = self.current_path.clone();
            self.expand_tree_to(&path_snap);

            self.save_active_tab();
        }
    }

    fn go_back(&mut self) {
        if let Some(previous) = self.back_history.pop() {
            self.forward_history.push(self.current_path.clone());
            self.current_path = previous;
            let path_snap = self.current_path.clone();
            self.expand_tree_to(&path_snap);
            self.refresh_contents();
            self.save_active_tab();
        }
    }

    fn go_forward(&mut self) {
        if let Some(next) = self.forward_history.pop() {
            self.back_history.push(self.current_path.clone());
            self.current_path = next;
            let path_snap = self.current_path.clone();
            self.expand_tree_to(&path_snap);
            self.refresh_contents();
            self.save_active_tab();
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

    fn is_code_file(path: &PathBuf) -> bool {
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

    fn is_archive(path: &PathBuf) -> bool {
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

    fn format_modified_time(time: SystemTime) -> String {
        use std::time::UNIX_EPOCH;
        if let Ok(duration) = time.duration_since(UNIX_EPOCH) {
            let secs = duration.as_secs();
            let days = secs / 86400;
            let epoch_start = 719163; // Days from year 0 to 1970-01-01
            let total_days = epoch_start + days as i64;

            // Simple date calculation
            let mut remaining_days = total_days;

            // Find the year
            let mut year = (remaining_days / 365) as i32;
            let mut days_in_years = 0i64;
            for y in 0..=year {
                let is_leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
                days_in_years += if is_leap { 366 } else { 365 };
            }
            while days_in_years > total_days {
                year -= 1;
                let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
                days_in_years -= if is_leap { 366 } else { 365 };
            }
            remaining_days = total_days - days_in_years;

            // Find month and day
            let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            let days_in_months = if is_leap {
                [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
            } else {
                [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
            };

            let mut month = 1;
            for (i, &days_in_month) in days_in_months.iter().enumerate() {
                if remaining_days < days_in_month as i64 {
                    month = i + 1;
                    break;
                }
                remaining_days -= days_in_month as i64;
            }
            let day = remaining_days + 1;

            // Time calculation
            let time_secs = secs % 86400;
            let hour = time_secs / 3600;
            let minute = (time_secs % 3600) / 60;

            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                year, month, day, hour, minute
            )
        } else {
            String::new()
        }
    }

    fn copy_files(sources: &[PathBuf], dest: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        for source in sources {
            let file_name = source.file_name().unwrap();
            let mut target = dest.join(file_name);

            // If target already exists (e.g. copying to same folder), generate a unique name
            if target.exists() {
                let stem = target.file_stem().unwrap_or_default().to_string_lossy().to_string();
                let ext = target.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                let mut n = 1u32;
                target = dest.join(format!("{} - Copy{}", stem, ext));
                while target.exists() {
                    n += 1;
                    target = dest.join(format!("{} - Copy ({}){}", stem, n, ext));
                }
            }

            if source.is_dir() {
                copy_dir_recursive(source, &target)?;
            } else {
                std::fs::copy(source, &target)?;
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

fn read_dir_children(path: &PathBuf) -> Vec<PathBuf> {
    std::fs::read_dir(path)
        .map(|entries| {
            let mut children: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.path())
                .collect();
            children.sort_by(|a, b| {
                let a_name = a.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                let b_name = b.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                a_name.cmp(&b_name)
            });
            children
        })
        .unwrap_or_default()
}

fn render_tree_node(
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
) {
    let is_expanded = expanded.contains(path);
    let display_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    let indent = depth as f32 * 10.0;
    let max_w = ui.available_width();
    let is_current = path == current_path;
    let is_ancestor = !is_current && current_path.ancestors().any(|a| a == path.as_path());

    // Truncate display name to fit available width (prevents layout overflow)
    let font_id = if is_ancestor || is_current {
        egui::FontId::new(11.0, egui::FontFamily::Name("Bold".into()))
    } else {
        egui::FontId::new(11.0, egui::FontFamily::Proportional)
    };
    let btn_width = max_w - indent - 4.0; // padding
    let truncated_name = {
        let fonts = ui.fonts(|f| f.clone());
        let full_w = fonts.layout_no_wrap(display_name.clone(), font_id.clone(), egui::Color32::WHITE).size().x;
        if full_w <= btn_width || btn_width <= 0.0 {
            display_name.clone()
        } else {
            let ellipsis = "…";
            let mut truncated = display_name.clone();
            while !truncated.is_empty() {
                truncated.pop();
                let candidate = format!("{}{}", truncated, ellipsis);
                let w = fonts.layout_no_wrap(candidate.clone(), font_id.clone(), egui::Color32::WHITE).size().x;
                if w <= btn_width {
                    break;
                }
            }
            format!("{}…", truncated)
        }
    };

    let is_tree_drop_target = dnd_active
        && dnd_drop_target.as_ref() == Some(path)
        && !is_current; // can't drop onto the current folder (it's the source parent)

    let response = ui.allocate_ui_with_layout(
        egui::vec2(max_w, 16.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_max_width(max_w);
            ui.set_clip_rect(ui.max_rect());
            if indent > 0.0 {
                ui.add_space(indent);
            }
            let text_color = if is_current || is_tree_drop_target {
                egui::Color32::WHITE
            } else {
                egui::Color32::BLACK
            };
            let base_text = egui::RichText::new(&truncated_name).color(text_color);
            let label_text = if is_ancestor || is_current {
                base_text.font(font_id.clone())
            } else {
                base_text
            };
            let button = if is_tree_drop_target {
                egui::Button::new(label_text)
                    .fill(egui::Color32::from_rgb(80, 200, 80))
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
    // Record rect for next-frame DnD drop detection
    // Instead, detect hover directly this frame
    let is_valid_drop = dnd_active && !is_current && !dnd_sources.contains(path);
    // Use raw rect check — response.inner.hovered() is suppressed while a mouse button is held
    if is_valid_drop {
        if let Some(pos) = ui.ctx().input(|i| i.pointer.hover_pos()) {
            if response.inner.rect.contains(pos) {
                *hovered_drop = Some(path.clone());
            }
        }
    }
    if response.inner
            .on_hover_text(path.to_string_lossy())
            .clicked()
        {
            // Toggle expand/collapse
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
            // navigate_to would re-expand everything and undo a collapse
            if !is_current {
                *nav = Some(path.clone());
            }
        }

    if is_expanded {
        if let Some(children) = children_cache.get(path).cloned() {
            for child in &children {
                render_tree_node(ui, child, expanded, children_cache, nav, current_path, depth + 1, dnd_active, dnd_sources, dnd_drop_target, hovered_drop);
            }
        }
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

impl RusplorerApp {
    fn start_file_watcher(&mut self) {
        // Signal old watcher to stop
        if let Some(stop_tx) = self.stop_watcher.take() {
            let _ = stop_tx.send(());
        }

        let (tx, rx) = channel();
        let (stop_tx, stop_rx) = channel();
        let current_path = self.current_path.clone();

        // Create watcher in a separate thread
        let tx = std::sync::Arc::new(std::sync::Mutex::new(tx));

        std::thread::spawn(move || {
            let tx = tx.clone();
            if let Ok(mut watcher) = recommended_watcher(move |res| {
                match res {
                    Ok(notify::event::Event {
                        kind:
                            notify::event::EventKind::Modify(_)
                            | notify::event::EventKind::Create(_)
                            | notify::event::EventKind::Remove(_),
                        paths,
                        ..
                    }) => {
                        // Send the actual changed paths to invalidate cache
                        for path in paths {
                            if let Ok(tx) = tx.lock() {
                                let _ = tx.send(path);
                            }
                        }
                    }
                    _ => {}
                }
            }) {
                // Watch the directory (non-recursive to avoid flood of deep events)
                match watcher.watch(&current_path, RecursiveMode::NonRecursive) {
                    Ok(_) => {
                        // Keep watcher alive until stop signal arrives
                        let _ = stop_rx.recv();
                    }
                    Err(_) => {
                        return;
                    }
                }
            }
        });

        self.watch_receiver = Some(rx);
        self.stop_watcher = Some(stop_tx);
    }

    fn process_file_changes(&mut self) {
        let mut needs_refresh = false;

        if let Some(ref rx) = self.watch_receiver {
            while let Ok(path) = rx.try_recv() {
                // Only care about direct children of current directory
                if let Some(parent) = path.parent() {
                    if parent == self.current_path {
                        let file_name = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        let exists_in_list = self.contents.iter().any(|e| e.name == file_name);
                        let exists_on_disk = path.exists();

                        if (exists_on_disk && !exists_in_list)
                            || (!exists_on_disk && exists_in_list)
                        {
                            // Direct child created or removed - full refresh needed
                            needs_refresh = true;
                        } else if exists_on_disk && !path.is_dir() {
                            // Direct child file was modified - update its size inline
                            if let Ok(metadata) = path.metadata() {
                                let size = metadata.len();
                                self.file_sizes.insert(path, size);
                                if size > self.max_file_size {
                                    self.max_file_size = size;
                                }
                            }
                        }
                    }
                }
            }
        }

        if needs_refresh {
            self.refresh_contents();
        }
    }
}

impl eframe::App for RusplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Rotate drop target: prev holds last frame's value for color display;
        // current is reset to None so tree / breadcrumbs / table can detect fresh this frame.
        if self.dnd_active {
            self.dnd_drop_target_prev = self.dnd_drop_target.clone();
            self.dnd_drop_target = None;
        } else {
            self.dnd_drop_target_prev = None;
        }

        // Clear suppress flag once all buttons are physically released.
        // We must use GetAsyncKeyState (actual hardware state) instead of egui's
        // pointer tracking, because egui never receives WM_xBUTTONUP when the
        // release happened in another window (e.g. another Rusplorer instance).
        if self.dnd_suppress {
            #[cfg(windows)]
            {
                use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
                let lmb_down = unsafe { GetAsyncKeyState(0x01) } & (0x8000u16 as i16) != 0; // VK_LBUTTON
                let rmb_down = unsafe { GetAsyncKeyState(0x02) } & (0x8000u16 as i16) != 0; // VK_RBUTTON
                if !lmb_down && !rmb_down {
                    self.dnd_suppress = false;
                }
            }
            #[cfg(not(windows))]
            {
                let any_held = ctx.input(|i|
                    i.pointer.primary_down()
                        || i.pointer.button_down(egui::PointerButton::Secondary)
                );
                if !any_held {
                    self.dnd_suppress = false;
                }
            }
        }

        // Move own window to "Rusplorer" virtual desktop on startup (in-process: no E_ACCESSDENIED)
        if !self.startup_vd_done {
            self.startup_vd_attempts += 1;
            #[cfg(windows)]
            if try_move_to_rusplorer_desktop() || self.startup_vd_attempts >= 10 {
                self.startup_vd_done = true;
            }
            #[cfg(not(windows))]
            { self.startup_vd_done = true; }
        }

        // Register OLE IDropTarget on our HWND so Explorer can drag files in
        #[cfg(windows)]
        if !self.drop_target_registered {
            if let Some(tx) = self.ole_drop_sender.take() {
                let rc_tx = self.ole_rclick_drop_sender.take();
                let wide_title: Vec<u16> = OsStr::new("Rusplorer")
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
                unsafe {
                    let hwnd_raw = winapi::um::winuser::FindWindowW(
                        std::ptr::null(), wide_title.as_ptr());
                    if !hwnd_raw.is_null() {
                        let hwnd_ptr = hwnd_raw as *mut _;
                        let rc = rc_tx.unwrap_or_else(|| std::sync::mpsc::channel().0);
                        if let Some(target) = register_ole_drop_target(hwnd_ptr, tx, rc) {
                            self._ole_drop_target = Some(target);
                            self.drop_target_registered = true;
                        } else {
                            // Registration failed — don't retry (probably no OLE)
                            self.drop_target_registered = true;
                        }
                    } else {
                        // HWND not ready yet — put senders back
                        self.ole_drop_sender = Some(tx);
                        self.ole_rclick_drop_sender = rc_tx;
                    }
                }
            }
        }

        // Check if archive compression finished
        if let Some(ref rx) = self.archive_done_receiver {
            if let Ok(archive_name) = rx.try_recv() {
                self.refresh_contents();
                self.selected_entries.clear();
                self.selected_entries.insert(archive_name);
                self.archive_done_receiver = None;
            }
        }

        // Check if extraction finished
        if let Some(ref rx) = self.extract_done_receiver {
            if rx.try_recv().is_ok() {
                self.refresh_contents();
                self.show_extract_dialog = false;
                self.extract_done_receiver = None;
            }
        }

        // Process any file system changes detected by watcher
        self.process_file_changes();

        // Track window focus and pause/resume background work
        let is_focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if is_focused != self.is_focused {
            self.is_focused = is_focused;
            self.pause_token.store(!is_focused, Ordering::SeqCst);
        }

        // Receive OLE drops from Explorer (drag-in)  — left-click = move
        #[cfg(windows)]
        {
            let incoming: Vec<Vec<PathBuf>> = self.ole_drop_receiver
                .as_ref()
                .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
                .unwrap_or_default();
            for files in incoming {
                if !files.is_empty() {
                    let dest = self.current_path.clone();
                    let _ = Self::move_files(&files, &dest);
                    self.refresh_contents();
                    ctx.request_repaint();
                }
            }
        }

        // Receive OLE right-click drops — show menu
        #[cfg(windows)]
        {
            let incoming: Vec<Vec<PathBuf>> = self.ole_rclick_drop_receiver
                .as_ref()
                .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
                .unwrap_or_default();
            for files in incoming {
                if !files.is_empty() {
                    let dest = self.current_path.clone();
                    let drop_pos = ctx.input(|i| i.pointer.hover_pos().unwrap_or(egui::pos2(300.0, 300.0)));
                    self.dnd_right_drop_menu = Some((files, dest, drop_pos));
                    ctx.request_repaint();
                }
            }
        }

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
                    self.is_right_click_drag =
                        i.pointer.button_down(egui::PointerButton::Secondary);
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
        let mut sizes_updated = false;
        if let Some(ref rx) = self.size_receiver {
            while let Ok((path, size)) = rx.try_recv() {
                self.file_sizes.insert(path, size);
                if size > self.max_file_size {
                    self.max_file_size = size;
                }
                sizes_updated = true;
            }
        }

        // Receive directory completion signals
        if let Some(ref rx) = self.dirs_done_receiver {
            while let Ok(path) = rx.try_recv() {
                self.dirs_done.insert(path);
            }
        }

        // Re-sort when sizes arrive and we're sorting by size
        if sizes_updated && self.sort_column == SortColumn::Size {
            self.sort_contents();
        }

        // Handle mouse buttons 4 and 5 (back/forward)
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::PointerButton {
                    button, pressed, ..
                } = event
                {
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

        // Handle Ctrl+A to select all
        if ctx.input(|i| i.key_pressed(egui::Key::A) && i.modifiers.ctrl) {
            self.selected_entries.clear();
            for entry in &self.contents {
                if !entry.name.starts_with("[..]") {
                    self.selected_entries.insert(entry.name.clone());
                }
            }
        }

        // Handle Escape to deselect all
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selected_entries.clear();
        }

        // Handle Ctrl+C / Ctrl+X / Ctrl+V / DEL using Windows API directly (bypass egui)
        let (got_copy, got_cut, got_paste, got_delete) = {
            #[cfg(windows)]
            {
                const VK_CONTROL: i32 = 0x11;
                const VK_C: i32 = 0x43;
                const VK_V: i32 = 0x56;
                const VK_X: i32 = 0x58;
                const VK_DELETE: i32 = 0x2E;

                let ctrl_down = unsafe { GetAsyncKeyState(VK_CONTROL) } as u16 & 0x8000 != 0;
                let c_down = ctrl_down && (unsafe { GetAsyncKeyState(VK_C) } as u16 & 0x8000 != 0);
                let v_down = ctrl_down && (unsafe { GetAsyncKeyState(VK_V) } as u16 & 0x8000 != 0);
                let x_down = ctrl_down && (unsafe { GetAsyncKeyState(VK_X) } as u16 & 0x8000 != 0);
                let del_down = unsafe { GetAsyncKeyState(VK_DELETE) } as u16 & 0x8000 != 0;

                // Always update prev state to avoid a false edge-trigger when we regain focus
                let prev_c = self.prev_ctrl_c_down;
                let prev_v = self.prev_ctrl_v_down;
                let prev_x = self.prev_ctrl_x_down;
                let prev_d = self.prev_del_down;
                self.prev_ctrl_c_down = c_down;
                self.prev_ctrl_v_down = v_down;
                self.prev_ctrl_x_down = x_down;
                self.prev_del_down = del_down;

                // Only fire actions when Rusplorer actually has focus
                if self.is_focused {
                    let copy_pressed   = c_down   && !prev_c;
                    let paste_pressed  = v_down   && !prev_v;
                    let cut_pressed    = x_down   && !prev_x;
                    let delete_pressed = del_down && !prev_d;
                    (copy_pressed, cut_pressed, paste_pressed, delete_pressed)
                } else {
                    (false, false, false, false)
                }
            }
            #[cfg(not(windows))]
            {
                let c = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::C));
                let x = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::X));
                let v = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::V));
                let d = ctx.input(|i| i.key_pressed(egui::Key::Delete));
                (c, x, v, d)
            }
        };

        if got_copy && !self.selected_entries.is_empty() {
            let files: Vec<PathBuf> = self
                .selected_entries
                .iter()
                .map(|name| self.current_path.join(name))
                .collect();

            #[cfg(windows)]
            {
                if let Ok(_) = copy_files_to_clipboard(&files) {
                    self.clipboard_files = files;
                    self.clipboard_mode = Some(ClipboardMode::Copy);
                }
            }
            #[cfg(not(windows))]
            {
                self.clipboard_files = files;
                self.clipboard_mode = Some(ClipboardMode::Copy);
            }
        }

        if got_cut && !self.selected_entries.is_empty() {
            let files: Vec<PathBuf> = self
                .selected_entries
                .iter()
                .map(|name| self.current_path.join(name))
                .collect();

            #[cfg(windows)]
            {
                if let Ok(_) = copy_files_to_clipboard(&files) {
                    self.clipboard_files = files;
                    self.clipboard_mode = Some(ClipboardMode::Cut);
                }
            }
            #[cfg(not(windows))]
            {
                self.clipboard_files = files;
                self.clipboard_mode = Some(ClipboardMode::Cut);
            }
        }

        if got_paste {
            #[cfg(windows)]
            {
                // Try to read from Windows clipboard
                if let Ok(clipboard_files) = read_files_from_clipboard() {
                    if !clipboard_files.is_empty() {
                        let dest = self.current_path.clone();

                        // Check if these are our internal cut files
                        let is_cut = self.clipboard_mode == Some(ClipboardMode::Cut)
                            && clipboard_files == self.clipboard_files;

                        if is_cut {
                            let _ = RusplorerApp::move_files(&clipboard_files, &dest);
                            self.clipboard_files.clear();
                            self.clipboard_mode = None;
                        } else {
                            let _ = RusplorerApp::copy_files(&clipboard_files, &dest);
                        }

                        self.refresh_contents();
                    }
                }
            }
            #[cfg(not(windows))]
            {
                if let Some(mode) = self.clipboard_mode {
                    if !self.clipboard_files.is_empty() {
                        let files = self.clipboard_files.clone();
                        let dest = self.current_path.clone();

                        match mode {
                            ClipboardMode::Copy => {
                                let _ = RusplorerApp::copy_files(&files, &dest);
                            }
                            ClipboardMode::Cut => {
                                let _ = RusplorerApp::move_files(&files, &dest);
                                self.clipboard_files.clear();
                                self.clipboard_mode = None;
                            }
                        }

                        self.refresh_contents();
                    }
                }
            }
        }

        // Handle DEL key - send to recycle bin
        if got_delete && !self.selected_entries.is_empty() {
            let files_to_delete: Vec<PathBuf> = self
                .selected_entries
                .iter()
                .map(|name| self.current_path.join(name))
                .collect();

            #[cfg(windows)]
            {
                // Build double-null-terminated wide string list
                let mut path_buffer: Vec<u16> = Vec::new();
                for path in &files_to_delete {
                    let wide: Vec<u16> = OsStr::new(path.to_str().unwrap())
                        .encode_wide()
                        .chain(std::iter::once(0u16))
                        .collect();
                    path_buffer.extend_from_slice(&wide);
                }
                path_buffer.push(0u16); // Final null terminator

                unsafe {
                    let mut file_op = SHFILEOPSTRUCTW {
                        hwnd: std::ptr::null_mut(),
                        wFunc: FO_DELETE as u32,
                        pFrom: path_buffer.as_ptr(),
                        pTo: std::ptr::null(),
                        fFlags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION,
                        fAnyOperationsAborted: 0,
                        hNameMappings: std::ptr::null_mut(),
                        lpszProgressTitle: std::ptr::null(),
                    };

                    let result = SHFileOperationW(&mut file_op);
                    if result == 0 {
                        self.selected_entries.clear();
                        self.refresh_contents();
                    }
                }
            }
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

        // ── Left panel ────────────────────────────────────────────────────
        let mut nav_from_panel: Option<PathBuf> = None;

        // Measure ideal panel width from visible content (for this frame, apply next frame)
        {
            let font_id = egui::FontId::new(11.0, egui::FontFamily::Proportional);
            let mut max_w: f32 = 80.0;
            // Measure favorites (8px indent + name + 16px for × button)
            for fav in &self.favorites {
                let name = fav.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| fav.to_string_lossy().to_string());
                let text_w = ctx.fonts(|f| f.layout_no_wrap(name, font_id.clone(), egui::Color32::WHITE).size().x);
                max_w = max_w.max(8.0 + text_w + 16.0);
            }
            // Measure tree (recursively through expanded nodes)
            fn measure_tree(
                path: &PathBuf,
                depth: usize,
                expanded: &HashSet<PathBuf>,
                cache: &HashMap<PathBuf, Vec<PathBuf>>,
                font_id: &egui::FontId,
                ctx: &egui::Context,
                max_w: &mut f32,
            ) {
                let name = path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string_lossy().to_string());
                let indent = depth as f32 * 10.0;
                let text_w = ctx.fonts(|f| f.layout_no_wrap(name, font_id.clone(), egui::Color32::WHITE).size().x);
                *max_w = max_w.max(indent + text_w + 12.0);
                if expanded.contains(path) {
                    if let Some(children) = cache.get(path) {
                        for child in children {
                            measure_tree(child, depth + 1, expanded, cache, font_id, ctx, max_w);
                        }
                    }
                }
            }
            for drive in &self.available_drives {
                let drive_path = PathBuf::from(drive);
                measure_tree(&drive_path, 0, &self.tree_expanded, &self.tree_children_cache, &font_id, ctx, &mut max_w);
            }
            self.left_panel_width = max_w.min(250.0).max(80.0);
        }

        // Capture right panel width on first frame, then resize window to fit left+right
        let inner_w = ctx.input(|i| i.viewport().inner_rect.map(|r| r.width())).unwrap_or(0.0);
        if self.right_panel_width == 0.0 && inner_w > 0.0 {
            // Initialise: remember right panel width from the actual window and initial left panel
            self.right_panel_width = (inner_w - self.left_panel_width - 8.0).max(200.0);
            self.prev_left_panel_width = self.left_panel_width;
        } else if self.right_panel_width > 0.0 {
            let left_changed = (self.left_panel_width - self.prev_left_panel_width).abs() > 0.5;
            if left_changed {
                // Left panel changed — resize window to preserve right panel width
                let desired_w = self.left_panel_width + self.right_panel_width + 8.0;
                let h = ctx.input(|i| i.viewport().inner_rect.map(|r| r.height())).unwrap_or(600.0);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(desired_w, h)));
                self.prev_left_panel_width = self.left_panel_width;
            } else {
                // Left panel unchanged — if window width changed, user resized: update right_panel_width
                let expected_w = self.left_panel_width + self.right_panel_width + 8.0;
                if (inner_w - expected_w).abs() > 2.0 {
                    self.right_panel_width = (inner_w - self.left_panel_width - 8.0).max(200.0);
                }
            }
        }

        egui::SidePanel::left("left_panel")
            .exact_width(self.left_panel_width)
            .resizable(false)
            .show(ctx, |ui| {
                // ── Favorites ────────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⭐ Favorites").small());
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
                            .on_hover_text(fav.to_string_lossy())
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

                ui.separator();

                // ── Folder tree ──────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("📁 Tree").small());
                });
                let dnd_active = self.dnd_active;
                let dnd_drop_target = self.dnd_drop_target_prev.clone(); // use prev for display
                let dnd_sources: Vec<PathBuf> = self.dnd_sources.clone();
                let mut tree_hovered_drop: Option<PathBuf> = None;

                // Use a child_ui with a strict clip rect so the tree scroll
                // area cannot paint over the favorites section above.
                let tree_rect = ui.available_rect_before_wrap();
                let mut child_ui = ui.child_ui(tree_rect, egui::Layout::top_down(egui::Align::LEFT));
                child_ui.set_clip_rect(tree_rect);
                egui::ScrollArea::vertical()
                    .id_source("tree_scroll")
                    .auto_shrink([false, false])
                    .show(&mut child_ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.set_max_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let drives: Vec<PathBuf> = self
                            .available_drives
                            .iter()
                            .map(PathBuf::from)
                            .collect();
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
                            );
                        }
                    });
                // Advance the parent ui past the area we used
                ui.allocate_rect(tree_rect, egui::Sense::hover());
                if let Some(target) = tree_hovered_drop {
                    self.dnd_drop_target = Some(target);
                }
            });

        if let Some(path) = nav_from_panel {
            self.navigate_to(path);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // ── Tab bar ──────────────────────────────────────────────────
            let mut switch_to: Option<usize> = None;
            let mut close_idx: Option<usize> = None;
            let mut open_new_tab = false;
            let mut open_save_session = false;

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for i in 0..self.tabs.len() {
                    let is_active = i == self.active_tab;
                    let label_text = self.tabs[i].label();
                    let display = if label_text.len() > 20 {
                        format!("{}…", &label_text[..19])
                    } else {
                        label_text.clone()
                    };

                    let fill = if is_active {
                        egui::Color32::from_rgb(60, 60, 60)
                    } else {
                        egui::Color32::from_rgb(40, 40, 40)
                    };

                    let frame = egui::Frame::none()
                        .fill(fill)
                        .inner_margin(egui::Margin::symmetric(6.0, 3.0))
                        .rounding(egui::Rounding { nw: 4.0, ne: 4.0, sw: 0.0, se: 0.0 });

                    let resp = frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            let text_color = if is_active {
                                egui::Color32::WHITE
                            } else {
                                egui::Color32::GRAY
                            };
                            let tab_btn = ui.add(
                                egui::Button::new(
                                    egui::RichText::new(&display).color(text_color).small(),
                                )
                                .frame(false),
                            );
                            if tab_btn.clicked() {
                                switch_to = Some(i);
                            }
                            tab_btn.on_hover_text(self.tabs[i].path.to_string_lossy());

                            // Close button (only when more than 1 tab)
                            if self.tabs.len() > 1 {
                                let close = ui.add(
                                    egui::Button::new(
                                        egui::RichText::new("×").color(text_color).small(),
                                    )
                                    .frame(false),
                                );
                                if close.clicked() {
                                    close_idx = Some(i);
                                }
                            }
                        });
                    });

                    // Middle-click anywhere on the tab to close
                    if resp.response.middle_clicked() && self.tabs.len() > 1 {
                        close_idx = Some(i);
                    }
                }

                // "+" button to add a new tab
                if ui
                    .add(egui::Button::new(egui::RichText::new("+").small()).frame(false))
                    .on_hover_text("New tab")
                    .clicked()
                {
                    open_new_tab = true;
                }

                // Save session button
                ui.add_space(4.0);
                if ui
                    .add(egui::Button::new(egui::RichText::new("💾").small()).frame(false))
                    .on_hover_text("Save session")
                    .clicked()
                {
                    open_save_session = true;
                }
            });
            ui.add_space(1.0);

            // Process tab actions (after the borrow of self.tabs in the loop ends)
            if let Some(idx) = close_idx {
                self.close_tab(idx);
            } else if let Some(idx) = switch_to {
                self.switch_to_tab(idx);
            }
            if open_new_tab {
                self.new_tab(None);
            }
            if open_save_session {
                self.save_session_filename = "session.rsess".to_string();
                self.save_session_status = None;
                self.show_save_session_dialog = true;
            }

            // ── Save-session dialog ──────────────────────────────────────
            if self.show_save_session_dialog {
                let mut still_open = true;
                egui::Window::new("Save Session")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .open(&mut still_open)
                    .show(ctx, |ui| {
                        ui.label("Save current tabs to a session file.");
                        ui.label("You can restore it by running:");
                        ui.label("  rusplorer.exe <file>");
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label("File name:");
                            ui.text_edit_singleline(&mut self.save_session_filename);
                        });
                        if let Some(ref status) = self.save_session_status.clone() {
                            ui.colored_label(
                                if status.starts_with("Saved") {
                                    egui::Color32::from_rgb(50, 160, 50)
                                } else {
                                    egui::Color32::RED
                                },
                                status,
                            );
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                // Resolve path relative to exe directory
                                let exe_dir = std::env::current_exe()
                                    .ok()
                                    .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                                let save_path = exe_dir.join(&self.save_session_filename);
                                match self.save_session_to_file(&save_path, ctx) {
                                    Ok(()) => {
                                        self.save_session_status = Some(format!(
                                            "Saved to {}",
                                            save_path.display()
                                        ));
                                    }
                                    Err(e) => {
                                        self.save_session_status = Some(format!("Error: {e}"));
                                    }
                                }
                            }
                            if ui.button("Close").clicked() {
                                self.show_save_session_dialog = false;
                            }
                        });
                    });
                if !still_open {
                    self.show_save_session_dialog = false;
                }
            }

            // Drive selector with filter and navigation buttons
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

                // Filter in the middle
                ui.label("Filter:");
                ui.allocate_ui(egui::vec2(70.0, 20.0), |ui| {
                    ui.text_edit_singleline(&mut self.filter);
                });

                // Add space and push navigation buttons to the right
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("🔄").on_hover_text("Refresh").clicked() {
                        self.refresh_contents();
                    }

                    let forward_enabled = !self.forward_history.is_empty();
                    if ui
                        .add_enabled(forward_enabled, egui::Button::new("▶"))
                        .clicked()
                    {
                        self.go_forward();
                    }

                    let back_enabled = !self.back_history.is_empty();
                    if ui
                        .add_enabled(back_enabled, egui::Button::new("◀"))
                        .clicked()
                    {
                        self.go_back();
                    }
                });
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
                        ui.label("/");
                    }

                    if is_last {
                        // Current directory - not clickable, just plain text
                        ui.label(name);
                    } else {
                        // Parent directories - clickable pills; also valid DnD drop targets
                        let is_bc_drop = self.dnd_active
                            && self.dnd_drop_target_prev.as_ref() == Some(path);
                        let fill = if is_bc_drop {
                            egui::Color32::from_rgb(80, 200, 80)
                        } else {
                            egui::Color32::from_rgb(255, 245, 150)
                        };
                        let text_color = if is_bc_drop { egui::Color32::WHITE } else { egui::Color32::BLACK };
                        let button = egui::Button::new(
                            egui::RichText::new(name).color(text_color),
                        )
                        .fill(fill)
                        .frame(true);
                        let resp = ui.add(button);
                        // Same-frame DnD detection for breadcrumbs (use raw rect check;
                        // resp.hovered() is suppressed while a mouse button is held)
                        if self.dnd_active && !self.dnd_sources.contains(path) {
                            if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                                if resp.rect.contains(pos) {
                                    self.dnd_drop_target = Some(path.clone());
                                }
                            }
                        }
                        if resp.clicked() {
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

            // Table with proper column alignment
            let show_dates = self
                .show_date_columns
                .get(&self.current_path)
                .copied()
                .unwrap_or(false);
            let mut sort_changed = false;

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

            // Pre-filter entries for the table body
            let filter_lower = self.filter.to_lowercase();
            let filtered_entries: Vec<FileEntry> = self
                .contents
                .iter()
                .filter(|entry| {
                    entry.name.starts_with("[..]")
                        || self.filter.is_empty()
                        || entry.name.to_lowercase().contains(&filter_lower)
                })
                .cloned()
                .collect();
            let num_rows = filtered_entries.len();

            let table_builder = TableBuilder::new(ui)
                .striped(true)
                .resizable(false)
                .vscroll(true)
                .drag_to_scroll(false)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(name_col_w).clip(true))
                .column(Column::exact(size_col_w))
                .column(Column::exact(date_col_w));

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
                                let text = format!("Modified{}", arrow);
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
                                .on_hover_text("Show modification date")
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

                                let button = if is_drop_target {
                                    egui::Button::new(
                                        egui::RichText::new(&entry.name)
                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(egui::Color32::from_rgb(80, 200, 80))
                                    .frame(false)
                                } else if is_selected && is_in_clipboard {
                                      egui::Button::new(
                                          egui::RichText::new(&entry.name)
                                              .color(egui::Color32::WHITE)
                                              .italics(),
                                      )
                                      .fill(egui::Color32::from_rgb(100, 150, 255))
                                      .frame(false)
                                  } else if is_selected {
                                      egui::Button::new(
                                          egui::RichText::new(&entry.name)


                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(egui::Color32::from_rgb(100, 150, 255))
                                    .frame(false)
                                } else if is_in_clipboard && entry.is_dir {
                                    egui::Button::new(egui::RichText::new(&entry.name).italics())
                                        .fill(egui::Color32::from_rgb(255, 245, 150))
                                        .frame(false)
                                } else if is_in_clipboard {
                                    egui::Button::new(egui::RichText::new(&entry.name).italics())
                                        .frame(false)
                                } else if entry.name.starts_with("[..]") {
                                    egui::Button::new(&entry.name)
                                        .fill(egui::Color32::TRANSPARENT)
                                        .frame(false)
                                } else if entry.is_dir {
                                    egui::Button::new(&entry.name)
                                        .fill(egui::Color32::from_rgb(255, 245, 150))
                                        .frame(false)
                                } else {
                                    egui::Button::new(&entry.name).frame(false)
                                };

                                let button = button.sense(egui::Sense::click_and_drag());
                                let response = ui.horizontal(|ui| ui.add(button)).inner;

                                self.entry_rects.insert(entry.name.clone(), response.rect);
                                // Use direct cursor-rect check so hover works even during drag
                                let cursor_over = ui.input(|i| {
                                    i.pointer.hover_pos().map_or(false, |p| response.rect.contains(p))
                                });
                                if cursor_over || response.hovered() {
                                    self.any_button_hovered = true;
                                }

                                // Drag-and-drop: raw pointer state detection
                                // (avoids egui's drag_started_by/dragged_by which desync
                                //  after the blocking DoDragDrop OLE call)
                                let primary_down = ui.input(|i| i.pointer.primary_down());
                                let secondary_down = ui.input(|i| i.pointer.button_down(egui::PointerButton::Secondary));
                                let any_btn_down = primary_down || secondary_down;

                                // Detect new press on this entry
                                if cursor_over
                                    && any_btn_down
                                    && !self.dnd_active
                                    && !self.dnd_suppress
                                    && self.dnd_start_pos.is_none()
                                    && !entry.name.starts_with("[..]")
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
                                    if is_ctrl {
                                        if self.selected_entries.contains(&entry.name) {
                                            self.selected_entries.remove(&entry.name);
                                        } else {
                                            self.selected_entries.insert(entry.name.clone());
                                        }
                                    } else {
                                        self.selected_entries.clear();
                                        self.selected_entries.insert(entry.name.clone());
                                    }
                                }

                                if response.secondary_clicked() && !self.dnd_is_right_click {
                                    // Select the right-clicked entry if not already part of selection
                                    if !self.selected_entries.contains(&entry.name) {
                                        self.selected_entries.clear();
                                        self.selected_entries.insert(entry.name.clone());
                                    }
                                    self.show_context_menu = true;
                                    self.context_menu_entry = Some(entry.clone());
                                    self.context_menu_position =
                                        ui.input(|i| i.pointer.hover_pos().unwrap_or_default());
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
                                        Self::format_modified_time(modified)
                                    } else {
                                        String::new()
                                    };
                                    if !date_text.is_empty() {
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                let label = if is_in_clipboard {
                                                    egui::RichText::new(&date_text).weak().italics()
                                                } else {
                                                    egui::RichText::new(&date_text).weak()
                                                };
                                                ui.label(label);
                                            },
                                        );
                                    }
                                }
                            });
                        });
                    });

            // Handle rectangular selection (only when not dragging files)
            if !self.dnd_active {
            ctx.input(|i| {
                if let Some(pointer_pos) = i.pointer.hover_pos() {
                    if i.pointer.primary_pressed() && !self.any_button_hovered {
                        self.is_dragging_selection = true;
                        self.selection_drag_start = Some(pointer_pos);
                        self.selection_drag_current = Some(pointer_pos);
                        self.selection_before_drag = self.selected_entries.clone();
                    }
                    if self.is_dragging_selection && i.pointer.primary_down() {
                        self.selection_drag_current = Some(pointer_pos);
                        if let (Some(start), Some(end)) =
                            (self.selection_drag_start, self.selection_drag_current)
                        {
                            let sel_rect = egui::Rect::from_two_pos(start, end);
                            if i.modifiers.ctrl {
                                self.selected_entries = self.selection_before_drag.clone();
                            } else {
                                self.selected_entries.clear();
                            }
                            for (name, rect) in &self.entry_rects {
                                if sel_rect.intersects(*rect) && !name.starts_with("[..]") {
                                    self.selected_entries.insert(name.clone());
                                }
                            }
                        }
                    }
                    if self.is_dragging_selection && !i.pointer.primary_down() {
                        self.is_dragging_selection = false;
                        self.selection_drag_start = None;
                        self.selection_drag_current = None;
                        self.selection_before_drag.clear();
                    }
                }
            });
            } // end if !self.dnd_active

            if sort_changed {
                self.sort_contents();
                self.config.sort_column = self.sort_column.clone();
                self.config.sort_ascending = self.sort_ascending;
                self.config.show_date_columns = self
                    .show_date_columns
                    .iter()
                    .map(|(k, v)| (k.to_string_lossy().to_string(), *v))
                    .collect();
                self.config.save();
            }

            // Draw selection rectangle if dragging
            if let (Some(start), Some(current)) =
                (self.selection_drag_start, self.selection_drag_current)
            {
                let sel_rect = egui::Rect::from_two_pos(start, current);
                ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("selection_rect"),
                ))
                .rect_stroke(
                    sel_rect,
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(100, 150, 255)),
                );
                ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("selection_rect"),
                ))
                .rect_filled(
                    sel_rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(100, 150, 255, 30),
                );
            }

            // Handle drag-and-drop: detect release and perform action
            if self.dnd_active {
                let left_down = ctx.input(|i| i.pointer.primary_down());
                let right_down = ctx.input(|i| i.pointer.button_down(egui::PointerButton::Secondary));
                let pointer_down = if self.dnd_is_right_click { right_down } else { left_down };
                let hover_pos = ctx.input(|i| i.pointer.hover_pos());

                // Cursor left the window while dragging → OLE drag-and-drop to Explorer / other apps
                #[cfg(windows)]
                {
                    let screen_rect = ctx.input(|i| i.screen_rect());
                    let cursor_outside = match hover_pos {
                        Some(pos) => !screen_rect.contains(pos),
                        None => true,
                    };
                    let btn_held = if self.dnd_is_right_click { right_down } else { left_down };
                    if btn_held && cursor_outside && !self.dnd_sources.is_empty() {
                        let sources = self.dnd_sources.clone();
                        let is_right = self.dnd_is_right_click;
                        // Reset internal DnD state first
                        self.dnd_active = false;
                        self.dnd_is_right_click = false;
                        self.dnd_sources.clear();
                        self.dnd_label.clear();
                        self.dnd_start_pos = None;
                        self.dnd_drag_entry = None;
                        self.dnd_drop_target = None;
                        self.dnd_drop_target_prev = None;
                        self.dnd_suppress = true;
                        // Blocking OLE drag — pumps Windows messages until drop/cancel
                        let was_move = ole_drag_files_out(&sources, is_right);
                        if was_move {
                            self.selected_entries.clear();
                        }
                        self.refresh_contents();
                    }
                }

                if !pointer_down && self.dnd_active {
                    // Fallback: if no specific folder target, use current directory
                    let dest = self.dnd_drop_target.take()
                        .filter(|d| d.is_dir())
                        .unwrap_or_else(|| self.current_path.clone());

                    let sources: Vec<PathBuf> = self.dnd_sources
                        .iter()
                        .filter(|s| **s != dest)
                        .cloned()
                        .collect();

                    if !sources.is_empty() {
                        if self.dnd_is_right_click {
                            // Right-click drop: open the move/copy/shortcut menu
                            // Use latest pointer position (may be over the tree panel)
                            let drop_pos = ctx.input(|i|
                                i.pointer.latest_pos().or(i.pointer.hover_pos()).unwrap_or_default()
                            );
                            self.dnd_right_drop_menu = Some((sources, dest, drop_pos));
                        } else {
                            // Left-click drop: always move
                            let _ = Self::move_files(&sources, &dest);
                            self.selected_entries.clear();
                            self.refresh_contents();
                        }
                    }

                    self.dnd_active = false;
                    self.dnd_is_right_click = false;
                    self.dnd_sources.clear();
                    self.dnd_label.clear();
                    self.dnd_start_pos = None;
                    self.dnd_drag_entry = None;
                    self.dnd_drop_target = None;
                    self.dnd_suppress = true;
                }

                // Draw ghost label near cursor
                if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                    let painter = ctx.layer_painter(egui::LayerId::new(
                        egui::Order::Tooltip,
                        egui::Id::new("dnd_ghost"),
                    ));
                    let galley = painter.layout_no_wrap(
                        self.dnd_label.clone(),
                        egui::FontId::proportional(12.0),
                        egui::Color32::WHITE,
                    );
                    let text_rect = egui::Rect::from_min_size(
                        pos + egui::vec2(12.0, 12.0),
                        galley.size() + egui::vec2(12.0, 6.0),
                    );
                    painter.rect_filled(
                        text_rect,
                        4.0,
                        egui::Color32::from_rgba_unmultiplied(40, 40, 40, 220),
                    );
                    painter.rect_stroke(
                        text_rect,
                        4.0,
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(100, 150, 255)),
                    );
                    painter.galley(
                        text_rect.min + egui::vec2(6.0, 3.0),
                        galley,
                        egui::Color32::WHITE,
                    );
                    ctx.request_repaint();
                }
            }
        });

        // Drop menu context window
        if self.show_drop_menu && !self.dragged_files.is_empty() {
            let old_resp = egui::Window::new("drop_copy_move_menu")
                .collapsible(false)
                .resizable(false)
                .title_bar(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .frame(egui::Frame {
                    fill: egui::Color32::from_rgb(230, 240, 255),
                    stroke: egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                    inner_margin: egui::Margin::same(4.0),
                    ..Default::default()
                })
                .show(ctx, |ui| {
                    ui.style_mut().spacing.button_padding = egui::vec2(6.0, 3.0);
                    if ui.button("Move here").clicked() {
                        let files = self.dragged_files.clone();
                        let dest = self.current_path.clone();
                        std::thread::spawn(move || {
                            let _ = RusplorerApp::move_files(&files, &dest);
                        });
                        self.show_drop_menu = false;
                        self.dragged_files.clear();
                    }
                    if ui.button("Copy here").clicked() {
                        let files = self.dragged_files.clone();
                        let dest = self.current_path.clone();
                        std::thread::spawn(move || {
                            let _ = RusplorerApp::copy_files(&files, &dest);
                        });
                        self.show_drop_menu = false;
                        self.dragged_files.clear();
                    }
                });
            let old_clicked_outside = ctx.input(|i| {
                i.pointer.any_click()
                    && old_resp.as_ref().map_or(true, |r| {
                        i.pointer.interact_pos()
                            .map_or(false, |p| !r.response.rect.contains(p))
                    })
            });
            if old_clicked_outside {
                self.show_drop_menu = false;
                self.dragged_files.clear();
            }
        }

        // Refresh contents periodically to catch updates from background threads
        if self.dragged_files.is_empty() && !self.show_drop_menu {
            // Let the file watcher pick up changes
        }

        // ── Right-click drop menu (Move / Copy / Create Shortcut) ──────────
        if let Some((ref sources, ref dest, menu_pos)) = self.dnd_right_drop_menu.clone() {
            let sources = sources.clone();
            let dest = dest.clone();
            let mut action: Option<&str> = None;
            let is_same_drive = sources.first()
                .and_then(|s| s.components().next())
                .zip(dest.components().next())
                .map(|(a, b)| a == b)
                .unwrap_or(false);

            let win_resp = egui::Window::new("drop_action_menu")
                .collapsible(false)
                .resizable(false)
                .title_bar(false)
                .fixed_pos(menu_pos)
                .default_width(160.0)
                .frame(egui::Frame {
                    fill: egui::Color32::from_rgb(230, 240, 255),
                    stroke: egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                    inner_margin: egui::Margin::same(4.0),
                    ..Default::default()
                })
                .show(ctx, |ui| {
                    ui.style_mut().spacing.button_padding = egui::vec2(6.0, 3.0);
                    if ui.button(if is_same_drive { "Move here" } else { "Move here  (copy+delete)" }).clicked() {
                        action = Some("move");
                    }
                    if ui.button("Copy here").clicked() {
                        action = Some("copy");
                    }
                    #[cfg(windows)]
                    if ui.button("Create shortcut here").clicked() {
                        action = Some("shortcut");
                    }
                });
            // Dismiss on click outside the menu window
            let clicked_outside = ctx.input(|i| {
                i.pointer.any_click()
                    && win_resp.as_ref().map_or(true, |r| {
                        i.pointer.interact_pos()
                            .map_or(false, |p| !r.response.rect.contains(p))
                    })
            });
            match action {
                Some("move") => {
                    let _ = Self::move_files(&sources, &dest);
                    self.selected_entries.clear();
                    self.refresh_contents();
                    self.dnd_right_drop_menu = None;
                }
                Some("copy") => {
                    let _ = Self::copy_files(&sources, &dest);
                    self.refresh_contents();
                    self.dnd_right_drop_menu = None;
                }
                #[cfg(windows)]
                Some("shortcut") => {
                    for s in &sources {
                        let _ = create_lnk_shortcut(s, &dest);
                    }
                    self.refresh_contents();
                    self.dnd_right_drop_menu = None;
                }
                Some(_) | None if clicked_outside => {
                    self.dnd_right_drop_menu = None;
                }
                _ => {}
            }
        }

        // Context menu
        if self.show_context_menu {
            if let Some(ref entry) = self.context_menu_entry {
                let full_path = self.current_path.join(&entry.name);

                egui::Window::new("Context Menu")
                    .collapsible(false)
                    .resizable(false)
                    .title_bar(false)
                    .fixed_pos(self.context_menu_position)
                    .default_width(0.0)
                    .frame(egui::Frame {
                        fill: egui::Color32::from_rgb(200, 220, 255),
                        stroke: egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                        ..Default::default()
                    })
                    .show(ctx, |ui| {
                        ui.style_mut().spacing.button_padding = egui::vec2(4.0, 2.0);

                        // Open with VS Code
                        if (entry.is_dir || Self::is_code_file(&full_path))
                            && ui.button("Open with Code").clicked()
                        {
                            let _ = std::process::Command::new("code").arg(&full_path).spawn();
                            self.show_context_menu = false;
                        }

                        // Extract here
                        if Self::is_archive(&full_path) && ui.button("Extract here").clicked() {
                            self.extract_archive_path = full_path.clone();
                            self.show_extract_dialog = true;
                            self.show_context_menu = false;
                        }

                        // Add to archive
                        if ui.button("Add to archive").clicked() {
                            self.files_to_archive.clear();
                            if !self.selected_entries.is_empty() {
                                for name in &self.selected_entries {
                                    self.files_to_archive.push(self.current_path.join(name));
                                }
                            } else {
                                self.files_to_archive.push(full_path.clone());
                            }

                            // Default archive name based on first item
                            let stem = if let Some(first) = self.files_to_archive.first() {
                                first
                                    .file_stem()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string()
                            } else {
                                "archive".to_string()
                            };
                            self.archive_name_buffer = stem;
                            self.show_archive_dialog = true;
                            self.show_context_menu = false;
                        }

                        ui.separator();

                        // Copy full path
                        if ui.button("📋 Copy full path").clicked() {
                            if let Ok(mut clipboard) = Clipboard::new() {
                                let _ = clipboard.set_text(full_path.to_string_lossy().to_string());
                            }
                            self.show_context_menu = false;
                        }

                        // Rename
                        if !entry.name.starts_with("[..]") && ui.button("Rename").clicked() {
                            self.rename_buffer = entry.name.clone();
                            self.show_rename_dialog = true;
                            self.show_context_menu = false;
                        }

                        // Properties
                        if ui.button("Properties").clicked() {
                            let _ = std::process::Command::new("explorer")
                                .args(&["/select,", &full_path.to_string_lossy()])
                                .spawn();
                            self.show_context_menu = false;
                        }

                        ui.separator();

                        if ui.button("Cancel").clicked() {
                            self.show_context_menu = false;
                        }
                    });
            }

            // Close context menu if clicked elsewhere
            if ctx.input(|i| i.pointer.primary_clicked() || i.key_pressed(egui::Key::Escape)) {
                self.show_context_menu = false;
            }
        }

        // Archive dialog
        if self.show_archive_dialog {
            // Draw semi-transparent backdrop
            let screen_rect = ctx.screen_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::PanelResizeLine,
                egui::Id::new("archive_backdrop"),
            ));
            painter.rect_filled(screen_rect, 0.0, egui::Color32::from_black_alpha(128));

            egui::Window::new("Add to archive")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Archive name:");
                        ui.text_edit_singleline(&mut self.archive_name_buffer);
                    });

                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        ui.label("Format:");
                        ui.selectable_value(&mut self.archive_type, 0, "7z");
                        ui.selectable_value(&mut self.archive_type, 1, "zip");
                    });

                    ui.horizontal(|ui| {
                        ui.label("Compression:");
                        ui.selectable_value(&mut self.compression_level, 0, "Store");
                        ui.selectable_value(&mut self.compression_level, 1, "Medium");
                        ui.selectable_value(&mut self.compression_level, 2, "High");
                    });

                    ui.add_space(4.0);

                    ui.label(format!(
                        "{} item(s) to archive",
                        self.files_to_archive.len()
                    ));

                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        if ui.button("Compress").clicked() {
                            let ext = match self.archive_type {
                                0 => "7z",
                                _ => "zip",
                            };
                            let format_flag = match self.archive_type {
                                0 => "-t7z",
                                _ => "-tzip",
                            };
                            let level_flag = match self.compression_level {
                                0 => "-mx0",
                                1 => "-mx5",
                                _ => "-mx9",
                            };

                            let archive_path = self
                                .current_path
                                .join(format!("{}.{}", self.archive_name_buffer, ext));
                            let archive_str = archive_path.to_string_lossy().to_string();

                            let archive_filename = format!("{}.{}", self.archive_name_buffer, ext);
                            let files_clone = self.files_to_archive.clone();
                            let (done_tx, done_rx) = channel();
                            let archive_str_clone = archive_str.clone();
                            let format_flag = format_flag.to_string();
                            let level_flag = level_flag.to_string();

                            std::thread::spawn(move || {
                                let mut cmd =
                                    std::process::Command::new("C:\\Program Files\\7-Zip\\7z.exe");
                                cmd.args(&["a", &format_flag, &level_flag, &archive_str_clone]);
                                for f in &files_clone {
                                    cmd.arg(f);
                                }
                                let result = cmd.spawn().or_else(|_| {
                                    let mut cmd2 = std::process::Command::new("7z.exe");
                                    cmd2.args(&[
                                        "a",
                                        &format_flag,
                                        &level_flag,
                                        &archive_str_clone,
                                    ]);
                                    for f in &files_clone {
                                        cmd2.arg(f);
                                    }
                                    cmd2.spawn()
                                });
                                if let Ok(mut child) = result {
                                    let _ = child.wait();
                                }
                                let _ = done_tx.send(archive_filename);
                            });

                            self.archive_done_receiver = Some(done_rx);
                            self.show_archive_dialog = false;
                            self.files_to_archive.clear();
                        }

                        if ui.button("Cancel").clicked() {
                            self.show_archive_dialog = false;
                            self.files_to_archive.clear();
                        }
                    });
                });
        }

        // Start extraction if dialog was shown (one-time trigger)
        if self.show_extract_dialog && self.extract_done_receiver.is_none() {
            let archive_path = self.extract_archive_path.clone();
            let dest = self.current_path.clone();
            let (done_tx, done_rx) = channel();

            std::thread::spawn(move || {
                let dest_str = dest.to_string_lossy().to_string();
                let archive_str = archive_path.to_string_lossy().to_string();

                let result = std::process::Command::new("C:\\Program Files\\7-Zip\\7z.exe")
                    .args(&["x", &archive_str, &format!("-o{}", dest_str)])
                    .spawn()
                    .or_else(|_| {
                        std::process::Command::new("7z.exe")
                            .args(&["x", &archive_str, &format!("-o{}", dest_str)])
                            .spawn()
                    });

                if let Ok(mut child) = result {
                    let _ = child.wait();
                }
                let _ = done_tx.send(());
            });

            self.extract_done_receiver = Some(done_rx);
        }

        // Rename dialog
        if self.show_rename_dialog {
            if let Some(entry) = self.context_menu_entry.clone() {
                let entry_name = entry.name.clone();
                egui::Window::new("Rename")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label("New name:");
                        let response = ui.text_edit_singleline(&mut self.rename_buffer);

                        // Auto-focus the text field
                        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            // Perform rename
                            let old_path = self.current_path.join(&entry_name);
                            let new_path = self.current_path.join(&self.rename_buffer);
                            if let Err(_) = std::fs::rename(&old_path, &new_path) {
                                // Error handling could be improved
                            }
                            self.show_rename_dialog = false;
                            self.refresh_contents();
                        }

                        ui.horizontal(|ui| {
                            if ui.button("OK").clicked() {
                                let old_path = self.current_path.join(&entry_name);
                                let new_path = self.current_path.join(&self.rename_buffer);
                                if let Err(_) = std::fs::rename(&old_path, &new_path) {
                                    // Error handling could be improved
                                }
                                self.show_rename_dialog = false;
                                self.refresh_contents();
                            }

                            if ui.button("Cancel").clicked() {
                                self.show_rename_dialog = false;
                            }
                        });
                    });
            }
        }

        // Extract dialog
        if self.show_extract_dialog {
            // Draw semi-transparent backdrop
            let screen_rect = ctx.screen_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::PanelResizeLine,
                egui::Id::new("extract_backdrop"),
            ));
            painter.rect_filled(screen_rect, 0.0, egui::Color32::from_black_alpha(128));

            let archive_name = self
                .extract_archive_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            egui::Window::new("Extracting...")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(format!("Extracting: {}", archive_name));
                    ui.label("Please wait...");
                });
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

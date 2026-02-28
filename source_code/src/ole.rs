/// Windows OLE drag-and-drop, drop-target registration, and virtual-desktop helpers.

use std::path::PathBuf;

// ── Virtual desktop helpers ──────────────────────────────────────────────────

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

/// Find the top-level window belonging to the current thread.
/// Unlike FindWindowW, this never returns another process's window.
#[cfg(windows)]
pub fn find_own_hwnd() -> Option<winapi::shared::windef::HWND> {
    use winapi::um::processthreadsapi::GetCurrentThreadId;
    use winapi::um::winuser::EnumThreadWindows;
    use winapi::shared::windef::HWND;
    use winapi::shared::minwindef::{BOOL, LPARAM, TRUE};

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        if unsafe { winapi::um::winuser::IsWindowVisible(hwnd) } != 0 {
            let out = lparam as *mut HWND;
            unsafe { *out = hwnd };
            return 0; // stop enumerating
        }
        TRUE
    }

    unsafe {
        let mut result: HWND = std::ptr::null_mut();
        EnumThreadWindows(
            GetCurrentThreadId(),
            Some(callback),
            &mut result as *mut HWND as LPARAM,
        );
        if result.is_null() { None } else { Some(result) }
    }
}

/// Move own window to the "Rusplorer" virtual desktop.
/// Uses the public IVirtualDesktopManager COM API — works in-process (no E_ACCESSDENIED).
/// Returns true if the move succeeded OR if no "Rusplorer" desktop exists (no point retrying).
#[cfg(windows)]
pub fn try_move_to_rusplorer_desktop() -> bool {
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

    unsafe {
        let hwnd_raw = match find_own_hwnd() {
            Some(h) => h,
            None => return false, // Window not visible yet — retry later
        };
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

// ── OLE drag-out ─────────────────────────────────────────────────────────────

/// Initiate an OLE drag-and-drop of the given files out to other applications (e.g. Explorer).
/// Blocks until the user drops or cancels. Returns `true` when the target performed a *move*.
/// `right_button`: if true, tracks MK_RBUTTON instead of MK_LBUTTON.
#[cfg(windows)]
pub fn ole_drag_files_out(files: &[PathBuf], right_button: bool) -> bool {
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

// ── OLE drop-target registration ─────────────────────────────────────────────

/// Returns the IDropTarget COM object (must be kept alive for the duration of the session).
/// Returns None if registration failed.
#[cfg(windows)]
pub fn register_ole_drop_target(
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
        let _ = RevokeDragDrop(hwnd);
        if RegisterDragDrop(hwnd, &drop_target).is_ok() {
            Some(drop_target)
        } else {
            None
        }
    }
}

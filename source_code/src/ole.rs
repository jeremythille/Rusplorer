/// Windows OLE drag-and-drop, drop-target registration, and virtual-desktop helpers.

use std::path::PathBuf;

/// Write a diagnostic line to `%TEMP%\rusplorer_dnd_<pid>.log`.
/// Each call appends one line with a timestamp.
#[cfg(windows)]
pub fn log_dnd(msg: &str) {
    use std::io::Write;
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("rusplorer_dnd_{pid}.log"));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{:.3}] {msg}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64() % 100000.0);
    }
}

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

// ── Custom clipboard format for right-button drag signalling ─────────────────

/// Register / retrieve the custom clipboard format used to signal a right-button
/// OLE drag.  The returned ID is stable for the lifetime of the process (and
/// across all processes in the same Windows session that use the same name).
#[cfg(windows)]
fn rusplorer_right_drag_format() -> u16 {
    use std::sync::OnceLock;
    static FMT: OnceLock<u16> = OnceLock::new();
    *FMT.get_or_init(|| {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        let name: Vec<u16> = OsStr::new("RusplorerRightDrag")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe { winapi::um::winuser::RegisterClipboardFormatW(name.as_ptr()) as u16 }
    })
}

/// Register / retrieve the custom clipboard format used to mark ANY Rusplorer
/// OLE drag-out.  When the drop target (another Rusplorer window) sees this
/// format it refuses the drop so the drag can reach the application behind it.
#[cfg(windows)]
fn rusplorer_source_drag_format() -> u16 {
    use std::sync::OnceLock;
    static FMT: OnceLock<u16> = OnceLock::new();
    *FMT.get_or_init(|| {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        let name: Vec<u16> = OsStr::new("RusplorerDragSource")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe { winapi::um::winuser::RegisterClipboardFormatW(name.as_ptr()) as u16 }
    })
}

/// Register / retrieve the standard shell "Shell IDList Array" clipboard format
/// (CFSTR_SHELLIDLIST).  Explorer always offers this format on file drags; the
/// desktop / Explorer drop targets need it to enable the "Create shortcut here"
/// verb in the right-drag menu (plain CF_HDROP alone is not enough).
#[cfg(windows)]
fn shell_idlist_format() -> u16 {
    use std::sync::OnceLock;
    static FMT: OnceLock<u16> = OnceLock::new();
    *FMT.get_or_init(|| {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        let name: Vec<u16> = OsStr::new("Shell IDList Array")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe { winapi::um::winuser::RegisterClipboardFormatW(name.as_ptr()) as u16 }
    })
}

/// Register / retrieve CFSTR_PREFERREDDROPEFFECT.
///
/// This lets the source suggest copy/move/link as the default shell action for
/// drag-and-drop. On right-button drags, setting this to LINK helps Explorer/
/// desktop expose "Create shortcut here" in the shortcut menu.
#[cfg(windows)]
fn preferred_drop_effect_format() -> u16 {
    use std::sync::OnceLock;
    static FMT: OnceLock<u16> = OnceLock::new();
    *FMT.get_or_init(|| {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        let name: Vec<u16> = OsStr::new("Preferred DropEffect")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe { winapi::um::winuser::RegisterClipboardFormatW(name.as_ptr()) as u16 }
    })
}

/// Build a CFSTR_SHELLIDLIST (CIDA) blob for the given files.
///
/// All files must share the same parent directory (they always do in Rusplorer —
/// every drag originates from the current folder).  Returns `None` on any failure
/// so the caller can fall back to a CF_HDROP-only data object with no regression.
///
/// CIDA layout:
///   UINT cidl;                 // number of child items
///   UINT aoffset[cidl + 1];    // [0] = parent PIDL, [1..=cidl] = child PIDLs
///   <parent absolute PIDL bytes>
///   <child-relative PIDL bytes>...
#[cfg(windows)]
fn build_shell_idlist(files: &[PathBuf]) -> Option<Vec<u8>> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{ILCreateFromPathW, ILFindLastID, ILFree, ILGetSize};

    if files.is_empty() {
        return None;
    }
    let parent = files[0].parent()?;
    // Require all files to share the parent directory.
    for f in files {
        if f.parent()? != parent {
            return None;
        }
    }

    let to_wide = |p: &std::path::Path| -> Vec<u16> {
        OsStr::new(p.as_os_str())
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };

    unsafe {
        // Parent absolute PIDL.
        let parent_w = to_wide(parent);
        let parent_pidl = ILCreateFromPathW(PCWSTR(parent_w.as_ptr()));
        if parent_pidl.is_null() {
            return None;
        }
        // Guard to free all allocated PIDLs on any early return.
        let mut child_pidls: Vec<*const ITEMIDLIST> = Vec::with_capacity(files.len());
        let free_all = |parent: *const ITEMIDLIST, children: &[*const ITEMIDLIST]| {
            ILFree(Some(parent));
            for c in children {
                ILFree(Some(*c));
            }
        };

        let parent_size = ILGetSize(Some(parent_pidl)) as usize;

        // Child PIDLs (absolute), plus the relative (last id) slice info.
        let mut child_rel: Vec<(*const u8, usize)> = Vec::with_capacity(files.len());
        for f in files {
            let fw = to_wide(f);
            let abs = ILCreateFromPathW(PCWSTR(fw.as_ptr()));
            if abs.is_null() {
                free_all(parent_pidl, &child_pidls);
                return None;
            }
            child_pidls.push(abs);
            // Relative child PIDL = the last SHITEMID within the absolute PIDL.
            let last = ILFindLastID(abs);
            let rel_size = ILGetSize(Some(last)) as usize; // includes 2-byte terminator
            child_rel.push((last as *const u8, rel_size));
        }

        let cidl = files.len();
        let header_size = 4 * (cidl + 2); // cidl (UINT) + (cidl+1) offsets
        let total = header_size
            + parent_size
            + child_rel.iter().map(|(_, s)| *s).sum::<usize>();

        let mut blob = vec![0u8; total];
        // cidl
        blob[0..4].copy_from_slice(&(cidl as u32).to_le_bytes());
        // offsets
        let mut data_off = header_size;
        // aoffset[0] = parent
        blob[4..8].copy_from_slice(&(data_off as u32).to_le_bytes());
        std::ptr::copy_nonoverlapping(
            parent_pidl as *const u8,
            blob.as_mut_ptr().add(data_off),
            parent_size,
        );
        data_off += parent_size;
        // aoffset[1..=cidl] = children
        for (i, (rel_ptr, rel_size)) in child_rel.iter().enumerate() {
            let off_pos = 8 + i * 4;
            blob[off_pos..off_pos + 4].copy_from_slice(&(data_off as u32).to_le_bytes());
            std::ptr::copy_nonoverlapping(
                *rel_ptr,
                blob.as_mut_ptr().add(data_off),
                *rel_size,
            );
            data_off += *rel_size;
        }

        free_all(parent_pidl, &child_pidls);
        Some(blob)
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
        DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
    };
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;

    const MK_LBUTTON: u32 = 0x0001;
    const MK_RBUTTON: u32 = 0x0002;
    const DRAGDROP_S_DROP: HRESULT = HRESULT(0x00040100_i32);
    const DRAGDROP_S_CANCEL: HRESULT = HRESULT(0x00040101_i32);
    const DRAGDROP_S_USEDEFAULTCURSORS: HRESULT = HRESULT(0x00040102_i32);
    const CF_HDROP_RAW: u16 = 15;

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

    // ── IDataObject (CF_HDROP + optional right-drag marker + source marker) ─
    #[implement(IDataObject)]
    struct HdropData {
        blob: Vec<u8>,
        /// Custom clipboard format ID for right-button drag (0 = not a right drag).
        right_drag_fmt: u16,
        /// Custom clipboard format ID marking this drag as originating from Rusplorer.
        /// Always non-zero; used by other Rusplorer windows to refuse the drop.
        source_drag_fmt: u16,
        /// Standard CFSTR_SHELLIDLIST format ID (0 = not available).
        shellidlist_fmt: u16,
        /// CIDA blob for CFSTR_SHELLIDLIST (empty when shellidlist_fmt == 0).
        cida: Vec<u8>,
        /// Standard CFSTR_PREFERREDDROPEFFECT format ID.
        preferred_effect_fmt: u16,
        /// Suggested drop effect as a DWORD DROPEFFECT_* value.
        preferred_effect: u32,
    }

    impl IDataObject_Impl for HdropData_Impl {
        fn GetData(
            &self,
            pformatetcin: *const FORMATETC,
        ) -> windows::core::Result<STGMEDIUM> {
            unsafe {
                let fmt = &*pformatetcin;
                if fmt.cfFormat == CF_HDROP_RAW {
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
                } else if self.shellidlist_fmt != 0 && fmt.cfFormat == self.shellidlist_fmt {
                    // CFSTR_SHELLIDLIST (CIDA) — enables the "Create shortcut here"
                    // verb on the desktop / Explorer drop targets.
                    let hmem = GlobalAlloc(
                        GLOBAL_ALLOC_FLAGS(0x0042),
                        self.cida.len(),
                    )?;
                    let ptr = GlobalLock(hmem) as *mut u8;
                    if ptr.is_null() {
                        return Err(windows::core::Error::from_hresult(E_NOTIMPL));
                    }
                    std::ptr::copy_nonoverlapping(self.cida.as_ptr(), ptr, self.cida.len());
                    let _ = GlobalUnlock(hmem);
                    let mut medium: STGMEDIUM = std::mem::zeroed();
                    medium.tymed = TYMED_HGLOBAL.0 as u32;
                    medium.u.hGlobal = hmem;
                    Ok(medium)
                } else if fmt.cfFormat == self.preferred_effect_fmt {
                    let hmem = GlobalAlloc(GLOBAL_ALLOC_FLAGS(0x0042), 4)?;
                    let ptr = GlobalLock(hmem) as *mut u8;
                    if ptr.is_null() {
                        return Err(windows::core::Error::from_hresult(E_NOTIMPL));
                    }
                    let bytes = self.preferred_effect.to_le_bytes();
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, 4);
                    let _ = GlobalUnlock(hmem);
                    let mut medium: STGMEDIUM = std::mem::zeroed();
                    medium.tymed = TYMED_HGLOBAL.0 as u32;
                    medium.u.hGlobal = hmem;
                    Ok(medium)
                } else if fmt.cfFormat == self.source_drag_fmt {
                    // Payload is the source PID (u32 LE) so the drop target can
                    // distinguish drags from THIS process (must refuse to avoid
                    // recursive self-feeding) from drags from ANOTHER Rusplorer
                    // process (must accept — that's a real cross-window copy).
                    let hmem = GlobalAlloc(GLOBAL_ALLOC_FLAGS(0x0042), 4)?;
                    let ptr = GlobalLock(hmem) as *mut u8;
                    if !ptr.is_null() {
                        let pid = std::process::id().to_le_bytes();
                        std::ptr::copy_nonoverlapping(pid.as_ptr(), ptr, 4);
                        let _ = GlobalUnlock(hmem);
                    }
                    let mut medium: STGMEDIUM = std::mem::zeroed();
                    medium.tymed = TYMED_HGLOBAL.0 as u32;
                    medium.u.hGlobal = hmem;
                    Ok(medium)
                } else if self.right_drag_fmt != 0 && fmt.cfFormat == self.right_drag_fmt {
                    // Tiny marker — only existence is checked.
                    let hmem = GlobalAlloc(GLOBAL_ALLOC_FLAGS(0x0042), 1)?;
                    let ptr = GlobalLock(hmem) as *mut u8;
                    if !ptr.is_null() {
                        *ptr = 1;
                        let _ = GlobalUnlock(hmem);
                    }
                    let mut medium: STGMEDIUM = std::mem::zeroed();
                    medium.tymed = TYMED_HGLOBAL.0 as u32;
                    medium.u.hGlobal = hmem;
                    Ok(medium)
                } else {
                    Err(windows::core::Error::from_hresult(E_NOTIMPL))
                }
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
                let cf = (*pformatetc).cfFormat;
                if cf == CF_HDROP_RAW {
                    S_OK
                } else if self.shellidlist_fmt != 0 && cf == self.shellidlist_fmt {
                    S_OK
                } else if cf == self.preferred_effect_fmt {
                    S_OK
                } else if self.right_drag_fmt != 0 && cf == self.right_drag_fmt {
                    S_OK
                } else if cf == self.source_drag_fmt {
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
            let mut fmts = vec![FORMATETC {
                cfFormat: CF_HDROP_RAW,
                ptd: std::ptr::null_mut(),
                dwAspect: 1, // DVASPECT_CONTENT
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            }];
            if self.shellidlist_fmt != 0 {
                // Offer the Shell IDList Array first — Explorer-parity ordering;
                // required for the desktop's "Create shortcut here" verb.
                fmts.insert(0, FORMATETC {
                    cfFormat: self.shellidlist_fmt,
                    ptd: std::ptr::null_mut(),
                    dwAspect: 1,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                });
            }
            if self.right_drag_fmt != 0 {
                fmts.push(FORMATETC {
                    cfFormat: self.right_drag_fmt,
                    ptd: std::ptr::null_mut(),
                    dwAspect: 1,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                });
            }
            fmts.push(FORMATETC {
                cfFormat: self.preferred_effect_fmt,
                ptd: std::ptr::null_mut(),
                dwAspect: 1,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            });
            fmts.push(FORMATETC {
                cfFormat: self.source_drag_fmt,
                ptd: std::ptr::null_mut(),
                dwAspect: 1,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            });
            unsafe { SHCreateStdEnumFmtEtc(&fmts) }
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
    let right_drag_fmt = if right_button { rusplorer_right_drag_format() } else { 0 };
    let source_drag_fmt = rusplorer_source_drag_format();
    let preferred_effect_fmt = preferred_drop_effect_format();
    // Prefer LINK for right-drag so the shell can offer shortcut creation.
    let preferred_effect = if right_button {
        DROPEFFECT_LINK.0 as u32
    } else {
        DROPEFFECT_COPY.0 as u32
    };
    // Build the optional Shell IDList Array (CIDA). Any failure falls back to a
    // CF_HDROP-only data object with no behavioural regression.
    let (shellidlist_fmt, cida) = match build_shell_idlist(files) {
        Some(blob) if !blob.is_empty() => (shell_idlist_format(), blob),
        _ => (0u16, Vec::new()),
    };
    let data_obj: IDataObject = HdropData {
        blob,
        right_drag_fmt,
        source_drag_fmt,
        shellidlist_fmt,
        cida,
        preferred_effect_fmt,
        preferred_effect,
    }
    .into();
    let source: IDropSource = DropSource { button_mask: track_button }.into();
    let mut effect = DROPEFFECT_NONE;
    let hr = unsafe {
        DoDragDrop(
            &data_obj,
            &source,
            // Include LINK so shell right-drag menus can offer
            // "Create shortcut here" on Desktop/Explorer targets.
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
            &mut effect,
        )
    };
    log_dnd(&format!(
        "DoDragDrop: right={} hr=0x{:08X} effect=0x{:08X}",
        right_button,
        hr.0 as u32,
        effect.0 as u32
    ));
    hr == DRAGDROP_S_DROP && effect == DROPEFFECT_MOVE
}

// ── OLE drop-target registration ─────────────────────────────────────────────

/// Returns the IDropTarget COM object (must be kept alive for the duration of the session).
/// Returns:
///   `None`        — registration failed entirely (fatal, don't retry)
///   `Some(false)` — registered but winit's target was not yet present; retry next frame
///   `Some(true)`  — successfully revoked winit's target and installed ours
#[cfg(windows)]
pub fn register_ole_drop_target(
    hwnd_raw: *mut std::ffi::c_void,
    sender: std::sync::mpsc::Sender<Vec<PathBuf>>,
    right_click_sender: std::sync::mpsc::Sender<Vec<PathBuf>>,
    drag_in_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<(windows::Win32::System::Ole::IDropTarget, bool)> {
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
        /// Set in DragEnter (button is guaranteed held) — more reliable than last_key_state for Drop.
        is_right_drag: std::cell::Cell<bool>,
        /// Set when DragEnter determines this is a Rusplorer-originated drag (must refuse).
        refused_drag: std::cell::Cell<bool>,
        drag_in_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
            // Refuse drops whose IDataObject was produced by THIS very Rusplorer
            // process (PID payload of the source_drag_fmt marker matches our
            // own PID). Otherwise a botched cross-window drag whose drop happens
            // to land back on our own window can self-feed our IDropTarget,
            // copying/moving the dragged item into self.current_path.
            //
            // Drags coming from a DIFFERENT Rusplorer process must NOT be
            // refused — that's a legitimate cross-instance copy.
            let from_self = if let Some(obj) = pdataobj {
                let fmt = FORMATETC {
                    cfFormat: rusplorer_source_drag_format(),
                    ptd: std::ptr::null_mut(),
                    dwAspect: 1,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                };
                unsafe {
                    match obj.GetData(&fmt) {
                        Ok(medium) => {
                            let hmem = medium.u.hGlobal;
                            let locked = GlobalLock(hmem) as *const u8;
                            let same_pid = if !locked.is_null() {
                                let src_pid = std::ptr::read_unaligned(locked as *const u32);
                                let _ = GlobalUnlock(hmem);
                                src_pid == std::process::id()
                            } else {
                                // Couldn't read payload — be conservative and
                                // treat as foreign so cross-instance drops work.
                                false
                            };
                            same_pid
                        }
                        Err(_) => false,
                    }
                }
            } else {
                false
            };
            if from_self {
                self.refused_drag.set(true);
                log_dnd("DragEnter: refusing own-process IDataObject");
                unsafe { *pdweffect = DROPEFFECT_NONE; }
                return Ok(());
            }
            // Detect right-button drag via multiple signals:
            // 1. Custom clipboard format (reliable across processes)
            // 2. grfkeystate from OLE (may miss MK_RBUTTON in some cases)
            // 3. Hardware key state (most reliable)
            let right_via_format = if let Some(obj) = pdataobj {
                let fmt = FORMATETC {
                    cfFormat: rusplorer_right_drag_format(),
                    ptd: std::ptr::null_mut(),
                    dwAspect: 1,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                };
                unsafe { obj.QueryGetData(&fmt) == S_OK }
            } else {
                false
            };
            let hw_right = unsafe {
                windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x02)
            } & (0x8000u16 as i16) != 0;
            let is_right = right_via_format
                || grfkeystate.0 & MK_RBUTTON != 0
                || hw_right;
            self.is_right_drag.set(is_right);
            log_dnd(&format!("DragEnter: fmt={right_via_format} keystate=0x{:04X} hw={hw_right} => is_right={is_right}", grfkeystate.0));
            self.drag_in_active.store(true, std::sync::atomic::Ordering::SeqCst);
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
            // If we refused this drag in DragEnter, keep refusing.
            if self.refused_drag.get() {
                unsafe { *pdweffect = DROPEFFECT_NONE; }
                return Ok(());
            }
            self.last_key_state.set(grfkeystate.0);
            // Latch: if right button is seen in ANY DragOver, remember it.
            // DragEnter may have missed MK_RBUTTON on some timing paths.
            if grfkeystate.0 & MK_RBUTTON != 0 {
                if !self.is_right_drag.get() {
                    log_dnd(&format!("DragOver: latching is_right_drag via keystate 0x{:04X}", grfkeystate.0));
                }
                self.is_right_drag.set(true);
            }
            unsafe { *pdweffect = DROPEFFECT_COPY; }
            Ok(())
        }

        fn DragLeave(&self) -> windows::core::Result<()> {
            log_dnd(&format!("DragLeave: was_right={} was_refused={}", self.is_right_drag.get(), self.refused_drag.get()));
            self.is_right_drag.set(false);
            self.refused_drag.set(false);
            self.drag_in_active.store(false, std::sync::atomic::Ordering::SeqCst);
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
                // Refuse drops from this very Rusplorer process — see DragEnter.
                if self.refused_drag.get() {
                    log_dnd("Drop: refused (own process)");
                    self.refused_drag.set(false);
                    self.drag_in_active.store(false, std::sync::atomic::Ordering::SeqCst);
                    return Ok(());
                }
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

                self.drag_in_active.store(false, std::sync::atomic::Ordering::SeqCst);
                if !files.is_empty() {
                    // Some apps (e.g. 7-Zip) extract to a private temp folder, call
                    // Drop(), and immediately delete that temp folder once we return.
                    // Our async copy job runs *after* we return, so the files are gone.
                    // Fix: synchronously copy any temp-resident files into our own
                    // stable temp dir RIGHT NOW, before returning from Drop().
                    let temp_dir = std::env::temp_dir();
                    let staging = temp_dir.join("rusplorer_drop");
                    let files: Vec<PathBuf> = files.into_iter().map(|src| {
                        let in_temp = src.starts_with(&temp_dir);
                        if in_temp {
                            let _ = std::fs::create_dir_all(&staging);
                            if let Some(name) = src.file_name() {
                                let dst = staging.join(name);
                                match std::fs::copy(&src, &dst) {
                                    Ok(_) => {
                                        log_dnd(&format!("  staged: {} -> {}", src.display(), dst.display()));
                                        return dst;
                                    }
                                    Err(e) => log_dnd(&format!("  stage FAIL {}: {e}", src.display())),
                                }
                            }
                        }
                        src
                    }).collect();

                    // Determine right-drag via multiple methods:
                    // 1. Latched flag from DragEnter/DragOver (set while button held)
                    // 2. Custom format via GetData (more reliably marshaled cross-process
                    //    than QueryGetData)
                    let right_via_format = {
                        let fmt = FORMATETC {
                            cfFormat: rusplorer_right_drag_format(),
                            ptd: std::ptr::null_mut(),
                            dwAspect: 1,
                            lindex: -1,
                            tymed: TYMED_HGLOBAL.0 as u32,
                        };
                        obj.GetData(&fmt).is_ok()
                    };
                    let is_right = self.is_right_drag.get() || right_via_format;
                    log_dnd(&format!("Drop: {} file(s), latched={} fmt={right_via_format} => is_right={is_right}", files.len(), self.is_right_drag.get()));
                    for f in &files { log_dnd(&format!("  path: {}", f.display())); }
                    if is_right {
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
        is_right_drag: std::cell::Cell::new(false),
        refused_drag: std::cell::Cell::new(false),
        drag_in_active,
    }.into();
    unsafe {
        // OleInitialize is called by winit (drag_and_drop=true).
        // We revoke winit's IDropTarget and install our own.
        // RevokeDragDrop returns an error if nothing is registered yet
        // (winit registers slightly later than our first update frame).
        // We return Some(false) in that case so the caller retries next frame.
        let hwnd = HWND(hwnd_raw);
        log_dnd(&format!("Registering IDropTarget on HWND={hwnd_raw:?}"));
        let revoked = RevokeDragDrop(hwnd).is_ok();
        log_dnd(&format!("RevokeDragDrop ok={revoked}"));
        let reg_hr = RegisterDragDrop(hwnd, &drop_target);
        log_dnd(&format!(
            "RegisterDragDrop -> {}",
            if reg_hr.is_ok() { "OK" } else { "FAILED" }
        ));
        if reg_hr.is_ok() {
            Some((drop_target, revoked))
        } else {
            None
        }
    }
}

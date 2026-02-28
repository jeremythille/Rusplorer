/// Windows shell shortcuts (.lnk) — create and resolve.

use std::path::{Path, PathBuf};

/// Create a Windows .lnk shortcut pointing at `target` inside `dest_dir`.
#[cfg(windows)]
pub fn create_lnk_shortcut(target: &PathBuf, dest_dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
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

/// Resolve a Windows .lnk shortcut file to its target path.
#[cfg(windows)]
pub fn resolve_lnk(path: &Path) -> Option<PathBuf> {
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

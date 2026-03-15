//! Drive enumeration, classification (SSD / HDD / USB / …), and space queries.
//! All functions are static (no `&self`).

use std::path::PathBuf;

use crate::types::DriveKind;
use super::RusplorerApp;

impl RusplorerApp {
    pub(crate) fn list_drives() -> Vec<String> {
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

    /// Returns `Some(true)` = SSD, `Some(false)` = HDD, `None` = unknown.
    /// Uses IOCTL_STORAGE_QUERY_PROPERTY / StorageDeviceSeekPenaltyProperty.
    #[cfg(windows)]
    pub(crate) fn query_is_ssd(drive_letter: char) -> Option<bool> {
        use winapi::um::fileapi::CreateFileW;
        use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
        use winapi::um::ioapiset::DeviceIoControl;
        use winapi::shared::minwindef::{DWORD, LPVOID};

        const OPEN_EXISTING: DWORD = 3;
        const FILE_SHARE_READ: DWORD = 0x0000_0001;
        const FILE_SHARE_WRITE: DWORD = 0x0000_0002;
        const IOCTL_STORAGE_QUERY_PROPERTY: DWORD = 0x002D_1400;

        #[repr(C)]
        struct StoragePropertyQuery {
            property_id: u32,        // 7 = StorageDeviceSeekPenaltyProperty
            query_type: u32,         // 0 = PropertyStandardQuery
            additional_parameters: [u8; 1],
        }

        #[repr(C)]
        struct DeviceSeekPenaltyDescriptor {
            version: u32,
            size: u32,
            incurs_seek_penalty: u8, // 0 = SSD (no seek penalty)
        }

        let path: Vec<u16> = format!("\\\\.\\{}:", drive_letter)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return None;
        }

        let query = StoragePropertyQuery {
            property_id: 7,
            query_type: 0,
            additional_parameters: [0],
        };
        let mut desc: DeviceSeekPenaltyDescriptor = unsafe { std::mem::zeroed() };
        let mut bytes_returned: DWORD = 0;

        let result = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                &query as *const _ as LPVOID,
                std::mem::size_of::<StoragePropertyQuery>() as DWORD,
                &mut desc as *mut _ as LPVOID,
                std::mem::size_of::<DeviceSeekPenaltyDescriptor>() as DWORD,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };

        unsafe { CloseHandle(handle); }

        if result != 0 { Some(desc.incurs_seek_penalty == 0) } else { None }
    }

    #[cfg(not(windows))]
    pub(crate) fn query_is_ssd(_drive_letter: char) -> Option<bool> { None }

    #[cfg(windows)]
    pub(crate) fn classify_drive(drive_letter: char) -> DriveKind {
        use winapi::um::fileapi::GetDriveTypeW;
        const DRIVE_REMOVABLE: u32 = 2;
        const DRIVE_FIXED:     u32 = 3;
        const DRIVE_REMOTE:    u32 = 4;
        const DRIVE_CDROM:     u32 = 5;
        const BUS_TYPE_USB:    u32 = 7;
        let path: Vec<u16> = format!("{}:\\", drive_letter)
            .encode_utf16().chain(std::iter::once(0)).collect();
        let kind = unsafe { GetDriveTypeW(path.as_ptr()) };
        match kind {
            DRIVE_REMOVABLE => DriveKind::Removable,
            DRIVE_REMOTE    => DriveKind::Network,
            DRIVE_CDROM     => DriveKind::CdRom,
            DRIVE_FIXED     => {
                // Some USB drives (especially high-capacity ones) self-report as
                // DRIVE_FIXED.  Check the actual bus type to catch them.
                if Self::query_bus_type(drive_letter) == Some(BUS_TYPE_USB) {
                    return DriveKind::Removable;
                }
                match Self::query_is_ssd(drive_letter) {
                    Some(true)  => DriveKind::Ssd,
                    Some(false) => DriveKind::Hdd,
                    // Drive asleep / IOCTL failed → assume HDD so a border is shown
                    None        => DriveKind::Hdd,
                }
            },
            _               => DriveKind::Unknown,
        }
    }

    /// Returns the bus type constant from `STORAGE_DEVICE_DESCRIPTOR.BusType`.
    /// 7 = BusTypeUsb.
    #[cfg(windows)]
    pub(crate) fn query_bus_type(drive_letter: char) -> Option<u32> {
        use winapi::um::fileapi::CreateFileW;
        use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
        use winapi::um::ioapiset::DeviceIoControl;
        use winapi::shared::minwindef::{DWORD, LPVOID};
        const OPEN_EXISTING: DWORD = 3;
        const FILE_SHARE_READ:  DWORD = 0x0000_0001;
        const FILE_SHARE_WRITE: DWORD = 0x0000_0002;
        const IOCTL_STORAGE_QUERY_PROPERTY: DWORD = 0x002D_1400;

        #[repr(C)]
        struct StoragePropertyQuery {
            property_id: u32,        // 0 = StorageDeviceProperty
            query_type:  u32,        // 0 = PropertyStandardQuery
            additional_parameters: [u8; 1],
        }
        // Only the fields we care about; layout matches Windows SDK struct.
        #[repr(C)]
        struct StorageDeviceDescriptor {
            version:                  u32,
            size:                     u32,
            device_type:              u8,
            device_type_modifier:     u8,
            removable_media:          u8,
            command_queueing:         u8,
            vendor_id_offset:         u32,
            product_id_offset:        u32,
            product_revision_offset:  u32,
            serial_number_offset:     u32,
            bus_type:                 u32,
        }

        let path: Vec<u16> = format!("\\\\.\\{}:", drive_letter)
            .encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(path.as_ptr(), 0,
                FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null_mut(),
                OPEN_EXISTING, 0, std::ptr::null_mut())
        };
        if handle == INVALID_HANDLE_VALUE { return None; }

        let query = StoragePropertyQuery { property_id: 0, query_type: 0, additional_parameters: [0] };
        let mut desc: StorageDeviceDescriptor = unsafe { std::mem::zeroed() };
        let mut bytes: DWORD = 0;
        let ok = unsafe {
            DeviceIoControl(handle, IOCTL_STORAGE_QUERY_PROPERTY,
                &query as *const _ as LPVOID,
                std::mem::size_of::<StoragePropertyQuery>() as DWORD,
                &mut desc as *mut _ as LPVOID,
                std::mem::size_of::<StorageDeviceDescriptor>() as DWORD,
                &mut bytes, std::ptr::null_mut())
        };
        unsafe { CloseHandle(handle); }
        if ok != 0 { Some(desc.bus_type) } else { None }
    }

    #[cfg(not(windows))]
    pub(crate) fn classify_drive(_drive_letter: char) -> DriveKind { DriveKind::Unknown }
    #[cfg(not(windows))]
    pub(crate) fn query_bus_type(_: char) -> Option<u32> { None }

    /// Returns `(free_bytes, total_bytes)` for the given drive root (e.g. `"C:\\"`).
    #[cfg(windows)]
    pub(crate) fn get_drive_space(drive: &str) -> (u64, u64) {
        use winapi::um::fileapi::GetDiskFreeSpaceExW;
        let path: Vec<u16> = drive.encode_utf16().chain(std::iter::once(0)).collect();
        let mut free_caller: u64 = 0;
        let mut total: u64       = 0;
        let mut free_total: u64  = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                path.as_ptr(),
                &mut free_caller as *mut u64 as *mut _,
                &mut total       as *mut u64 as *mut _,
                &mut free_total  as *mut u64 as *mut _,
            )
        };
        if ok != 0 { (free_total, total) } else { (0, 0) }
    }
    #[cfg(not(windows))]
    pub(crate) fn get_drive_space(_drive: &str) -> (u64, u64) { (0, 0) }

    /// Human-readable byte count (TB / GB / MB / KB).
    pub(crate) fn format_bytes(bytes: u64) -> String {
        const TB: u64 = 1 << 40;
        const GB: u64 = 1 << 30;
        const MB: u64 = 1 << 20;
        if bytes >= TB {
            format!("{:.2} TB", bytes as f64 / TB as f64)
        } else if bytes >= GB {
            format!("{:.1} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.0} MB", bytes as f64 / MB as f64)
        } else {
            format!("{} KB", bytes / 1024)
        }
    }
}

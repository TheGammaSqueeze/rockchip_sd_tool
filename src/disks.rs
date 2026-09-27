//! Enumeration of the disks a card can be written to, per operating system.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskInfo {
    /// The path to open for raw access (`/dev/sdb`, `/dev/rdisk4`, `\\.\PhysicalDrive2`).
    pub path: String,
    /// Short name shown in lists (`sdb`, `disk4`, `PhysicalDrive2`).
    pub name: String,
    /// Vendor and model, when known.
    pub model: String,
    pub size: u64,
    pub removable: bool,
    /// USB, SD, MMC, SATA, NVMe and so on.
    pub bus: String,
    /// True when the disk holds the running operating system or is otherwise not a card.
    pub system: bool,
    /// Mount points or drive letters currently in use on this disk.
    pub mounts: Vec<String>,
}

impl DiskInfo {
    pub fn label(&self) -> String {
        let model = if self.model.is_empty() { "Unknown device".to_string() } else { self.model.clone() };
        format!("{} ({}) {}", model, crate::util::vendor_gb(self.size), self.name)
    }
    /// Disks that are shown by default: removable or on a USB/SD bus, and not the system disk.
    pub fn is_candidate(&self) -> bool {
        !self.system && (self.removable || matches!(self.bus.as_str(), "USB" | "SD" | "MMC" | "Loop"))
    }
}

/// True when `spec` names a raw disk rather than a file.
pub fn is_block_device_path(spec: &str) -> bool {
    #[cfg(windows)]
    {
        let l = spec.to_ascii_lowercase();
        return l.starts_with("\\\\.\\physicaldrive") || l.starts_with("\\\\?\\physicaldrive");
    }
    #[cfg(unix)]
    {
        if !spec.starts_with("/dev/") {
            return false;
        }
        match std::fs::metadata(spec) {
            Ok(m) => {
                use std::os::unix::fs::FileTypeExt;
                m.file_type().is_block_device() || m.file_type().is_char_device()
            }
            Err(_) => false,
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::path::Path;

    fn read_trim(p: &Path) -> String {
        std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
    }

    /// Mount table entries: (device, mount point).
    fn mounts() -> Vec<(String, String)> {
        std::fs::read_to_string("/proc/self/mounts")
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let dev = it.next()?.to_string();
                let mp = it.next()?.replace("\\040", " ");
                Some((dev, mp))
            })
            .collect()
    }

    fn root_disk() -> Option<String> {
        // The disk holding "/" (or /usr for split setups): resolve the mount source to a device
        // and walk up to its parent block device in sysfs.
        for (dev, mp) in mounts() {
            if mp == "/" || mp == "/usr" || mp == "/boot" {
                if let Ok(real) = std::fs::canonicalize(&dev) {
                    if let Some(name) = real.file_name().and_then(|n| n.to_str()) {
                        let sys = Path::new("/sys/class/block").join(name);
                        if let Ok(target) = std::fs::canonicalize(&sys) {
                            // /sys/devices/.../block/sda/sda1 -> parent dir name is the disk.
                            if let Some(parent) = target.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
                                if Path::new("/sys/block").join(parent).exists() {
                                    return Some(parent.to_string());
                                }
                            }
                            return Some(name.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    pub fn list() -> Vec<DiskInfo> {
        let mut out = Vec::new();
        let root = root_disk();
        let mtab = mounts();
        let Ok(rd) = std::fs::read_dir("/sys/block") else { return out };
        // ROCKCHIP_SD_TOOL_SHOW_LOOP=1 lists loop devices too (for testing with losetup).
        let show_loop = std::env::var_os("ROCKCHIP_SD_TOOL_SHOW_LOOP").is_some();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if (name.starts_with("loop") && !show_loop)
                || name.starts_with("ram")
                || name.starts_with("dm-")
                || name.starts_with("zram")
                || name.starts_with("md")
                || name.starts_with("nbd")
                || name.starts_with("sr")
                || name.starts_with("fd")
            {
                continue;
            }
            let sys = e.path();
            let sectors: u64 = read_trim(&sys.join("size")).parse().unwrap_or(0);
            if sectors == 0 {
                continue;
            }
            let removable = read_trim(&sys.join("removable")) == "1";
            let vendor = read_trim(&sys.join("device/vendor"));
            let model = read_trim(&sys.join("device/model"));
            let mmc_name = read_trim(&sys.join("device/name"));
            let model = if !model.is_empty() {
                format!("{vendor} {model}").trim().to_string()
            } else if !mmc_name.is_empty() {
                mmc_name.clone()
            } else {
                String::new()
            };
            let real = std::fs::canonicalize(&sys).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            let bus = if name.starts_with("loop") {
                "Loop"
            } else if real.contains("/usb") {
                "USB"
            } else if name.starts_with("mmcblk") {
                "SD"
            } else if real.contains("/nvme") {
                "NVMe"
            } else if real.contains("/ata") {
                "SATA"
            } else if real.contains("/virtio") {
                "VirtIO"
            } else {
                "Disk"
            };
            let path = format!("/dev/{name}");
            let mut mounts_here = Vec::new();
            for (dev, mp) in &mtab {
                if dev == &path || (dev.starts_with(&path) && dev[path.len()..].chars().all(|c| c.is_ascii_digit() || c == 'p')) {
                    mounts_here.push(mp.clone());
                }
            }
            let system = root.as_deref() == Some(name.as_str());
            out.push(DiskInfo { path, name, model, size: sectors * 512, removable, bus: bus.into(), system, mounts: mounts_here });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Unmounts every mounted partition of the disk.
    pub fn prepare_for_write(path: &str) -> anyhow::Result<()> {
        for (dev, mp) in mounts() {
            let on_disk = dev == path
                || (dev.starts_with(path) && dev[path.len()..].chars().all(|c| c.is_ascii_digit() || c == 'p'));
            if on_disk {
                let st = std::process::Command::new("umount").arg(&mp).status();
                match st {
                    Ok(s) if s.success() => {}
                    _ => anyhow::bail!("cannot unmount {mp} (on {dev}); unmount it and try again"),
                }
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::process::Command;

    fn plist_cmd(args: &[&str]) -> Option<plist::Value> {
        let out = Command::new("diskutil").args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        plist::from_bytes(&out.stdout).ok()
    }

    fn str_of(d: &plist::Dictionary, key: &str) -> String {
        d.get(key).and_then(|v| v.as_string()).unwrap_or("").to_string()
    }
    fn bool_of(d: &plist::Dictionary, key: &str) -> bool {
        d.get(key).and_then(|v| v.as_boolean()).unwrap_or(false)
    }
    fn u64_of(d: &plist::Dictionary, key: &str) -> u64 {
        d.get(key).and_then(|v| v.as_unsigned_integer().or_else(|| v.as_signed_integer().map(|i| i as u64))).unwrap_or(0)
    }

    pub fn list() -> Vec<DiskInfo> {
        let mut out = Vec::new();
        let Some(root) = plist_cmd(&["list", "-plist", "physical"]) else { return out };
        let Some(dict) = root.as_dictionary() else { return out };
        let names: Vec<String> = dict
            .get("WholeDisks")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_string().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        for name in names {
            let Some(info) = plist_cmd(&["info", "-plist", &name]) else { continue };
            let Some(d) = info.as_dictionary() else { continue };
            let size = u64_of(d, "TotalSize").max(u64_of(d, "Size"));
            if size == 0 {
                continue;
            }
            let proto = str_of(d, "BusProtocol");
            let bus = match proto.as_str() {
                "USB" => "USB",
                "Secure Digital" => "SD",
                "SATA" => "SATA",
                "PCI-Express" | "PCI" => "NVMe",
                "Apple Fabric" => "Internal",
                _ => "Disk",
            };
            let internal = bool_of(d, "Internal");
            let removable = bool_of(d, "RemovableMedia") || bool_of(d, "Removable") || bool_of(d, "Ejectable");
            let model = str_of(d, "MediaName");
            let system = internal || bool_of(d, "SystemImage");
            // Mount points of the volumes on this disk.
            let mut mounts = Vec::new();
            if let Some(v) = plist_cmd(&["list", "-plist", &name]) {
                if let Some(vd) = v.as_dictionary() {
                    if let Some(all) = vd.get("AllDisksAndPartitions").and_then(|v| v.as_array()) {
                        for disk in all {
                            let Some(dd) = disk.as_dictionary() else { continue };
                            for key in ["Partitions", "APFSVolumes"] {
                                if let Some(parts) = dd.get(key).and_then(|v| v.as_array()) {
                                    for p in parts {
                                        if let Some(pd) = p.as_dictionary() {
                                            let mp = str_of(pd, "MountPoint");
                                            if !mp.is_empty() {
                                                mounts.push(mp);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            out.push(DiskInfo {
                path: format!("/dev/r{name}"),
                name: name.clone(),
                model,
                size,
                removable,
                bus: bus.into(),
                system,
                mounts,
            });
        }
        out
    }

    pub fn prepare_for_write(path: &str) -> anyhow::Result<()> {
        let name = path.trim_start_matches("/dev/r").trim_start_matches("/dev/");
        let st = Command::new("diskutil").args(["unmountDisk", "force", name]).status();
        match st {
            Ok(s) if s.success() => Ok(()),
            _ => anyhow::bail!("diskutil could not unmount {name}"),
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetLogicalDrives, FILE_SHARE_READ,
        FILE_SHARE_WRITE, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceProperty, IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
        STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY, VOLUME_DISK_EXTENTS,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

    fn wide(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    fn open_query(path: &str) -> Option<windows_sys::Win32::Foundation::HANDLE> {
        let h = unsafe {
            CreateFileW(
                wide(path).as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            // Some devices refuse GENERIC_READ without admin rights; a zero-access handle still
            // answers property queries.
            let h2 = unsafe {
                CreateFileW(wide(path).as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
            };
            if h2 == INVALID_HANDLE_VALUE {
                return None;
            }
            return Some(h2);
        }
        Some(h)
    }

    /// Disk numbers used by the volume at `vol` (`\\.\C:` or `\\?\Volume{..}` without the trailing
    /// backslash).
    fn volume_disks(vol: &str) -> Vec<u32> {
        let Some(h) = open_query(vol) else { return Vec::new() };
        let mut buf = vec![0u8; std::mem::size_of::<VOLUME_DISK_EXTENTS>() + 64 * 24];
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                h,
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                std::ptr::null(),
                0,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return Vec::new();
        }
        let ext = unsafe { &*(buf.as_ptr() as *const VOLUME_DISK_EXTENTS) };
        let n = ext.NumberOfDiskExtents as usize;
        let base = unsafe { buf.as_ptr().add(8) as *const windows_sys::Win32::System::Ioctl::DISK_EXTENT };
        (0..n).map(|i| unsafe { (*base.add(i)).DiskNumber }).collect()
    }

    /// Every volume (with or without a drive letter) that lives on the given disk, as openable
    /// paths (`\\.\E:` or `\\.\Volume{guid}`).
    pub fn windows_volumes_on_disk(disk: u32) -> Vec<String> {
        let mut out = Vec::new();
        let mut name = [0u16; 512];
        let h = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
        if h == INVALID_HANDLE_VALUE {
            return out;
        }
        loop {
            let s = String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(0)]);
            // "\\?\Volume{guid}\" -> "\\.\Volume{guid}"
            let openable = format!("\\\\.\\{}", s.trim_start_matches("\\\\?\\").trim_end_matches('\\'));
            if volume_disks(&openable).contains(&disk) {
                out.push(openable);
            }
            if unsafe { FindNextVolumeW(h, name.as_mut_ptr(), name.len() as u32) } == 0 {
                break;
            }
        }
        unsafe { FindVolumeClose(h) };
        out
    }

    /// Drive letters (as `E:`) whose volume lives on the given disk.
    fn letters_on_disk(disk: u32) -> Vec<String> {
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            if volume_disks(&format!("\\\\.\\{letter}:")).contains(&disk) {
                out.push(format!("{letter}:"));
            }
        }
        out
    }

    pub fn windows_disk_number(path: &str) -> Option<u32> {
        let l = path.to_ascii_lowercase();
        let rest = l.strip_prefix("\\\\.\\physicaldrive").or_else(|| l.strip_prefix("\\\\?\\physicaldrive"))?;
        rest.parse().ok()
    }

    fn system_disk() -> Option<u32> {
        let mut buf = [0u16; 260];
        let n = unsafe { GetWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
        if n == 0 {
            return None;
        }
        let s = String::from_utf16_lossy(&buf[..n as usize]);
        let letter = s.chars().next()?;
        let disks = volume_disks(&format!("\\\\.\\{letter}:"));
        disks.first().copied()
    }

    pub fn list() -> Vec<DiskInfo> {
        let mut out = Vec::new();
        let sys = system_disk();
        for n in 0..64u32 {
            let path = format!("\\\\.\\PhysicalDrive{n}");
            let Some(h) = open_query(&path) else { continue };
            let size = crate::blockdev::imp_disk_length(h).unwrap_or(0);
            // Device descriptor: bus type, vendor and product strings, removable flag.
            let mut q: STORAGE_PROPERTY_QUERY = unsafe { std::mem::zeroed() };
            q.PropertyId = StorageDeviceProperty;
            q.QueryType = PropertyStandardQuery;
            let mut buf = vec![0u8; 4096];
            let mut ret: u32 = 0;
            let ok = unsafe {
                DeviceIoControl(
                    h,
                    IOCTL_STORAGE_QUERY_PROPERTY,
                    &q as *const _ as *const _,
                    std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    &mut ret,
                    std::ptr::null_mut(),
                )
            };
            let mut dn: STORAGE_DEVICE_NUMBER = unsafe { std::mem::zeroed() };
            let mut ret2: u32 = 0;
            unsafe {
                DeviceIoControl(
                    h,
                    IOCTL_STORAGE_GET_DEVICE_NUMBER,
                    std::ptr::null(),
                    0,
                    &mut dn as *mut _ as *mut _,
                    std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                    &mut ret2,
                    std::ptr::null_mut(),
                )
            };
            unsafe { CloseHandle(h) };
            if size == 0 {
                continue;
            }
            let (mut model, mut bus, mut removable) = (String::new(), "Disk".to_string(), false);
            if ok != 0 {
                let d = unsafe { &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
                removable = d.RemovableMedia;
                bus = match d.BusType {
                    7 => "USB",
                    0xc => "SD",
                    0xd => "MMC",
                    0xb => "SATA",
                    0x11 => "NVMe",
                    0x3 => "ATA",
                    0x1 => "SCSI",
                    _ => "Disk",
                }
                .to_string();
                let cstr_at = |o: u32| -> String {
                    if o == 0 || o as usize >= buf.len() {
                        return String::new();
                    }
                    let s = &buf[o as usize..];
                    let end = s.iter().position(|&c| c == 0).unwrap_or(0);
                    String::from_utf8_lossy(&s[..end]).trim().to_string()
                };
                let vendor = cstr_at(d.VendorIdOffset);
                let product = cstr_at(d.ProductIdOffset);
                model = format!("{vendor} {product}").trim().to_string();
            }
            let mounts = letters_on_disk(n);
            out.push(DiskInfo {
                path,
                name: format!("PhysicalDrive{n}"),
                model,
                size,
                removable,
                bus,
                system: sys == Some(n),
                mounts,
            });
        }
        out
    }

    pub fn prepare_for_write(_path: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

pub use imp::{list, prepare_for_write};
#[cfg(windows)]
pub use imp::{windows_disk_number, windows_volumes_on_disk};

/// Finds a listed disk by its path.
pub fn find(path: &str) -> Option<DiskInfo> {
    list().into_iter().find(|d| d.path.eq_ignore_ascii_case(path))
}

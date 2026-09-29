//! Raw block device access for Linux, macOS and Windows.
//!
//! All I/O is in whole 512-byte sectors. Reads used for verification bypass the operating
//! system cache so that the card itself is checked, not a cached copy of what was just written.

#[allow(unused_imports)]
use anyhow::{bail, Context, Result};

use crate::target::Target;

pub const SECTOR: u64 = 512;

/// Rounds a buffer up to the alignment that unbuffered I/O needs on every platform.
pub const IO_ALIGN: usize = 4096;

/// A heap buffer aligned to [`IO_ALIGN`] bytes.
pub struct AlignedBuf {
    raw: Vec<u8>,
    off: usize,
    len: usize,
}

impl AlignedBuf {
    pub fn new(len: usize) -> AlignedBuf {
        let raw = vec![0u8; len + IO_ALIGN];
        let off = (IO_ALIGN - (raw.as_ptr() as usize % IO_ALIGN)) % IO_ALIGN;
        AlignedBuf { raw, off, len }
    }
    pub fn as_slice(&self) -> &[u8] {
        &self.raw[self.off..self.off + self.len]
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.raw[self.off..self.off + self.len]
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{FileExt, OpenOptionsExt};

    pub struct BlockDevice {
        path: String,
        file: File,
        /// Unbuffered handle for verification reads.
        direct: Option<File>,
        size: u64,
        writable: bool,
    }

    #[cfg(target_os = "linux")]
    fn device_size(f: &File) -> Result<u64> {
        use std::os::unix::io::AsRawFd;
        let mut size: u64 = 0;
        // BLKGETSIZE64 = _IOR(0x12, 114, size_t)
        const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
        let r = unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64 as _, &mut size as *mut u64) };
        if r != 0 {
            bail!("BLKGETSIZE64 failed: {}", std::io::Error::last_os_error());
        }
        Ok(size)
    }

    #[cfg(target_os = "macos")]
    fn device_size(f: &File) -> Result<u64> {
        use std::os::unix::io::AsRawFd;
        const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
        const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;
        let mut bs: u32 = 0;
        let mut count: u64 = 0;
        if unsafe { libc::ioctl(f.as_raw_fd(), DKIOCGETBLOCKSIZE, &mut bs as *mut u32) } != 0 {
            bail!("DKIOCGETBLOCKSIZE failed: {}", std::io::Error::last_os_error());
        }
        if unsafe { libc::ioctl(f.as_raw_fd(), DKIOCGETBLOCKCOUNT, &mut count as *mut u64) } != 0 {
            bail!("DKIOCGETBLOCKCOUNT failed: {}", std::io::Error::last_os_error());
        }
        Ok(bs as u64 * count)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn device_size(f: &File) -> Result<u64> {
        Ok(f.metadata()?.len())
    }

    impl BlockDevice {
        pub fn open_for_write(path: &str) -> Result<BlockDevice> {
            Self::open_for_write_ex(path, false)
        }

        /// `keep_layout` leaves the existing partition table alone (upgrade mode).
        pub fn open_for_write_ex(path: &str, keep_layout: bool) -> Result<BlockDevice> {
            let _ = keep_layout;
            crate::disks::prepare_for_write(path)?;
            let file = open_rw(path)?;
            set_nocache(&file);
            let size = device_size(&file)?;
            let direct = open_direct(path).ok();
            Ok(BlockDevice { path: path.to_string(), file, direct, size, writable: true })
        }

        pub fn open_for_read(path: &str) -> Result<BlockDevice> {
            let file = File::open(path).with_context(|| format!("cannot open {path} for reading"))?;
            set_nocache(&file);
            let size = device_size(&file)?;
            let direct = open_direct(path).ok();
            Ok(BlockDevice { path: path.to_string(), file, direct, size, writable: false })
        }

        /// Asks the kernel to re-read the partition table.
        pub fn reread_partitions(&self) {
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::io::AsRawFd;
                const BLKRRPART: libc::c_ulong = 0x125f;
                unsafe {
                    libc::ioctl(self.file.as_raw_fd(), BLKRRPART as _);
                }
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn open_rw(path: &str) -> Result<File> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        #[cfg(target_os = "linux")]
        opts.custom_flags(libc::O_EXCL);
        opts.open(path).with_context(|| format!("cannot open {path} for writing (is it mounted or in use?)"))
    }

    /// macOS: the program never runs as root. Like Raspberry Pi Imager, it asks the system's
    /// `authopen` helper (which shows the administrator password dialog) to open the raw disk
    /// and hand the descriptor back over a socket, so the image file is still read with the
    /// user's own permissions (macOS privacy rules keep even root out of Downloads).
    #[cfg(target_os = "macos")]
    fn open_rw(path: &str) -> Result<File> {
        use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
        use std::os::unix::net::UnixStream;
        use std::process::{Command, Stdio};
        if unsafe { libc::geteuid() } == 0 {
            return OpenOptions::new().read(true).write(true).open(path).with_context(|| format!("cannot open {path} for writing"));
        }
        let (parent, child) = UnixStream::pair()?;
        let child_fd = child.into_raw_fd();
        let mut cmd = Command::new("/usr/libexec/authopen");
        cmd.arg("-stdoutpipe").arg("-o").arg(format!("{}", libc::O_RDWR)).arg(path);
        cmd.stdin(Stdio::null()).stderr(Stdio::piped());
        cmd.stdout(unsafe { Stdio::from_raw_fd(child_fd) });
        let mut proc_ = cmd.spawn().context("cannot start /usr/libexec/authopen")?;
        // Receive the descriptor (SCM_RIGHTS) from authopen.
        let mut data = [0u8; 16];
        let mut iov = libc::iovec { iov_base: data.as_mut_ptr() as *mut libc::c_void, iov_len: data.len() };
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) } as usize;
        let mut cbuf = vec![0u8; space];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        let mut fd: libc::c_int = -1;
        loop {
            let n = unsafe { libc::recvmsg(parent.as_raw_fd(), &mut msg, 0) };
            if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if n > 0 {
                let c = unsafe { libc::CMSG_FIRSTHDR(&msg) };
                if !c.is_null() && unsafe { (*c).cmsg_type } == libc::SCM_RIGHTS {
                    fd = unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const libc::c_int) };
                }
            }
            break;
        }
        drop(parent);
        let status = proc_.wait()?;
        let mut err = String::new();
        if let Some(mut e) = proc_.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut err);
        }
        if !status.success() || fd < 0 {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
            let err = err.trim();
            if err.is_empty() {
                bail!("access to {path} was not granted (the administrator password dialog was cancelled or failed)");
            }
            bail!("cannot open {path}: {err}");
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    #[cfg(target_os = "linux")]
    fn open_direct(path: &str) -> Result<File> {
        Ok(OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(path)?)
    }

    #[cfg(target_os = "macos")]
    fn open_direct(path: &str) -> Result<File> {
        // The raw device (/dev/rdiskN) is unbuffered by nature; a second plain handle would need
        // its own authorization, so verification reads reuse the main handle with F_NOCACHE.
        let _ = path;
        bail!("not used on macOS")
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn open_direct(path: &str) -> Result<File> {
        Ok(File::open(path)?)
    }

    impl Target for BlockDevice {
        fn size(&self) -> u64 {
            self.size
        }
        fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
            if !self.writable {
                bail!("device opened read-only");
            }
            if off % SECTOR != 0 || buf.len() as u64 % SECTOR != 0 {
                bail!("unaligned write ({} bytes at {})", buf.len(), off);
            }
            if off + buf.len() as u64 > self.size {
                bail!("write of {} bytes at {} exceeds the device size {}", buf.len(), off, self.size);
            }
            self.file.write_all_at(buf, off).with_context(|| format!("write failed at offset {off}"))?;
            Ok(())
        }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
            if off + buf.len() as u64 > self.size {
                bail!("read of {} bytes at {} exceeds the device size {}", buf.len(), off, self.size);
            }
            // Unbuffered reads need aligned memory and lengths; use a bounce buffer.
            if let Some(d) = &self.direct {
                let len = buf.len();
                let alen = (len + IO_ALIGN - 1) / IO_ALIGN * IO_ALIGN;
                let aoff = off / IO_ALIGN as u64 * IO_ALIGN as u64;
                let head = (off - aoff) as usize;
                let total = (head + alen + IO_ALIGN - 1) / IO_ALIGN * IO_ALIGN;
                let total = std::cmp::min(total as u64, self.size - aoff) as usize;
                let mut bb = AlignedBuf::new(total);
                match d.read_exact_at(bb.as_mut_slice(), aoff) {
                    Ok(()) => {
                        buf.copy_from_slice(&bb.as_slice()[head..head + len]);
                        return Ok(());
                    }
                    Err(_) => {
                        // Fall back to the buffered handle after dropping the cache.
                        drop_cache(&self.file);
                    }
                }
            }
            self.file.read_exact_at(buf, off).with_context(|| format!("read failed at offset {off}"))?;
            Ok(())
        }
        fn flush(&mut self) -> Result<()> {
            if self.writable {
                if let Err(e) = self.file.sync_all() {
                    // Raw devices on macOS (/dev/rdiskN) answer fsync with ENOTTY: writes to them
                    // are unbuffered, so there is nothing to flush.
                    let benign = matches!(e.raw_os_error(), Some(libc::ENOTTY) | Some(libc::EINVAL)) && cfg!(target_os = "macos");
                    if !benign {
                        return Err(e).context("sync failed");
                    }
                }
                drop_cache(&self.file);
            }
            Ok(())
        }
        fn finish(&mut self) -> Result<()> {
            self.flush()?;
            self.reread_partitions();
            Ok(())
        }
        fn description(&self) -> String {
            format!("device {}", self.path)
        }
    }

    fn set_nocache(f: &File) {
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1);
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = f;
        }
    }

    fn drop_cache(f: &File) {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = f;
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, SetFilePointerEx, WriteFile, FILE_BEGIN, FILE_FLAG_NO_BUFFERING,
        FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME, IOCTL_DISK_DELETE_DRIVE_LAYOUT,
        IOCTL_DISK_GET_LENGTH_INFO, IOCTL_DISK_UPDATE_PROPERTIES,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    pub fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn last_error() -> std::io::Error {
        std::io::Error::last_os_error()
    }

    pub struct BlockDevice {
        path: String,
        handle: HANDLE,
        volumes: Vec<HANDLE>,
        size: u64,
        writable: bool,
    }

    unsafe impl Send for BlockDevice {}

    pub fn open_raw(path: &str, write: bool) -> Result<HANDLE> {
        let access = if write { GENERIC_READ | GENERIC_WRITE } else { GENERIC_READ };
        let h = unsafe {
            CreateFileW(
                wide(path).as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            bail!("cannot open {path}: {}", last_error());
        }
        Ok(h)
    }

    pub fn disk_length(h: HANDLE) -> Result<u64> {
        let mut len: i64 = 0;
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                h,
                IOCTL_DISK_GET_LENGTH_INFO,
                std::ptr::null(),
                0,
                &mut len as *mut i64 as *mut _,
                8,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            bail!("IOCTL_DISK_GET_LENGTH_INFO failed: {}", last_error());
        }
        Ok(len as u64)
    }

    fn ioctl_simple(h: HANDLE, code: u32) -> bool {
        let mut ret: u32 = 0;
        unsafe { DeviceIoControl(h, code, std::ptr::null(), 0, std::ptr::null_mut(), 0, &mut ret, std::ptr::null_mut()) != 0 }
    }

    impl BlockDevice {
        pub fn open_for_write(path: &str) -> Result<BlockDevice> {
            Self::open_for_write_ex(path, false)
        }

        /// `keep_layout` leaves the existing partition table alone (upgrade mode).
        pub fn open_for_write_ex(path: &str, keep_layout: bool) -> Result<BlockDevice> {
            let disk_number = crate::disks::windows_disk_number(path)
                .ok_or_else(|| anyhow::anyhow!("{path} is not a \\\\.\\PhysicalDriveN path"))?;
            // Lock and dismount every volume that lives on this disk, and keep the handles so the
            // lock holds for the whole write.
            let mut volumes = Vec::new();
            for vol in crate::disks::windows_volumes_on_disk(disk_number) {
                let h = match open_raw(&vol, true) {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                if !ioctl_simple(h, FSCTL_LOCK_VOLUME) {
                    unsafe { CloseHandle(h) };
                    bail!("cannot lock volume {vol}: it is in use (close Explorer windows and programs using the card)");
                }
                ioctl_simple(h, FSCTL_DISMOUNT_VOLUME);
                volumes.push(h);
            }
            let handle = open_raw(path, true)?;
            let size = disk_length(handle)?;
            // Drop the stale partition layout so Windows does not keep the old volumes around.
            // An upgrade keeps the table the card already has, so it must not be deleted.
            if !keep_layout {
                ioctl_simple(handle, IOCTL_DISK_DELETE_DRIVE_LAYOUT);
            }
            Ok(BlockDevice { path: path.to_string(), handle, volumes, size, writable: true })
        }

        pub fn open_for_read(path: &str) -> Result<BlockDevice> {
            let handle = open_raw(path, false)?;
            let size = disk_length(handle)?;
            Ok(BlockDevice { path: path.to_string(), handle, volumes: Vec::new(), size, writable: false })
        }

        fn seek(&self, off: u64) -> Result<()> {
            let ok = unsafe { SetFilePointerEx(self.handle, off as i64, std::ptr::null_mut(), FILE_BEGIN) };
            if ok == 0 {
                bail!("seek to {off} failed: {}", last_error());
            }
            Ok(())
        }
    }

    impl Drop for BlockDevice {
        fn drop(&mut self) {
            unsafe {
                if self.writable {
                    FlushFileBuffers(self.handle);
                    ioctl_simple(self.handle, IOCTL_DISK_UPDATE_PROPERTIES);
                }
                CloseHandle(self.handle);
                for v in self.volumes.drain(..) {
                    ioctl_simple(v, FSCTL_UNLOCK_VOLUME);
                    CloseHandle(v);
                }
            }
        }
    }

    impl Target for BlockDevice {
        fn size(&self) -> u64 {
            self.size
        }
        fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
            if !self.writable {
                bail!("device opened read-only");
            }
            if off % SECTOR != 0 || buf.len() as u64 % SECTOR != 0 {
                bail!("unaligned write ({} bytes at {})", buf.len(), off);
            }
            if off + buf.len() as u64 > self.size {
                bail!("write of {} bytes at {} exceeds the device size {}", buf.len(), off, self.size);
            }
            let mut ab = AlignedBuf::new(buf.len());
            ab.as_mut_slice().copy_from_slice(buf);
            self.seek(off)?;
            let mut done = 0usize;
            while done < buf.len() {
                let chunk = std::cmp::min(buf.len() - done, 32 << 20) as u32;
                let mut written: u32 = 0;
                let ok = unsafe {
                    WriteFile(self.handle, ab.as_slice()[done..].as_ptr(), chunk, &mut written, std::ptr::null_mut())
                };
                if ok == 0 || written == 0 {
                    bail!("write failed at offset {}: {}", off + done as u64, last_error());
                }
                done += written as usize;
            }
            Ok(())
        }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
            if off + buf.len() as u64 > self.size {
                bail!("read of {} bytes at {} exceeds the device size {}", buf.len(), off, self.size);
            }
            let len = buf.len();
            let aoff = off / SECTOR * SECTOR;
            let head = (off - aoff) as usize;
            let total = ((head + len) as u64 + SECTOR - 1) / SECTOR * SECTOR;
            let total = std::cmp::min(total, self.size - aoff) as usize;
            let mut ab = AlignedBuf::new(total);
            self.seek(aoff)?;
            let mut done = 0usize;
            while done < total {
                let chunk = std::cmp::min(total - done, 32 << 20) as u32;
                let mut got: u32 = 0;
                let ok = unsafe {
                    ReadFile(self.handle, ab.as_mut_slice()[done..].as_mut_ptr(), chunk, &mut got, std::ptr::null_mut())
                };
                if ok == 0 || got == 0 {
                    bail!("read failed at offset {}: {}", aoff + done as u64, last_error());
                }
                done += got as usize;
            }
            buf.copy_from_slice(&ab.as_slice()[head..head + len]);
            Ok(())
        }
        fn flush(&mut self) -> Result<()> {
            if self.writable && unsafe { FlushFileBuffers(self.handle) } == 0 {
                bail!("flush failed: {}", last_error());
            }
            Ok(())
        }
        fn description(&self) -> String {
            format!("device {}", self.path)
        }
    }
}

pub use imp::BlockDevice;
#[cfg(windows)]
pub fn imp_disk_length(h: windows_sys::Win32::Foundation::HANDLE) -> Result<u64> {
    imp::disk_length(h)
}

//! Write targets: a raw block device, a plain `.img` file, or an xz-compressed `.img.xz` stream.
//!
//! Every target exposes random-access `write_at`/`read_at` in whole sectors; the xz target only
//! accepts writes in ascending order (the writer feeds it a flattened, sorted plan) and fills the
//! gaps with zeros itself.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

pub const SECTOR: u64 = 512;

pub trait Target {
    /// Total size in bytes.
    fn size(&self) -> u64;
    /// Writes `buf` (a whole number of sectors) at byte offset `off`.
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()>;
    /// Reads `buf.len()` bytes at `off`. Not supported by streaming targets.
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()>;
    /// True when the target can only be written front to back.
    fn sequential_only(&self) -> bool {
        false
    }
    /// True when untouched regions read back as zero (fresh files), so zero ranges may be skipped.
    fn zero_by_default(&self) -> bool {
        false
    }
    /// True when reading a block back right after writing it says something about the medium.
    /// A plain file is served from the page cache, so per-block read-back is skipped for it.
    fn supports_block_verify(&self) -> bool {
        true
    }
    /// Flushes everything to stable storage.
    fn flush(&mut self) -> Result<()>;
    /// Finishes the target (pads a stream to its full size).
    fn finish(&mut self) -> Result<()> {
        self.flush()
    }
    fn description(&self) -> String;
}

/// A plain image file of a fixed size. Created sparse where the file system allows it.
pub struct FileTarget {
    file: File,
    size: u64,
    path: PathBuf,
}

impl FileTarget {
    pub fn create(path: &Path, size: u64) -> Result<FileTarget> {
        if size % SECTOR != 0 {
            bail!("image size must be a multiple of 512 bytes");
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        file.set_len(size).with_context(|| format!("cannot size {} to {} bytes", path.display(), size))?;
        Ok(FileTarget { file, size, path: path.to_path_buf() })
    }

    pub fn open_existing(path: &Path, writable: bool) -> Result<FileTarget> {
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        let size = file.metadata()?.len();
        Ok(FileTarget { file, size, path: path.to_path_buf() })
    }
}

impl Target for FileTarget {
    fn size(&self) -> u64 {
        self.size
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        if off + buf.len() as u64 > self.size {
            bail!("write of {} bytes at {} exceeds the image size {}", buf.len(), off, self.size);
        }
        self.file.seek(SeekFrom::Start(off))?;
        self.file.write_all(buf)?;
        Ok(())
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(off))?;
        self.file.read_exact(buf)?;
        Ok(())
    }
    fn zero_by_default(&self) -> bool {
        true
    }
    fn supports_block_verify(&self) -> bool {
        false
    }
    fn flush(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
    fn description(&self) -> String {
        format!("image file {}", self.path.display())
    }
}

/// Opens the right target for a path or device name.
pub fn open_output(spec: &str, size: u64, xz_level: u32) -> Result<Box<dyn Target>> {
    open_output_ex(spec, size, xz_level, false)
}

/// As [`open_output`]; with `upgrade` the target must already exist and is opened in place, so
/// nothing outside the ranges the plan writes is disturbed.
pub fn open_output_ex(spec: &str, size: u64, xz_level: u32, upgrade: bool) -> Result<Box<dyn Target>> {
    if crate::disks::is_block_device_path(spec) {
        return Ok(Box::new(crate::blockdev::BlockDevice::open_for_write_ex(spec, upgrade)?));
    }
    let path = Path::new(spec);
    if upgrade {
        if is_xz_path(spec) {
            bail!("a compressed image cannot be upgraded in place; upgrade a card or a raw .img file");
        }
        if !path.exists() {
            bail!("{} does not exist; an upgrade needs a card or image that already has the layout", path.display());
        }
        return Ok(Box::new(FileTarget::open_existing(path, true)?));
    }
    if size == 0 {
        bail!("an image size (the SD card size) is required when writing to a file");
    }
    if is_xz_path(spec) {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        Ok(Box::new(crate::xzimg::XzImageWriter::create(path, size, xz_level, threads)?))
    } else {
        Ok(Box::new(FileTarget::create(path, size)?))
    }
}

pub fn is_xz_path(spec: &str) -> bool {
    spec.to_ascii_lowercase().ends_with(".xz")
}

/// Opens a block device, a raw image file or a compressed image read-only for verification.
pub fn open_readable(spec: &str) -> Result<Box<dyn Target>> {
    if crate::disks::is_block_device_path(spec) {
        Ok(Box::new(crate::blockdev::BlockDevice::open_for_read(spec)?))
    } else if is_xz_path(spec) {
        Ok(Box::new(crate::xzimg::XzImageReader::open(Path::new(spec))?))
    } else {
        Ok(Box::new(FileTarget::open_existing(Path::new(spec), false)?))
    }
}

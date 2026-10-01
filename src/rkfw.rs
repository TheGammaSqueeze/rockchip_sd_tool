//! Parsing of Rockchip RKFW firmware images.
//!
//! An RKFW file is a small header followed by two blobs:
//!
//! * the loader, an RKBOOT container (magic `BOOT` or `LDR `) holding the DDR init, USB plug and
//!   flash loader entries (`FlashHead`, `FlashData`, `FlashBoot`), each RC4 scrambled;
//! * the firmware, an RKAF container (`update.img`) holding the partition images plus the
//!   `parameter` file describing the partition layout.
//!
//! A 32-character hexadecimal MD5 of everything before it is appended at the end of the file.
//! Multi-byte fields are little endian.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

pub const RKFW_MAGIC: &[u8; 4] = b"RKFW";
pub const RKAF_MAGIC: &[u8; 4] = b"RKAF";
pub const BOOT_MAGIC: &[u8; 4] = b"BOOT";
pub const LDR_MAGIC: &[u8; 4] = b"LDR ";

/// Rockchip release time stamp.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RkTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl std::fmt::Display for RkTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

fn read_time(b: &[u8]) -> RkTime {
    RkTime {
        year: u16::from_le_bytes([b[0], b[1]]),
        month: b[2],
        day: b[3],
        hour: b[4],
        minute: b[5],
        second: b[6],
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Formats a Rockchip packed version (`major.minor.patch` in 8/8/16 bits).
pub fn format_version(v: u32) -> String {
    format!("{}.{}.{}", v >> 24, (v >> 16) & 0xff, v & 0xffff)
}

/// Turns the chip tag (an ASCII code stored reversed, e.g. `8653` for RK3568) into a name.
pub fn chip_name(tag: u32) -> String {
    let b = tag.to_le_bytes();
    let s: String = b.iter().rev().map(|&c| if c.is_ascii_graphic() { c as char } else { '?' }).collect();
    match tag {
        0x33353638 => "RK3568".into(),
        0x33353636 => "RK3566".into(),
        0x33353838 => "RK3588".into(),
        0x33333939 => "RK3399".into(),
        0x33323838 => "RK3288".into(),
        0x33333238 => "RK3328".into(),
        0x33333236 => "RK3326".into(),
        0x33353632 => "RK3562".into(),
        0x33353736 => "RK3576".into(),
        _ => {
            if s.chars().all(|c| c.is_ascii_alphanumeric()) {
                format!("RK{s}")
            } else {
                format!("0x{tag:08x}")
            }
        }
    }
}

/// The RKFW file header.
#[derive(Debug, Clone)]
pub struct RkfwHeader {
    pub header_size: u16,
    pub version: u32,
    pub code: u32,
    pub time: RkTime,
    pub chip: u32,
    pub loader_offset: u64,
    pub loader_size: u64,
    pub image_offset: u64,
    pub image_size: u64,
}

/// One entry of an RKBOOT container.
#[derive(Debug, Clone)]
pub struct BootEntry {
    /// 1 = 471 (DDR / usb head), 2 = 472 (usb plug), 4 = loader (flash head/data/boot).
    pub kind: u32,
    pub name: String,
    /// Absolute file offset of the entry data.
    pub offset: u64,
    pub size: u64,
    pub delay: u32,
}

/// The RKBOOT container found in an RKFW image.
#[derive(Debug, Clone)]
pub struct BootInfo {
    pub magic: [u8; 4],
    pub version: u32,
    pub merge_version: u32,
    pub time: RkTime,
    pub chip: u32,
    pub sign_flag: u8,
    /// 1 means the boot ROM wants plain (unscrambled) loader data.
    pub rc4_flag: u8,
    pub entries_471: Vec<BootEntry>,
    pub entries_472: Vec<BootEntry>,
    pub entries_loader: Vec<BootEntry>,
}

impl BootInfo {
    pub fn loader_entry(&self, name: &str) -> Option<&BootEntry> {
        self.entries_loader.iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }
}

/// One item of the RKAF (update.img) container.
#[derive(Debug, Clone)]
pub struct AfItem {
    pub name: String,
    pub file_name: String,
    /// Partition size in sectors as recorded by afptool (0 when unknown).
    pub nand_size: u32,
    /// Absolute file offset of the item data.
    pub offset: u64,
    /// Partition offset in sectors as recorded by afptool (0xffffffff when not a partition).
    pub nand_addr: u32,
    pub padded_size: u64,
    pub size: u64,
}

impl AfItem {
    /// `backup` is recorded as RESERVED (the update image itself); it carries no data.
    pub fn is_reserved(&self) -> bool {
        self.file_name.eq_ignore_ascii_case("RESERVED") || self.file_name.eq_ignore_ascii_case("SELF")
    }
}

/// The RKAF container.
#[derive(Debug, Clone)]
pub struct AfInfo {
    pub length: u32,
    pub model: String,
    pub id: String,
    pub manufacturer: String,
    pub version: u32,
    pub items: Vec<AfItem>,
}

impl AfInfo {
    pub fn item(&self, name: &str) -> Option<&AfItem> {
        self.items.iter().find(|i| i.name.eq_ignore_ascii_case(name))
    }
}

/// A parsed RKFW image, with the parameter text loaded.
#[derive(Debug, Clone)]
pub struct RkfwImage {
    pub path: PathBuf,
    pub file_size: u64,
    pub header: RkfwHeader,
    pub boot: BootInfo,
    pub af: AfInfo,
    pub parameter_text: String,
    pub parameter: crate::parameter::Parameter,
    /// The 32 hex characters at the end of the file, if present.
    pub md5_hex: Option<String>,
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn wstr(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

fn read_exact_at(f: &mut File, off: u64, len: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(&mut v).with_context(|| format!("short read at offset {off} ({len} bytes)"))?;
    Ok(v)
}

/// Parses the RKBOOT container at `base` inside the file.
pub fn parse_boot(f: &mut File, base: u64, size: u64) -> Result<BootInfo> {
    if size < 0x66 {
        bail!("loader section too small ({size} bytes)");
    }
    let h = read_exact_at(f, base, 0x66)?;
    let magic: [u8; 4] = h[0..4].try_into().unwrap();
    if &magic != BOOT_MAGIC && &magic != LDR_MAGIC {
        bail!("loader section has no BOOT/LDR magic ({:?})", String::from_utf8_lossy(&magic));
    }
    let hsize = u16_at(&h, 4) as u64;
    let version = u32_at(&h, 6);
    let merge_version = u32_at(&h, 10);
    let time = read_time(&h[14..21]);
    let chip = u32_at(&h, 21);
    let mut o = 25;
    let mut groups = Vec::new();
    for _ in 0..3 {
        let count = h[o] as usize;
        let off = u32_at(&h, o + 1) as u64;
        let esize = h[o + 5] as usize;
        groups.push((count, off, esize));
        o += 6;
    }
    let sign_flag = h[o];
    let rc4_flag = h[o + 1];
    let mut result: Vec<Vec<BootEntry>> = Vec::new();
    for (gi, (count, off, esize)) in groups.iter().enumerate() {
        let mut v = Vec::new();
        if *count > 0 {
            if *esize < 57 {
                bail!("loader entry size {esize} too small");
            }
            if off + (*count as u64) * (*esize as u64) > size {
                bail!("loader entry table {gi} outside the loader section");
            }
            let table = read_exact_at(f, base + off, count * esize)?;
            for i in 0..*count {
                let e = &table[i * esize..(i + 1) * esize];
                let kind = u32_at(e, 1);
                let name = wstr(&e[5..45]);
                let doff = u32_at(e, 45) as u64;
                let dsize = u32_at(e, 49) as u64;
                let delay = u32_at(e, 53);
                if doff + dsize > size {
                    bail!("loader entry {name} data outside the loader section");
                }
                v.push(BootEntry { kind, name, offset: base + doff, size: dsize, delay });
            }
        }
        result.push(v);
    }
    let _ = hsize;
    let entries_loader = result.pop().unwrap();
    let entries_472 = result.pop().unwrap();
    let entries_471 = result.pop().unwrap();
    Ok(BootInfo { magic, version, merge_version, time, chip, sign_flag, rc4_flag, entries_471, entries_472, entries_loader })
}

/// Parses the RKAF container at `base`. `avail` is the number of bytes available after `base`.
pub fn parse_af(f: &mut File, base: u64, avail: u64) -> Result<AfInfo> {
    if avail < 0x8c {
        bail!("firmware section too small ({avail} bytes)");
    }
    let h = read_exact_at(f, base, 0x8c)?;
    if &h[0..4] != RKAF_MAGIC {
        bail!("firmware section has no RKAF magic");
    }
    let length = u32_at(&h, 4);
    let model = cstr(&h[8..0x2a]);
    let id = cstr(&h[0x2a..0x48]);
    let manufacturer = cstr(&h[0x48..0x80]);
    let version = u32_at(&h, 0x84);
    let num = u32_at(&h, 0x88) as usize;
    if num > 64 {
        bail!("firmware section claims {num} items");
    }
    let table = read_exact_at(f, base + 0x8c, num * 0x70)?;
    let mut items = Vec::with_capacity(num);
    for i in 0..num {
        let e = &table[i * 0x70..(i + 1) * 0x70];
        let name = cstr(&e[0..32]);
        let file_name = cstr(&e[32..92]);
        let nand_size = u32_at(e, 92);
        let mut pos = u32_at(e, 96) as u64;
        let nand_addr = u32_at(e, 100);
        let padded_size = u32_at(e, 104) as u64;
        let mut size = u32_at(e, 108) as u64;
        // Items over 4 GiB: afptool stores the high 32 bits of pos and size inside the file
        // name buffer, each behind an 'H' marker (file[0x32] / file[0x33..0x37] for pos,
        // file[0x37] / file[0x38..0x3c] for size). SDDiskTool reads them the same way.
        if e[0x52] == b'H' {
            pos |= (u32_at(e, 0x53) as u64) << 32;
        }
        if e[0x57] == b'H' {
            size |= (u32_at(e, 0x58) as u64) << 32;
        }
        let is_data = !(file_name.eq_ignore_ascii_case("RESERVED") || file_name.eq_ignore_ascii_case("SELF"));
        if is_data && pos + size > avail {
            bail!("item {name} ({size} bytes at {pos}) lies outside the firmware section");
        }
        items.push(AfItem { name, file_name, nand_size, offset: base + pos, nand_addr, padded_size, size });
    }
    Ok(AfInfo { length, model, id, manufacturer, version, items })
}

impl RkfwImage {
    /// Opens and parses an RKFW image. Only the headers and the parameter text are read.
    pub fn open(path: &Path) -> Result<RkfwImage> {
        let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let file_size = f.metadata()?.len();
        if file_size < 0x66 {
            bail!("{} is too small to be an RKFW image", path.display());
        }
        let h = read_exact_at(&mut f, 0, 0x66)?;
        if &h[0..4] != RKFW_MAGIC {
            if &h[0..4] == RKAF_MAGIC {
                bail!("{} is a bare update.img (RKAF) without the loader; an RKFW image is required", path.display());
            }
            bail!("{} is not an RKFW image (magic {:?})", path.display(), String::from_utf8_lossy(&h[0..4]));
        }
        let header = RkfwHeader {
            header_size: u16_at(&h, 4),
            version: u32_at(&h, 6),
            code: u32_at(&h, 10),
            time: read_time(&h[14..21]),
            chip: u32_at(&h, 21),
            loader_offset: u32_at(&h, 25) as u64,
            loader_size: u32_at(&h, 29) as u64,
            image_offset: u32_at(&h, 33) as u64,
            image_size: u32_at(&h, 37) as u64,
        };
        if header.loader_offset + header.loader_size > file_size {
            bail!("loader section lies outside the file");
        }
        if header.image_offset >= file_size {
            bail!("firmware section lies outside the file");
        }
        // The size field is only 32 bits; like rkdeveloptool, take everything up to the MD5.
        let mut md5_hex = None;
        let mut fw_end = file_size;
        if file_size >= 32 {
            let tail = read_exact_at(&mut f, file_size - 32, 32)?;
            if tail.iter().all(|c| c.is_ascii_hexdigit()) {
                md5_hex = Some(String::from_utf8_lossy(&tail).to_ascii_lowercase());
                fw_end = file_size - 32;
            }
        }
        let boot = parse_boot(&mut f, header.loader_offset, header.loader_size)?;
        let af = parse_af(&mut f, header.image_offset, fw_end - header.image_offset)?;
        let param_item = af
            .item("parameter")
            .ok_or_else(|| anyhow!("the firmware has no parameter item"))?;
        if param_item.size > 1 << 20 {
            bail!("parameter item is unreasonably large ({} bytes)", param_item.size);
        }
        let ptext = read_exact_at(&mut f, param_item.offset, param_item.size as usize)?;
        let parameter_text = String::from_utf8_lossy(&ptext).into_owned();
        let parameter = crate::parameter::Parameter::parse(&parameter_text)?;
        Ok(RkfwImage { path: path.to_path_buf(), file_size, header, boot, af, parameter_text, parameter, md5_hex })
    }

    /// Reads an arbitrary byte range of the image file.
    pub fn read_range(&self, off: u64, len: usize) -> Result<Vec<u8>> {
        let mut f = File::open(&self.path)?;
        read_exact_at(&mut f, off, len)
    }

    /// Reads a loader entry as it must appear on the card: descrambled when the RKBOOT header
    /// says the boot ROM wants plain data (`rc4_flag != 0`), zero padded to a multiple of four
    /// sectors (2048 bytes), the rounding SDDiskTool and rkdeveloptool both apply.
    pub fn loader_entry_for_card(&self, name: &str) -> Result<Vec<u8>> {
        let e = self
            .boot
            .loader_entry(name)
            .ok_or_else(|| anyhow!("loader has no {name} entry"))?;
        let mut d = self.read_range(e.offset, e.size as usize)?;
        if self.boot.rc4_flag != 0 {
            crate::rc4::rc4_sectors(&mut d);
        }
        let padded = (d.len() + 2047) & !2047;
        d.resize(padded, 0);
        Ok(d)
    }

    /// Checks the CRC32 stored in the last four bytes of the RKBOOT blob (Rockchip's CRC32
    /// variant with polynomial 0x04c10db7, as used by the boot_merger tool).
    pub fn check_loader_crc(&self) -> Result<bool> {
        let size = self.header.loader_size as usize;
        if size < 4 {
            bail!("loader section too small");
        }
        let d = self.read_range(self.header.loader_offset, size)?;
        let stored = u32::from_le_bytes(d[size - 4..].try_into().unwrap());
        Ok(crate::rkcrc::crc32_rk(&d[..size - 4]) == stored)
    }

    /// The major Android version this firmware is, read from the boot image header exactly as
    /// the Rockchip bootloader reads it (`os_version` bits 25..31). `None` when the boot item is
    /// missing or is not an Android boot image.
    pub fn android_major_version(&self) -> Option<u32> {
        let item = self.af.item("boot")?;
        if item.size < 2048 {
            return None;
        }
        let h = self.read_range(item.offset, 48).ok()?;
        if &h[0..8] != b"ANDROID!" {
            return None;
        }
        let os_version = u32::from_le_bytes(h[44..48].try_into().ok()?);
        let major = (os_version >> 25) & 0x7f;
        // 0x7f is the GKI marker, which the bootloader treats as "new enough".
        Some(major)
    }

    /// Computes the MD5 of the file body and compares it to the stored digest.
    /// Returns Ok(None) when the image carries no digest.
    pub fn check_md5(&self, mut progress: impl FnMut(u64, u64)) -> Result<Option<bool>> {
        let Some(expected) = &self.md5_hex else { return Ok(None) };
        let mut f = File::open(&self.path)?;
        let total = self.file_size - 32;
        let mut ctx = crate::md5::Md5::new();
        let mut buf = vec![0u8; 4 << 20];
        let mut done = 0u64;
        while done < total {
            let n = std::cmp::min(buf.len() as u64, total - done) as usize;
            f.read_exact(&mut buf[..n])?;
            ctx.update(&buf[..n]);
            done += n as u64;
            progress(done, total);
        }
        let got = ctx.finish_hex();
        Ok(Some(&got == expected))
    }
}

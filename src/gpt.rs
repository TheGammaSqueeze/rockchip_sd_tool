//! GPT construction, the way Rockchip's SDDiskTool does it (its `create_gpt_buffer`, a variant of
//! the one in rkdeveloptool).
//!
//! Layout: a protective MBR in sector 0, the header in sector 1, 128 entries of 128 bytes in
//! sectors 2..34, and the backup copy at the end of the disk (entries in the 32 sectors before the
//! last one, header in the last sector). Partition type and unique GUIDs are random version 4
//! UUIDs unless the parameter file overrides the unique GUID with a `uuid:` line. The tool
//! reserves 64 sectors at the end of the disk when the card is 4 GiB or larger (33 below that), so
//! a `grow` partition ends at `total - 65`; with the usual card sizes that makes it end on a
//! 64-sector boundary (SDDiskTool v1.65 release note). The header's last usable LBA is written
//! as `total - 34` (see [`last_usable_lba`]).

use anyhow::{bail, Result};
use rand::Rng as _;

use crate::parameter::Parameter;

pub const SECTOR: u64 = 512;
pub const ENTRY_SIZE: usize = 128;
pub const ENTRY_COUNT: usize = 128;
pub const ENTRY_SECTORS: u64 = (ENTRY_SIZE * ENTRY_COUNT) as u64 / SECTOR; // 32
pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
pub const ATTR_BOOTABLE: u64 = 1 << 2;

/// The bytes of a complete partition table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptImage {
    /// Sectors 0..34: MBR, header, entries.
    pub primary: Vec<u8>,
    /// 33 sectors: entries then header. Written at `total_sectors - 33`.
    pub backup: Vec<u8>,
    pub total_sectors: u64,
    pub disk_guid: [u8; 16],
    pub entries: Vec<GptEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptEntry {
    pub name: String,
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
}

impl GptEntry {
    pub fn sectors(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }
}

/// Random UUID version 4 in GPT (mixed endian) byte order, generated like rkdeveloptool's
/// gen_rand_uuid (random words, version nibble 4, variant bits 10).
pub fn random_guid() -> [u8; 16] {
    let mut g = [0u8; 16];
    rand::rng().fill_bytes(&mut g);
    // time_hi_and_version is bytes 6..8 little endian in GPT order; the version nibble is the
    // high nibble of byte 7.
    g[7] = (g[7] & 0x0f) | 0x40;
    g[8] = (g[8] & 0x3f) | 0x80;
    g
}

/// Formats a GPT (mixed endian) GUID as text.
pub fn guid_to_string(g: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g[3], g[2], g[1], g[0], g[5], g[4], g[7], g[6], g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    )
}

/// Sectors SDDiskTool keeps free at the end of the disk (backup GPT plus alignment slack).
pub fn reserved_tail(total_sectors: u64) -> u64 {
    if total_sectors >= 0x80_0000 {
        0x40
    } else {
        0x21
    }
}

/// The last usable LBA written into the header: the sector before the backup entries, which is
/// where the RG DS Plus firmware (on first boot) and Windows (when the disk is released) move it
/// anyway. SDDiskTool itself writes `total - 65` on big cards, which leaves the header
/// inconsistent with its backup entry position and gets "repaired" by both.
pub fn last_usable_lba(total_sectors: u64) -> u64 {
    total_sectors - 34
}

/// The end (exclusive) of a grow partition: SDDiskTool's value (64 sectors of tail reserve on
/// cards of 4 GiB and more), so the partition entries match its output exactly.
pub fn grow_end(total_sectors: u64) -> u64 {
    total_sectors - reserved_tail(total_sectors)
}

/// Sectors the card must have so that every fixed partition plus the backup GPT fits and the
/// grow partition is not empty.
pub fn minimum_sectors(param: &Parameter) -> u64 {
    let mut end = param.fixed_end();
    for p in &param.partitions {
        if p.is_grow() {
            end = end.max(p.offset + 64);
        }
    }
    // Room for the reserved tail (64 sectors on cards of 4 GiB and more, 33 below).
    end + 0x41
}

pub fn build(param: &Parameter, total_sectors: u64) -> Result<GptImage> {
    build_with(param, total_sectors, random_guid)
}

/// Same as [`build`] with a caller-supplied GUID source (used by the tests for determinism).
pub fn build_with(param: &Parameter, total_sectors: u64, mut guid: impl FnMut() -> [u8; 16]) -> Result<GptImage> {
    if param.partitions.len() > ENTRY_COUNT {
        bail!("parameter lists {} partitions, GPT holds at most {ENTRY_COUNT}", param.partitions.len());
    }
    let min = minimum_sectors(param);
    if total_sectors < min {
        bail!(
            "the card has {} sectors ({}) but the layout needs at least {} ({})",
            total_sectors,
            crate::util::human_bytes(total_sectors * SECTOR),
            min,
            crate::util::human_bytes(min * SECTOR)
        );
    }
    let last_usable = last_usable_lba(total_sectors);
    let mut entries = Vec::new();
    for p in &param.partitions {
        let first = p.offset;
        let last = match p.size {
            Some(s) => first + s - 1,
            None => grow_end(total_sectors) - 1,
        };
        if p.is_grow() && last < first {
            bail!("partition {} has no room on this card", p.name);
        }
        if last > last_usable {
            bail!("partition {} ends at sector {} beyond the last usable sector {}", p.name, last, last_usable);
        }
        if last < first {
            bail!("partition {} has no room on this card", p.name);
        }
        entries.push(GptEntry {
            name: p.name.clone(),
            type_guid: guid(),
            unique_guid: p.uuid.unwrap_or_else(&mut guid),
            first_lba: first,
            last_lba: last,
            attributes: if p.bootable { ATTR_BOOTABLE } else { 0 },
        });
    }
    let disk_guid = guid();

    // Entry array.
    let mut array = vec![0u8; ENTRY_SIZE * ENTRY_COUNT];
    for (i, e) in entries.iter().enumerate() {
        let o = i * ENTRY_SIZE;
        array[o..o + 16].copy_from_slice(&e.type_guid);
        array[o + 16..o + 32].copy_from_slice(&e.unique_guid);
        array[o + 32..o + 40].copy_from_slice(&e.first_lba.to_le_bytes());
        array[o + 40..o + 48].copy_from_slice(&e.last_lba.to_le_bytes());
        array[o + 48..o + 56].copy_from_slice(&e.attributes.to_le_bytes());
        for (k, u) in e.name.encode_utf16().take(36).enumerate() {
            array[o + 56 + k * 2..o + 58 + k * 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    let array_crc = crc32fast::hash(&array);

    let header = |my_lba: u64, alt_lba: u64, entries_lba: u64| -> Vec<u8> {
        let mut h = vec![0u8; 92];
        h[0..8].copy_from_slice(GPT_SIGNATURE);
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        h[40..48].copy_from_slice(&34u64.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid);
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&(ENTRY_COUNT as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
        h[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let crc = crc32fast::hash(&h);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h.resize(SECTOR as usize, 0);
        h
    };

    // Protective MBR.
    let mut mbr = vec![0u8; SECTOR as usize];
    mbr[0x1be + 4] = 0xee;
    mbr[0x1be + 8..0x1be + 12].copy_from_slice(&1u32.to_le_bytes());
    mbr[0x1be + 12..0x1be + 16].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
    mbr[0x1fe] = 0x55;
    mbr[0x1ff] = 0xaa;

    let mut primary = mbr;
    primary.extend_from_slice(&header(1, total_sectors - 1, 2));
    primary.extend_from_slice(&array);

    // The backup header points at the entries right before it (total - 33), as the tool does.
    let mut backup = array.clone();
    backup.extend_from_slice(&header(total_sectors - 1, 1, total_sectors - 33));

    Ok(GptImage { primary, backup, total_sectors, disk_guid, entries })
}

/// Sector at which the backup GPT (33 sectors) is written.
pub fn backup_sector(total_sectors: u64) -> u64 {
    total_sectors - 33
}

/// A parsed GPT header for verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHeader {
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub disk_guid: [u8; 16],
    pub entries_lba: u64,
    pub entry_count: u32,
    pub entry_size: u32,
    pub array_crc: u32,
    pub header_crc_ok: bool,
}

pub fn parse_header(sector: &[u8]) -> Result<ParsedHeader> {
    if sector.len() < 92 || &sector[0..8] != GPT_SIGNATURE {
        bail!("no GPT signature");
    }
    let hsize = u32::from_le_bytes(sector[12..16].try_into().unwrap()) as usize;
    if !(92..=sector.len()).contains(&hsize) {
        bail!("bad GPT header size {hsize}");
    }
    let stored_crc = u32::from_le_bytes(sector[16..20].try_into().unwrap());
    let mut copy = sector[..hsize].to_vec();
    copy[16..20].fill(0);
    let u64_at = |o: usize| u64::from_le_bytes(sector[o..o + 8].try_into().unwrap());
    let u32_at = |o: usize| u32::from_le_bytes(sector[o..o + 4].try_into().unwrap());
    Ok(ParsedHeader {
        my_lba: u64_at(24),
        alternate_lba: u64_at(32),
        first_usable: u64_at(40),
        last_usable: u64_at(48),
        disk_guid: sector[56..72].try_into().unwrap(),
        entries_lba: u64_at(72),
        entry_count: u32_at(80),
        entry_size: u32_at(84),
        array_crc: u32_at(88),
        header_crc_ok: crc32fast::hash(&copy) == stored_crc,
    })
}

pub fn parse_entries(array: &[u8], count: u32, size: u32) -> Vec<GptEntry> {
    let mut v = Vec::new();
    for i in 0..count as usize {
        let o = i * size as usize;
        if o + ENTRY_SIZE > array.len() {
            break;
        }
        let e = &array[o..o + ENTRY_SIZE];
        if e[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let units: Vec<u16> = e[56..128].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        v.push(GptEntry {
            name: String::from_utf16_lossy(&units[..end]),
            type_guid: e[0..16].try_into().unwrap(),
            unique_guid: e[16..32].try_into().unwrap(),
            first_lba: u64::from_le_bytes(e[32..40].try_into().unwrap()),
            last_lba: u64::from_le_bytes(e[40..48].try_into().unwrap()),
            attributes: u64::from_le_bytes(e[48..56].try_into().unwrap()),
        });
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_card() {
        // Values observed on a 250347520-sector card made by SDDiskTool v1.69.
        let text = "TYPE: GPT\nCMDLINE:mtdparts=rk29xxnand:0x00002000@0x00002000(security),-@0x01225400(userdata:grow)\n";
        let p = Parameter::parse(text).unwrap();
        let g = build(&p, 250347520).unwrap();
        let h = parse_header(&g.primary[512..1024]).unwrap();
        assert!(h.header_crc_ok);
        assert_eq!(h.my_lba, 1);
        assert_eq!(h.alternate_lba, 250347519);
        assert_eq!(h.first_usable, 34);
        assert_eq!(h.last_usable, 250347486);
        assert_eq!(h.entries_lba, 2);
        assert_eq!(g.entries[0].first_lba, 0x2000);
        assert_eq!(g.entries[0].last_lba, 0x3fff);
        assert_eq!(g.entries[1].last_lba, 250347455);
        let b = parse_header(&g.backup[32 * 512..]).unwrap();
        assert!(b.header_crc_ok);
        assert_eq!(b.my_lba, 250347519);
        assert_eq!(b.alternate_lba, 1);
        assert_eq!(b.entries_lba, 250347487);
        assert_eq!(b.array_crc, h.array_crc);
        assert_eq!(crc32fast::hash(&g.primary[1024..]), h.array_crc);
        assert_eq!(backup_sector(250347520), 250347487);
    }

    #[test]
    fn too_small() {
        let text = "TYPE: GPT\nCMDLINE:mtdparts=rk29xxnand:0x1000@0x2000(a),-@0x3000(b:grow)\n";
        let p = Parameter::parse(text).unwrap();
        assert!(build(&p, 0x3000).is_err());
        assert!(build(&p, minimum_sectors(&p)).is_ok());
        // Small cards reserve 33 sectors for the grow partition end, big ones 64.
        assert_eq!(grow_end(0x7f_ffff), 0x7f_ffff - 33);
        assert_eq!(grow_end(0x80_0000), 0x80_0000 - 64);
    }
}

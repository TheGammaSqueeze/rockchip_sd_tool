//! A minimal FAT32 writer, enough to lay out the data partition of a firmware update card.
//!
//! The card Rockchip's tool calls an "upgrade card" carries the firmware image in a FAT
//! filesystem that the device's recovery reads. Nothing here needs to be a general filesystem: the
//! partition is made from scratch and holds a handful of files that are written once and never
//! changed, so every file is laid out contiguously from the start of the data region and the whole
//! filesystem can be described up front.
//!
//! That matters for this tool: the result is a list of byte ranges with a source each, so the
//! firmware image stays a reference to the file on disk instead of being held in memory, and the
//! usual read-back verification covers the filesystem exactly as it covers everything else.

use anyhow::{bail, Result};

use crate::plan::Source;

pub const SECTOR: u64 = 512;
/// FAT32 is only valid above this many clusters; below it a reader may take the volume for FAT16.
pub const MIN_CLUSTERS: u64 = 65525;
/// Largest file FAT32 can hold.
pub const MAX_FILE: u64 = u32::MAX as u64;

const RESERVED_SECTORS: u64 = 32;
const NUM_FATS: u64 = 2;
const FSINFO_SECTOR: u64 = 1;
const BACKUP_BOOT_SECTOR: u64 = 6;
const ROOT_CLUSTER: u32 = 2;
const DIR_ENTRY: usize = 32;

/// A file to place in the filesystem.
pub struct FatFile {
    pub name: String,
    pub len: u64,
    pub source: Source,
}

/// One range of the finished partition, at `offset` bytes from its start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub offset: u64,
    pub len: u64,
    pub source: Source,
}

/// The geometry chosen for a partition of `sectors` sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub sectors: u64,
    pub sectors_per_cluster: u64,
    pub sectors_per_fat: u64,
    pub cluster_count: u64,
    pub data_start: u64,
}

impl Geometry {
    pub fn cluster_bytes(&self) -> u64 {
        self.sectors_per_cluster * SECTOR
    }
    /// First sector of a cluster.
    pub fn cluster_sector(&self, cluster: u32) -> u64 {
        self.data_start + (cluster as u64 - 2) * self.sectors_per_cluster
    }
}

/// Cluster size by volume size, following the sizes Windows picks for FAT32.
fn sectors_per_cluster_for(sectors: u64) -> u64 {
    let mb = sectors * SECTOR / (1 << 20);
    match mb {
        0..=259 => 1,
        260..=8191 => 8,      // 4 KiB
        8192..=16383 => 16,   // 8 KiB
        16384..=32767 => 32,  // 16 KiB
        _ => 64,              // 32 KiB
    }
}

/// Works out the FAT32 geometry for a partition, growing the cluster size until the table fits
/// and shrinking it if the volume would have too few clusters to be FAT32 at all.
pub fn geometry(sectors: u64) -> Result<Geometry> {
    if sectors <= RESERVED_SECTORS + NUM_FATS {
        bail!("the data partition is too small for a filesystem ({sectors} sectors)");
    }
    let mut spc = sectors_per_cluster_for(sectors);
    loop {
        // Size the table by iteration rather than by the usual closed-form estimate, which can
        // come out a sector or two short: a bigger table leaves fewer clusters, which needs a
        // smaller table, so this settles after a couple of rounds and is exact.
        let usable = sectors - RESERVED_SECTORS;
        let denom = (256 * spc) + NUM_FATS;
        let mut sectors_per_fat = (usable + denom - 1) / denom;
        let mut cluster_count;
        let mut data_start;
        loop {
            data_start = RESERVED_SECTORS + NUM_FATS * sectors_per_fat;
            if data_start >= sectors {
                bail!("the data partition is too small for a FAT32 filesystem");
            }
            cluster_count = (sectors - data_start) / spc;
            let needed = ((cluster_count + 2) * 4 + SECTOR - 1) / SECTOR;
            if needed <= sectors_per_fat {
                break;
            }
            sectors_per_fat = needed;
        }
        if cluster_count < MIN_CLUSTERS {
            if spc == 1 {
                bail!(
                    "the data partition holds only {cluster_count} clusters, too few for FAT32 (needs {MIN_CLUSTERS})"
                );
            }
            spc /= 2;
            continue;
        }
        if cluster_count > 0x0fff_fff5 {
            spc *= 2;
            continue;
        }
        return Ok(Geometry { sectors, sectors_per_cluster: spc, sectors_per_fat, cluster_count, data_start });
    }
}

fn short_name(name: &str, index: usize) -> [u8; 11] {
    let mut out = [b' '; 11];
    let up: String = name.to_ascii_uppercase();
    let (stem, ext) = match up.rsplit_once('.') {
        Some((s, e)) => (s, e),
        None => (up.as_str(), ""),
    };
    let ok = |c: char| c.is_ascii_alphanumeric() || "$%'-_@~`!(){}^#&".contains(c);
    let stem_chars: Vec<u8> = stem.chars().filter(|c| ok(*c)).map(|c| c as u8).collect();
    let ext_chars: Vec<u8> = ext.chars().filter(|c| ok(*c)).map(|c| c as u8).collect();
    let needs_tail = stem_chars.len() > 8 || stem.chars().any(|c| !ok(c));
    if needs_tail {
        // "LONGNA~1" style, which is what a reader shows when it cannot use the long name.
        let keep = std::cmp::min(6, stem_chars.len());
        out[..keep].copy_from_slice(&stem_chars[..keep]);
        out[keep] = b'~';
        out[keep + 1] = b'0' + (index as u8 % 10);
    } else {
        let keep = std::cmp::min(8, stem_chars.len());
        out[..keep].copy_from_slice(&stem_chars[..keep]);
    }
    let keep = std::cmp::min(3, ext_chars.len());
    out[8..8 + keep].copy_from_slice(&ext_chars[..keep]);
    out
}

fn short_name_checksum(name: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &c in name.iter() {
        sum = (sum >> 1) | (sum << 7);
        sum = sum.wrapping_add(c);
    }
    sum
}

/// True when the 8.3 entry alone reproduces the name exactly, case included, so no long-name
/// entries are needed. The comparison is case sensitive on purpose: a plain 8.3 entry is always
/// upper case, so a lower-case name like `sdupdate.img` still gets a long name and reaches the
/// device spelled the way the firmware asks for it, whatever the reader's case rules are.
fn fits_short(name: &str, short: &[u8; 11]) -> bool {
    let rebuilt = {
        let stem = String::from_utf8_lossy(&short[..8]).trim_end().to_string();
        let ext = String::from_utf8_lossy(&short[8..]).trim_end().to_string();
        if ext.is_empty() {
            stem
        } else {
            format!("{stem}.{ext}")
        }
    };
    rebuilt == name
}

fn push_lfn_entries(out: &mut Vec<u8>, name: &str, checksum: u8) {
    let units: Vec<u16> = name.encode_utf16().collect();
    let parts = (units.len() + 12) / 13;
    for part in (0..parts).rev() {
        let mut e = [0u8; DIR_ENTRY];
        let seq = (part + 1) as u8;
        e[0] = if part == parts - 1 { seq | 0x40 } else { seq };
        e[11] = 0x0f; // long name attribute
        e[13] = checksum;
        let slots: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (i, off) in slots.iter().enumerate() {
            let idx = part * 13 + i;
            let v: u16 = match idx.cmp(&units.len()) {
                std::cmp::Ordering::Less => units[idx],
                std::cmp::Ordering::Equal => 0x0000,
                std::cmp::Ordering::Greater => 0xffff,
            };
            e[*off..*off + 2].copy_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&e);
    }
}

fn dir_entry(short: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; DIR_ENTRY] {
    let mut e = [0u8; DIR_ENTRY];
    e[..11].copy_from_slice(short);
    e[11] = attr;
    // A fixed timestamp keeps the output reproducible: 2026-01-01 00:00:00.
    let date: u16 = ((2026 - 1980) << 9) | (1 << 5) | 1;
    e[16..18].copy_from_slice(&date.to_le_bytes()); // created
    e[18..20].copy_from_slice(&date.to_le_bytes()); // accessed
    e[22..24].copy_from_slice(&0u16.to_le_bytes()); // write time
    e[24..26].copy_from_slice(&date.to_le_bytes()); // write date
    e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    e[26..28].copy_from_slice(&((cluster & 0xffff) as u16).to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

/// Builds the filesystem. Returns the geometry and the ranges that make up the partition; every
/// range not mentioned is zero.
pub fn build(sectors: u64, label: &str, files: Vec<FatFile>, volume_id: u32) -> Result<(Geometry, Vec<Piece>)> {
    let g = geometry(sectors)?;
    let cluster_bytes = g.cluster_bytes();

    // Allocate: the root directory takes cluster 2, then each file follows contiguously.
    let mut next = ROOT_CLUSTER + 1;
    let mut placed: Vec<(u32, u64)> = Vec::new(); // (first cluster, cluster count)
    for f in &files {
        if f.len > MAX_FILE {
            bail!("{} is {} bytes; FAT32 cannot hold a file of 4 GiB or more", f.name, f.len);
        }
        let clusters = if f.len == 0 { 0 } else { (f.len + cluster_bytes - 1) / cluster_bytes };
        placed.push((if clusters == 0 { 0 } else { next }, clusters));
        next += clusters as u32;
    }
    let used_clusters = (next - ROOT_CLUSTER) as u64;
    if used_clusters > g.cluster_count {
        let need = used_clusters * cluster_bytes;
        bail!(
            "the files need {} but the data partition only holds {}",
            crate::util::human_bytes(need),
            crate::util::human_bytes(g.cluster_count * cluster_bytes)
        );
    }

    // The file allocation table: one entry per cluster, each pointing at the next.
    let mut fat = vec![0u8; (g.sectors_per_fat * SECTOR) as usize];
    let mut put = |cluster: u32, value: u32| {
        let o = cluster as usize * 4;
        if o + 4 <= fat.len() {
            fat[o..o + 4].copy_from_slice(&value.to_le_bytes());
        }
    };
    put(0, 0x0fff_fff8);
    put(1, 0x0fff_ffff);
    put(ROOT_CLUSTER, 0x0fff_ffff); // the root directory is one cluster
    for (first, count) in &placed {
        for i in 0..*count {
            let c = *first + i as u32;
            let v = if i + 1 == *count { 0x0fff_ffff } else { c + 1 };
            put(c, v);
        }
    }

    // The root directory: a volume label, then each file.
    let mut dir: Vec<u8> = Vec::new();
    let mut lbl = [b' '; 11];
    for (i, c) in label.to_ascii_uppercase().bytes().take(11).enumerate() {
        lbl[i] = c;
    }
    dir.extend_from_slice(&dir_entry(&lbl, 0x08, 0, 0));
    for (i, f) in files.iter().enumerate() {
        let short = short_name(&f.name, i + 1);
        if !fits_short(&f.name, &short) {
            push_lfn_entries(&mut dir, &f.name, short_name_checksum(&short));
        }
        dir.extend_from_slice(&dir_entry(&short, 0x20, placed[i].0, f.len as u32));
    }
    if dir.len() as u64 > cluster_bytes {
        bail!("too many files for a single-cluster root directory");
    }
    dir.resize(cluster_bytes as usize, 0);

    // Boot sector.
    let mut boot = vec![0u8; SECTOR as usize];
    boot[0..3].copy_from_slice(&[0xeb, 0x58, 0x90]); // jump, as every reader expects
    boot[3..11].copy_from_slice(b"MSWIN4.1");
    boot[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    boot[13] = g.sectors_per_cluster as u8;
    boot[14..16].copy_from_slice(&(RESERVED_SECTORS as u16).to_le_bytes());
    boot[16] = NUM_FATS as u8;
    boot[17..19].copy_from_slice(&0u16.to_le_bytes()); // root entries: zero on FAT32
    boot[19..21].copy_from_slice(&0u16.to_le_bytes()); // small sector count: unused
    boot[21] = 0xf8; // fixed disk
    boot[22..24].copy_from_slice(&0u16.to_le_bytes()); // FAT size 16: unused
    boot[24..26].copy_from_slice(&63u16.to_le_bytes()); // sectors per track
    boot[26..28].copy_from_slice(&255u16.to_le_bytes()); // heads
    boot[28..32].copy_from_slice(&0u32.to_le_bytes()); // hidden sectors, filled by the caller
    boot[32..36].copy_from_slice(&(sectors as u32).to_le_bytes());
    boot[36..40].copy_from_slice(&(g.sectors_per_fat as u32).to_le_bytes());
    boot[40..42].copy_from_slice(&0u16.to_le_bytes()); // flags: mirror all FATs
    boot[42..44].copy_from_slice(&0u16.to_le_bytes()); // version
    boot[44..48].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
    boot[48..50].copy_from_slice(&(FSINFO_SECTOR as u16).to_le_bytes());
    boot[50..52].copy_from_slice(&(BACKUP_BOOT_SECTOR as u16).to_le_bytes());
    boot[64] = 0x80; // drive number
    boot[66] = 0x29; // extended boot signature
    boot[67..71].copy_from_slice(&volume_id.to_le_bytes());
    boot[71..82].copy_from_slice(&lbl);
    boot[82..90].copy_from_slice(b"FAT32   ");
    boot[510] = 0x55;
    boot[511] = 0xaa;

    // FSInfo.
    let mut fsinfo = vec![0u8; SECTOR as usize];
    fsinfo[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    fsinfo[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    let free = g.cluster_count - used_clusters;
    fsinfo[488..492].copy_from_slice(&(free as u32).to_le_bytes());
    fsinfo[492..496].copy_from_slice(&next.to_le_bytes());
    fsinfo[510] = 0x55;
    fsinfo[511] = 0xaa;

    // Lay it all out.
    let mut pieces = Vec::new();
    let boot_arc: std::sync::Arc<Vec<u8>> = std::sync::Arc::new(boot);
    let fat_arc: std::sync::Arc<Vec<u8>> = std::sync::Arc::new(fat);
    pieces.push(Piece { offset: 0, len: SECTOR, source: Source::Bytes(boot_arc.clone()) });
    pieces.push(Piece { offset: FSINFO_SECTOR * SECTOR, len: SECTOR, source: Source::Bytes(fsinfo.into()) });
    pieces.push(Piece { offset: BACKUP_BOOT_SECTOR * SECTOR, len: SECTOR, source: Source::Bytes(boot_arc) });
    for i in 0..NUM_FATS {
        pieces.push(Piece {
            offset: (RESERVED_SECTORS + i * g.sectors_per_fat) * SECTOR,
            len: g.sectors_per_fat * SECTOR,
            source: Source::Bytes(fat_arc.clone()),
        });
    }
    pieces.push(Piece { offset: g.cluster_sector(ROOT_CLUSTER) * SECTOR, len: cluster_bytes, source: Source::Bytes(dir.into()) });
    for (i, f) in files.into_iter().enumerate() {
        if placed[i].1 == 0 {
            continue;
        }
        pieces.push(Piece { offset: g.cluster_sector(placed[i].0) * SECTOR, len: f.len, source: f.source });
    }
    pieces.sort_by_key(|p| p.offset);
    Ok((g, pieces))
}

/// Patches the hidden-sector count, which has to be the partition's own start on the disk.
pub fn set_hidden_sectors(pieces: &mut [Piece], start_lba: u64) {
    for p in pieces.iter_mut() {
        if p.offset == 0 || p.offset == BACKUP_BOOT_SECTOR * SECTOR {
            if let Source::Bytes(b) = &p.source {
                let mut v = (**b).clone();
                v[28..32].copy_from_slice(&(start_lba as u32).to_le_bytes());
                p.source = Source::Bytes(v.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_is_valid_fat32() {
        for gb in [1u64, 4, 8, 16, 32, 64, 128, 512] {
            let sectors = gb * (1 << 30) / SECTOR;
            let g = geometry(sectors).unwrap();
            assert!(g.cluster_count >= MIN_CLUSTERS, "{gb} GiB: {} clusters", g.cluster_count);
            assert!(g.cluster_count <= 0x0fff_fff5);
            // The table must be able to address every cluster it claims.
            assert!((g.cluster_count + 2) * 4 <= g.sectors_per_fat * SECTOR);
            assert!(g.data_start + g.cluster_count * g.sectors_per_cluster <= sectors);
        }
        // Too small to be FAT32 at all.
        assert!(geometry(1024).is_err());
    }

    #[test]
    fn short_names_and_long_names() {
        let s = short_name("sdupdate.img", 1);
        assert_eq!(&s, b"SDUPDATEIMG");
        // Lower case does not survive an 8.3 entry, so it still needs a long name.
        assert!(!fits_short("sdupdate.img", &s));
        assert!(fits_short("SDUPDATE.IMG", &s));
        let s = short_name("rksdfw.tag", 1);
        assert_eq!(&s, b"RKSDFW  TAG");
        assert!(!fits_short("rksdfw.tag", &s));
        // Too long for 8.3, so it needs long-name entries and a numbered alias.
        let s = short_name("sd_boot_config.config", 1);
        assert_eq!(&s, b"SD_BOO~1CON");
        assert!(!fits_short("sd_boot_config.config", &s));
    }

    #[test]
    fn lays_out_a_volume_that_reads_back() {
        let sectors = 2 * (1 << 30) / SECTOR; // 2 GiB
        let files = vec![
            FatFile { name: "sdupdate.img".into(), len: 5_000_000, source: Source::File { offset: 0, len: 5_000_000 } },
            FatFile { name: "rksdfw.tag".into(), len: 4, source: Source::Bytes(vec![1, 2, 3, 4].into()) },
            FatFile { name: "sd_boot_config.config".into(), len: 10, source: Source::Bytes(vec![b'x'; 10].into()) },
        ];
        let (g, pieces) = build(sectors, "UPGRADE", files, 0x1234_5678).unwrap();
        // Nothing overlaps and everything is inside the partition.
        let mut last_end = 0u64;
        for p in &pieces {
            assert!(p.offset >= last_end, "overlap at {}", p.offset);
            last_end = p.offset + p.len;
            assert!(last_end <= sectors * SECTOR);
        }
        // The first file begins right after the root directory cluster.
        let data = g.cluster_sector(3) * SECTOR;
        assert!(pieces.iter().any(|p| p.offset == data && p.len == 5_000_000));
    }
}

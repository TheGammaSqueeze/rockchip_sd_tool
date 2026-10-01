//! Turns an RKFW image plus a card size into the exact list of writes SDDiskTool performs in its
//! "SD Boot" mode, in the same order:
//!
//! 1. clear the first two sectors (MBR and primary GPT header);
//! 2. the loader (`FlashHead`, `FlashData`, `FlashBoot`) at sector 64, 68 and 68 + data sectors;
//! 3. every firmware item that has a partition address, in table order, at its partition offset,
//!    Android sparse images expanded (zeros first, then the chunks);
//! 4. the primary and backup GPT.
//!
//! The plan is a list of sector-granular operations. Random-access targets execute it in this
//! order; streaming targets get it flattened into ascending, non-overlapping ranges first.
//!
//! In [`Mode::Upgrade`] steps 1 and 4 are left out: the card keeps the partition table it already
//! has, so nothing outside the partitions the image carries is touched and user data survives.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use crate::gpt;
use crate::rkfw::RkfwImage;
use crate::sparse::{self, Chunk, SparseHeader};

pub const SECTOR: u64 = 512;
/// Where the loader goes (SDDiskTool `IDBLOCK_POS`, default 64).
pub const IDBLOCK_POS: u64 = 64;

/// Partitions an upgrade never writes, because they hold the state of the device rather than
/// firmware. Writing them from the image is what a factory flash does, and it is exactly what
/// destroys what an upgrade is meant to keep:
///
/// * `misc` carries the bootloader control block. Rockchip firmware ships it with the command
///   `boot-recovery` and the recovery argument `--wipe_all`, so a device that is given the
///   image's `misc` wipes user data on its next boot. This is intended for a factory flash and
///   must not happen during an upgrade.
/// * `metadata` holds the keys that user data is encrypted with; replacing it makes the existing
///   user data unreadable, which is a wipe in all but name.
/// * `cache`, `frp`, `swap`, `backup` and `userdata` are scratch or user state as well.
pub const UPGRADE_KEEPS: &[&str] = &["misc", "cache", "metadata", "userdata", "frp", "swap", "backup"];

/// True when an upgrade must leave this partition alone.
pub fn upgrade_keeps(name: &str) -> bool {
    UPGRADE_KEEPS.iter().any(|k| k.eq_ignore_ascii_case(name))
}

/// Partitions whose filesystem signature a full write clears, so the device formats them itself.
///
/// The firmware carries no image for them, so a card keeps whatever the previous owner of those
/// sectors left behind. Android is meant to be handed a blank one: `metadata` holds the keys
/// `userdata` is encrypted with, and the pair only works when both are made together. The vendor
/// arranges that by asking recovery to wipe on the first boot, but a device that never reaches
/// recovery, or reaches it without a command, is then left with a half-made filesystem it cannot
/// repair, which is a card that boots to the recovery menu and stays there.
///
/// Clearing the signature costs a few megabytes and makes the first boot deterministic: the
/// filesystems are absent, so Android creates them whether or not the recovery wipe runs.
pub const ERASE_ON_FULL_WRITE: &[&str] = &["metadata", "cache", "userdata"];

/// How much of each of those partitions is cleared. A filesystem's primary superblock lives in
/// the first few kilobytes (f2fs at 1 KiB, ext4 at 1 KiB); four megabytes covers those and their
/// nearby copies without writing anything substantial.
pub const ERASE_BYTES: u64 = 4 << 20;

/// Size of one Android bootloader control block (`bootloader_message`).
pub const BCB_SIZE: usize = 2048;
/// The two places a bootloader control block can live in a Rockchip `misc` partition: Google's
/// offset 0, and Rockchip's legacy 16 KiB offset.
pub const BCB_OFFSET_GOOGLE: usize = 0;
pub const BCB_OFFSET_ROCKCHIP: usize = 0x4000;

/// Where this firmware's bootloader reads the boot command from, by the same rule the Rockchip
/// bootloader uses: offset 0 from Android 10, the 16 KiB offset before that. `None` when the
/// Android version cannot be determined, in which case the `misc` image is left as it is.
pub fn bcb_offset_for(android_major: Option<u32>) -> Option<usize> {
    match android_major {
        Some(v) if v >= 10 => Some(BCB_OFFSET_GOOGLE),
        Some(_) => Some(BCB_OFFSET_ROCKCHIP),
        None => None,
    }
}

/// Removes the boot command from the control block the bootloader does not read.
///
/// Rockchip firmware ships `misc` with "boot-recovery" and "--wipe_all" written at **both**
/// offsets, so that one image suits bootloaders of either convention. On a card that is a
/// problem: the bootloader acts on one copy, and Android's recovery only ever clears the copy at
/// offset 0. If the bootloader happens to read the other one, it sends the device to recovery on
/// every boot while recovery itself finds no command and sits in its menu, which is a device that
/// never finishes booting and cannot be rescued without rewriting the card.
///
/// So only the copy this firmware's bootloader actually reads is kept; the other is cleared.
pub fn normalize_misc(data: &mut [u8], used: usize) {
    let unused = if used == BCB_OFFSET_GOOGLE { BCB_OFFSET_ROCKCHIP } else { BCB_OFFSET_GOOGLE };
    if unused + BCB_SIZE <= data.len() {
        data[unused..unused + BCB_SIZE].fill(0);
    }
}

/// True when `data` carries a boot command at `offset`.
pub fn bcb_has_command(data: &[u8], offset: usize) -> bool {
    data.get(offset..offset + 32).map(|c| c[0] != 0).unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Zeros.
    Zero,
    /// Bytes from the image file at this offset; shorter than the op length means zero padding.
    File { offset: u64, len: u64 },
    /// A repeated 4-byte pattern.
    Fill([u8; 4]),
    /// Literal bytes (loader, GPT).
    Bytes(std::sync::Arc<Vec<u8>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    /// Name of the step this op belongs to (shown as progress).
    pub step: String,
    pub sector: u64,
    pub sectors: u64,
    pub source: Source,
}

impl Op {
    pub fn bytes(&self) -> u64 {
        self.sectors * SECTOR
    }
    pub fn end_sector(&self) -> u64 {
        self.sector + self.sectors
    }
}

/// What a plan does to the card as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Write the loader, every partition the image carries and a fresh partition table.
    Full,
    /// Write the loader and every partition the image carries onto a card that already has this
    /// layout, keeping its partition table and everything the image does not cover (userdata).
    Upgrade,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub total_sectors: u64,
    pub mode: Mode,
    pub ops: Vec<Op>,
    /// The table this plan writes, or `None` in upgrade mode (the card keeps its own).
    pub gpt: Option<gpt::GptImage>,
    /// The partitions the card is expected to have once the plan has run.
    pub entries: Vec<gpt::GptEntry>,
    /// Human readable summary lines.
    pub notes: Vec<String>,
}

impl Plan {
    pub fn total_bytes(&self) -> u64 {
        self.ops.iter().map(|o| o.bytes()).sum()
    }

    /// Flattens the plan into ascending, non-overlapping ops where later writes win, exactly the
    /// final content of the card. Zero ops are kept (the caller decides whether to skip them).
    pub fn flattened(&self) -> Vec<Op> {
        // Interval map keyed by start sector: start -> (end, index of op, offset into op).
        let mut map: BTreeMap<u64, (u64, usize, u64)> = BTreeMap::new();
        for (idx, op) in self.ops.iter().enumerate() {
            if op.sectors == 0 {
                continue;
            }
            let (s, e) = (op.sector, op.end_sector());
            // Split or remove everything that overlaps [s, e).
            let overlapping: Vec<u64> = map.range(..e).filter(|(_, (oe, _, _))| *oe > s).map(|(k, _)| *k).collect();
            for k in overlapping {
                let (oe, oi, ooff) = map.remove(&k).unwrap();
                if k < s {
                    map.insert(k, (s, oi, ooff));
                }
                if oe > e {
                    map.insert(e, (oe, oi, ooff + (e - k)));
                }
            }
            map.insert(s, (e, idx, 0));
        }
        let mut out = Vec::with_capacity(map.len());
        for (s, (e, idx, off)) in map {
            let op = &self.ops[idx];
            let sectors = e - s;
            let source = match &op.source {
                Source::Zero => Source::Zero,
                Source::Fill(p) => Source::Fill(*p),
                Source::File { offset, len } => {
                    let skip = off * SECTOR;
                    if skip >= *len {
                        Source::Zero
                    } else {
                        let l = std::cmp::min(len - skip, sectors * SECTOR);
                        Source::File { offset: offset + skip, len: l }
                    }
                }
                Source::Bytes(b) => {
                    let skip = (off * SECTOR) as usize;
                    let take = std::cmp::min(b.len().saturating_sub(skip), (sectors * SECTOR) as usize);
                    if take == 0 {
                        Source::Zero
                    } else {
                        Source::Bytes(std::sync::Arc::new(b[skip..skip + take].to_vec()))
                    }
                }
            };
            out.push(Op { step: op.step.clone(), sector: s, sectors, source });
        }
        out
    }
}

fn sectors_for(bytes: u64) -> u64 {
    (bytes + SECTOR - 1) / SECTOR
}

/// Builds the plan for a full write of `img` to a card of `total_sectors` sectors.
pub fn build(img: &RkfwImage, total_sectors: u64) -> Result<Plan> {
    build_inner(img, total_sectors, None)
}

/// Builds the plan for upgrading a card that already carries `existing` partitions: the loader and
/// every partition the image has an image for are rewritten, the partition table and everything
/// else (userdata above all) are left exactly as they are.
pub fn build_upgrade(img: &RkfwImage, total_sectors: u64, existing: &[gpt::GptEntry]) -> Result<Plan> {
    build_inner(img, total_sectors, Some(existing))
}

/// Checks that a card's existing partitions are the layout this image expects. Every fixed
/// partition must be at the same sector with the same size; the growing partition (userdata) only
/// has to start at the same sector, since its size follows the card it was made for.
pub fn check_layout(param: &crate::parameter::Parameter, existing: &[gpt::GptEntry]) -> Result<()> {
    let advice = "this card was not made from a compatible image; write it in full instead of upgrading";
    for p in &param.partitions {
        let Some(e) = existing.iter().find(|e| e.name == p.name) else {
            bail!("the card has no {} partition ({advice})", p.name);
        };
        if e.first_lba != p.offset {
            bail!(
                "the card's {} partition starts at sector {} but this image expects sector {} ({advice})",
                p.name, e.first_lba, p.offset
            );
        }
        if let Some(size) = p.size {
            if e.sectors() != size {
                bail!(
                    "the card's {} partition is {} sectors but this image expects {} ({advice})",
                    p.name, e.sectors(), size
                );
            }
        }
    }
    for e in existing {
        if !param.partitions.iter().any(|p| p.name == e.name) {
            bail!("the card has an extra {} partition that this image does not know ({advice})", e.name);
        }
    }
    Ok(())
}

fn build_inner(img: &RkfwImage, total_sectors: u64, existing: Option<&[gpt::GptEntry]>) -> Result<Plan> {
    if img.parameter.part_type != "GPT" {
        bail!(
            "the image's parameter file has TYPE: {} ; only GPT layouts are supported for SD boot cards",
            if img.parameter.part_type.is_empty() { "(none)" } else { &img.parameter.part_type }
        );
    }
    let mode = if existing.is_some() { Mode::Upgrade } else { Mode::Full };
    let (gpt_img, entries) = match existing {
        Some(e) => {
            check_layout(&img.parameter, e)?;
            (None, e.to_vec())
        }
        None => {
            let g = gpt::build(&img.parameter, total_sectors)?;
            let entries = g.entries.clone();
            (Some(g), entries)
        }
    };
    let mut ops = Vec::new();
    let mut notes = Vec::new();

    // 1. Clear MBR: 1 KiB of zeros at sector 0. An upgrade keeps the table that is there.
    if mode == Mode::Full {
        ops.push(Op { step: "Clear MBR".into(), sector: 0, sectors: 2, source: Source::Zero });
    } else {
        notes.push(
            "upgrade: the partition table, user data and the device's own state (misc, cache, metadata) are left untouched"
                .into(),
        );
    }

    // 2. Loader.
    let head = img.loader_entry_for_card("FlashHead");
    let data = img.loader_entry_for_card("FlashData")?;
    let boot = img.loader_entry_for_card("FlashBoot")?;
    let data_sectors = data.len() as u64 / SECTOR;
    let boot_sectors = boot.len() as u64 / SECTOR;
    match head {
        Ok(head) if !head.is_empty() => {
            let head_sectors = head.len() as u64 / SECTOR;
            ops.push(Op { step: "Loader".into(), sector: IDBLOCK_POS, sectors: head_sectors, source: Source::Bytes(head.into()) });
            notes.push(format!("loader: RKNS header {head_sectors} sectors, DDR {data_sectors} sectors, boot {boot_sectors} sectors at sector {IDBLOCK_POS}"));
        }
        _ => {
            // Legacy chips without a FlashHead entry: a 2 KiB id block built by the tool.
            let idb = legacy_idb(data_sectors, boot_sectors);
            ops.push(Op { step: "Loader".into(), sector: IDBLOCK_POS, sectors: 4, source: Source::Bytes(idb.into()) });
            notes.push(format!("loader: legacy id block, DDR {data_sectors} sectors, boot {boot_sectors} sectors at sector {IDBLOCK_POS}"));
        }
    }
    ops.push(Op { step: "Loader".into(), sector: IDBLOCK_POS + 4, sectors: data_sectors, source: Source::Bytes(data.into()) });
    ops.push(Op { step: "Loader".into(), sector: IDBLOCK_POS + 4 + data_sectors, sectors: boot_sectors, source: Source::Bytes(boot.into()) });
    let loader_end = IDBLOCK_POS + 4 + data_sectors + boot_sectors;
    for p in &img.parameter.partitions {
        if p.offset < loader_end {
            bail!("partition {} at sector {} overlaps the loader (sectors {IDBLOCK_POS}..{loader_end})", p.name, p.offset);
        }
    }

    // 3. Firmware items.
    for item in &img.af.items {
        if item.nand_addr == 0xffff_ffff || item.size == 0 || item.is_reserved() {
            continue;
        }
        if item.name.eq_ignore_ascii_case("parameter") {
            // GPT layouts get their table from the GPT step; the parameter item is not written.
            continue;
        }
        if mode == Mode::Upgrade && upgrade_keeps(&item.name) {
            notes.push(format!("{}: kept as it is (an upgrade does not write it)", item.name));
            continue;
        }
        let start = item.nand_addr as u64;
        let part = img.parameter.partition(&item.name);
        // Extents come from the table the card will have: the one being written, or the one it
        // already carries when upgrading.
        let part_sectors = match entries.iter().find(|e| e.name == item.name) {
            Some(e) => e.sectors(),
            None => {
                // SDDiskTool trusts nand_addr from the packer; without a partition the size is
                // whatever is left on the card (its nand_size field is 0xffffffff then).
                if item.nand_size != 0xffff_ffff && item.nand_size != 0 {
                    item.nand_size as u64
                } else {
                    total_sectors.saturating_sub(start)
                }
            }
        };
        if let Some(p) = part {
            if p.offset != start {
                bail!(
                    "item {} is packed for sector {} but the parameter puts partition {} at sector {}",
                    item.name, start, p.name, p.offset
                );
            }
        }
        // misc carries the boot command. Keep only the copy this firmware's bootloader reads.
        if item.name.eq_ignore_ascii_case("misc") && item.size as usize >= BCB_OFFSET_ROCKCHIP + BCB_SIZE {
            if let Some(used) = bcb_offset_for(img.android_major_version()) {
                let mut data = img.read_range(item.offset, item.size as usize)?;
                let unused = if used == BCB_OFFSET_GOOGLE { BCB_OFFSET_ROCKCHIP } else { BCB_OFFSET_GOOGLE };
                let had_stale = bcb_has_command(&data, unused);
                normalize_misc(&mut data, used);
                let n = sectors_for(data.len() as u64);
                if n > part_sectors {
                    bail!("misc: image ({} sectors) is larger than the partition ({} sectors)", n, part_sectors);
                }
                data.resize((n * SECTOR) as usize, 0);
                ops.push(Op { step: item.name.clone(), sector: start, sectors: n, source: Source::Bytes(data.into()) });
                notes.push(format!(
                    "misc: {} at sector {}; the boot command is kept at offset {:#x}{}",
                    crate::util::human_bytes(item.size),
                    start,
                    used,
                    if had_stale { ", the unused copy is cleared so the device cannot loop into recovery" } else { "" }
                ));
                continue;
            }
        }
        let head = img.read_range(item.offset, std::cmp::min(item.size, 28) as usize)?;
        if let Some(sh) = SparseHeader::parse(&head) {
            sh.validate()?;
            let unsparsed_sectors = sectors_for(sh.expanded_size());
            if unsparsed_sectors > part_sectors {
                bail!(
                    "{}: the unpacked image ({} sectors) is larger than the partition ({} sectors)",
                    item.name, unsparsed_sectors, part_sectors
                );
            }
            if start + part_sectors > total_sectors {
                bail!("{}: partition end {} is beyond the card size {}", item.name, start + part_sectors, total_sectors);
            }
            let step = item.name.clone();
            // Erase the unpacked extent, then the last 64 sectors of the partition.
            ops.push(Op { step: step.clone(), sector: start, sectors: unsparsed_sectors, source: Source::Zero });
            if part_sectors >= 0x40 {
                ops.push(Op { step: step.clone(), sector: start + part_sectors - 0x40, sectors: 0x40, source: Source::Zero });
            }
            let chunks = sparse::walk_chunks(&sh, item.offset, item.size, |o, l| img.read_range(o, l))?;
            let mut raw = 0u64;
            let mut fill = 0u64;
            let mut dont_care = 0u64;
            for c in chunks {
                if c.is_empty() {
                    continue;
                }
                let sec = start + c.out() / SECTOR;
                let n = sectors_for(c.len());
                match c {
                    Chunk::Raw { data_offset, len, .. } => {
                        raw += len;
                        ops.push(Op { step: step.clone(), sector: sec, sectors: n, source: Source::File { offset: data_offset, len } });
                    }
                    Chunk::Fill { pattern, len, .. } => {
                        fill += len;
                        ops.push(Op { step: step.clone(), sector: sec, sectors: n, source: Source::Fill(pattern) });
                    }
                    Chunk::DontCare { len, .. } => {
                        dont_care += len;
                    }
                }
            }
            notes.push(format!(
                "{}: sparse image, {} unpacked ({} raw, {} fill, {} skipped) at sector {}",
                item.name,
                crate::util::human_bytes(sh.expanded_size()),
                crate::util::human_bytes(raw),
                crate::util::human_bytes(fill),
                crate::util::human_bytes(dont_care),
                start
            ));
        } else {
            let n = sectors_for(item.size);
            if n > part_sectors {
                bail!("{}: image ({} sectors) is larger than the partition ({} sectors)", item.name, n, part_sectors);
            }
            if start + n > total_sectors {
                bail!("{}: end {} is beyond the card size {}", item.name, start + n, total_sectors);
            }
            ops.push(Op { step: item.name.clone(), sector: start, sectors: n, source: Source::File { offset: item.offset, len: item.size } });
            notes.push(format!("{}: {} at sector {}", item.name, crate::util::human_bytes(item.size), start));
        }
    }

    // 3a. Clear the filesystem signatures of the partitions the image does not carry, so the
    // device can make them itself and does not depend on the first-boot recovery wipe.
    if mode == Mode::Full {
        for name in ERASE_ON_FULL_WRITE {
            let Some(e) = entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)) else { continue };
            let sectors = std::cmp::min(ERASE_BYTES / SECTOR, e.sectors());
            if sectors == 0 {
                continue;
            }
            ops.push(Op { step: (*name).to_string(), sector: e.first_lba, sectors, source: Source::Zero });
            notes.push(format!(
                "{name}: clearing the first {} so the device makes the filesystem itself",
                crate::util::human_bytes(sectors * SECTOR)
            ));
        }
    }

    // 3b. An upgrade does not write misc, but it does clear a stale boot command sitting in the
    // control block this firmware's bootloader does not read. That stale copy is what leaves a
    // device looping into recovery, and clearing it is the only way back without a full write.
    // No command is ever written here, so an upgrade still cannot ask the device to wipe itself.
    if mode == Mode::Upgrade {
        if let Some(used) = bcb_offset_for(img.android_major_version()) {
            let unused = if used == BCB_OFFSET_GOOGLE { BCB_OFFSET_ROCKCHIP } else { BCB_OFFSET_GOOGLE };
            if let Some(e) = entries.iter().find(|e| e.name.eq_ignore_ascii_case("misc")) {
                let sectors = (BCB_SIZE as u64) / SECTOR;
                let sector = e.first_lba + unused as u64 / SECTOR;
                if sector + sectors <= e.last_lba + 1 {
                    ops.push(Op { step: "misc".into(), sector, sectors, source: Source::Zero });
                    notes.push(format!(
                        "misc: clearing the unused boot control block at offset {unused:#x}; the command at {used:#x} and the rest of misc are untouched"
                    ));
                }
            }
        }
    }

    // 4. GPT: primary (34 sectors at 0), backup (33 sectors at total - 33).
    if let Some(g) = &gpt_img {
        ops.push(Op { step: "GPT".into(), sector: 0, sectors: 34, source: Source::Bytes(g.primary.clone().into()) });
        ops.push(Op { step: "GPT".into(), sector: gpt::backup_sector(total_sectors), sectors: 33, source: Source::Bytes(g.backup.clone().into()) });
    }

    Ok(Plan { total_sectors, mode, ops, gpt: gpt_img, entries, notes })
}

/// The 2 KiB id block SDDiskTool writes for chips without an RKNS `FlashHead` entry. Sector 0
/// carries the tag, the code offsets and sizes and is RC4 scrambled; the rest is zero except for a
/// flag word (1 = SD boot card) in sector 1.
pub fn legacy_idb(data_sectors: u64, boot_sectors: u64) -> Vec<u8> {
    let mut b = vec![0u8; 2048];
    b[0..4].copy_from_slice(&0x0ff0_aa55u32.to_le_bytes());
    b[0x0c..0x0e].copy_from_slice(&4u16.to_le_bytes());
    b[0x0e..0x10].copy_from_slice(&4u16.to_le_bytes());
    b[0x1fa..0x1fc].copy_from_slice(&(data_sectors as u16).to_le_bytes());
    b[0x1fc..0x1fe].copy_from_slice(&((data_sectors + boot_sectors) as u16).to_le_bytes());
    crate::rc4::rc4_sectors(&mut b[..512]);
    b[0x268..0x26c].copy_from_slice(&1u32.to_le_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn misc_with_both_copies() -> Vec<u8> {
        let mut m = vec![0u8; 0xc000];
        for off in [BCB_OFFSET_GOOGLE, BCB_OFFSET_ROCKCHIP] {
            m[off..off + 13].copy_from_slice(b"boot-recovery");
            m[off + 64..off + 64 + 19].copy_from_slice(b"recovery\n--wipe_all");
        }
        m
    }

    #[test]
    fn bcb_offset_follows_the_android_version() {
        assert_eq!(bcb_offset_for(Some(14)), Some(BCB_OFFSET_GOOGLE));
        assert_eq!(bcb_offset_for(Some(10)), Some(BCB_OFFSET_GOOGLE));
        assert_eq!(bcb_offset_for(Some(0x7f)), Some(BCB_OFFSET_GOOGLE)); // GKI marker
        assert_eq!(bcb_offset_for(Some(9)), Some(BCB_OFFSET_ROCKCHIP));
        assert_eq!(bcb_offset_for(None), None);
    }

    #[test]
    fn only_the_used_boot_command_survives() {
        let mut m = misc_with_both_copies();
        assert!(bcb_has_command(&m, BCB_OFFSET_GOOGLE) && bcb_has_command(&m, BCB_OFFSET_ROCKCHIP));
        normalize_misc(&mut m, BCB_OFFSET_GOOGLE);
        assert!(bcb_has_command(&m, BCB_OFFSET_GOOGLE), "the command the bootloader reads stays");
        assert!(!bcb_has_command(&m, BCB_OFFSET_ROCKCHIP), "the stale copy is cleared");
        assert!(m[BCB_OFFSET_ROCKCHIP..BCB_OFFSET_ROCKCHIP + BCB_SIZE].iter().all(|&b| b == 0));
        // Nothing outside the two control blocks is touched.
        let mut other = misc_with_both_copies();
        other[0x9000] = 0xa5;
        let before = other[0x9000];
        normalize_misc(&mut other, BCB_OFFSET_GOOGLE);
        assert_eq!(other[0x9000], before);

        // A pre-Android-10 firmware keeps the 16 KiB copy instead.
        let mut m = misc_with_both_copies();
        normalize_misc(&mut m, BCB_OFFSET_ROCKCHIP);
        assert!(!bcb_has_command(&m, BCB_OFFSET_GOOGLE));
        assert!(bcb_has_command(&m, BCB_OFFSET_ROCKCHIP));
    }

    fn plan_with(ops: Vec<Op>) -> Plan {
        let p = crate::parameter::Parameter::parse("TYPE: GPT\nCMDLINE:mtdparts=rk29xxnand:0x10@0x40(a),-@0x100(b:grow)\n").unwrap();
        let g = gpt::build(&p, 0x10000).unwrap();
        let entries = g.entries.clone();
        Plan { total_sectors: 0x10000, mode: Mode::Full, ops, gpt: Some(g), entries, notes: vec![] }
    }

    #[test]
    fn flatten_overrides_zero_with_data() {
        let plan = plan_with(vec![
            Op { step: "s".into(), sector: 100, sectors: 10, source: Source::Zero },
            Op { step: "s".into(), sector: 102, sectors: 3, source: Source::Fill([1, 2, 3, 4]) },
            Op { step: "s".into(), sector: 108, sectors: 4, source: Source::File { offset: 1000, len: 2048 } },
        ]);
        let f = plan.flattened();
        assert_eq!(f.len(), 4);
        assert_eq!((f[0].sector, f[0].sectors), (100, 2));
        assert_eq!(f[0].source, Source::Zero);
        assert_eq!((f[1].sector, f[1].sectors), (102, 3));
        assert_eq!((f[2].sector, f[2].sectors), (105, 3));
        assert_eq!((f[3].sector, f[3].sectors), (108, 4));
        assert_eq!(f[3].source, Source::File { offset: 1000, len: 2048 });
    }

    #[test]
    fn flatten_splits_file_sources() {
        let plan = plan_with(vec![
            Op { step: "s".into(), sector: 0, sectors: 8, source: Source::File { offset: 0, len: 4096 } },
            Op { step: "s".into(), sector: 2, sectors: 2, source: Source::Zero },
        ]);
        let f = plan.flattened();
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].source, Source::File { offset: 0, len: 1024 });
        assert_eq!(f[1].source, Source::Zero);
        assert_eq!(f[2].source, Source::File { offset: 2048, len: 2048 });
        assert_eq!((f[2].sector, f[2].sectors), (4, 4));
    }
}

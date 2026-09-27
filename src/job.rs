//! A complete "make a card" job: plan, write, verify. Shared by the CLI, the GUI and the
//! privileged helper process.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::plan;
use crate::rkfw::RkfwImage;
use crate::target;
use crate::writer::{self, Cancel, Progress};

#[derive(Debug, Clone)]
pub struct JobSpec {
    pub image: std::path::PathBuf,
    /// Device path or output file path.
    pub output: String,
    /// Card size in bytes; required for file outputs, ignored for devices.
    pub size: Option<u64>,
    pub verify: bool,
    /// xz preset for `.img.xz` outputs.
    pub xz_level: u32,
    /// Read every block back right after writing it and rewrite it on mismatch.
    pub verify_blocks: bool,
}

/// Runs the job, reporting progress through `progress`.
pub fn run(spec: &JobSpec, progress: &mut dyn FnMut(Progress), cancel: &Cancel) -> Result<plan::Plan> {
    let img = RkfwImage::open(&spec.image)?;
    let is_dev = crate::disks::is_block_device_path(&spec.output);
    let mut target: Box<dyn target::Target> = if is_dev {
        Box::new(crate::blockdev::BlockDevice::open_for_write(&spec.output)?)
    } else {
        let size = spec.size.ok_or_else(|| anyhow::anyhow!("a card size is required when writing to a file"))?;
        target::open_output(&spec.output, size, spec.xz_level)?
    };
    let total_sectors = target.size() / 512;
    if total_sectors == 0 {
        bail!("the target reports a size of zero");
    }
    let plan = plan::build(&img, total_sectors)?;
    let opts = writer::WriteOptions { verify_blocks: spec.verify_blocks, ..Default::default() };
    let ops = writer::write_plan(&plan, &img, target.as_mut(), opts, progress, cancel)?;
    if spec.verify {
        if target.sequential_only() {
            drop(target);
            let mut reader = target::open_readable(&spec.output)?;
            if reader.size() != total_sectors * 512 {
                bail!("the compressed image decodes to {} bytes, expected {}", reader.size(), total_sectors * 512);
            }
            writer::verify_target(&ops, &img, reader.as_mut(), progress, cancel)?;
        } else {
            // Verify through the same handle while the disk is still locked (reads bypass the
            // cache on every platform). The GPT is checked structurally rather than byte for
            // byte: hosts are free to rewrite header fields they consider inconsistent, and what
            // matters is that the table is valid and describes the same partitions.
            let data_ops: Vec<plan::Op> = ops.iter().filter(|o| o.step != "GPT").cloned().collect();
            writer::verify_target(&data_ops, &img, target.as_mut(), progress, cancel)?;
            let mut head = vec![0u8; 34 * 512];
            target.read_at(0, &mut head)?;
            check_gpt(&plan, &head, target.as_mut()).context("GPT verification failed")?;
        }
    }
    Ok(plan)
}

/// Verifies an existing card or image file against an RKFW image without writing anything.
pub fn verify_only(image: &Path, source: &str, progress: &mut dyn FnMut(Progress), cancel: &Cancel) -> Result<plan::Plan> {
    let img = RkfwImage::open(image)?;
    let mut t = target::open_readable(source)?;
    let total_sectors = t.size() / 512;
    let plan = plan::build(&img, total_sectors)?;
    // The GPT holds random GUIDs, so compare its structure separately and skip its bytes.
    let ops: Vec<plan::Op> = plan.flattened().into_iter().filter(|o| o.step != "GPT" && o.step != "Clear MBR").collect();
    writer::verify_target(&ops, &img, t.as_mut(), progress, cancel)?;
    // Structural GPT check.
    let mut head = vec![0u8; 34 * 512];
    t.read_at(0, &mut head)?;
    check_gpt(&plan, &head, t.as_mut())?;
    Ok(plan)
}

/// Checks that the GPT on `head` (sectors 0..34) matches the plan's partitions (names, ranges,
/// attributes), that the CRCs are valid and that the backup copy agrees.
pub fn check_gpt(plan: &plan::Plan, head: &[u8], t: &mut dyn target::Target) -> Result<()> {
    if head[0x1fe] != 0x55 || head[0x1ff] != 0xaa || head[0x1be + 4] != 0xee {
        bail!("no protective MBR");
    }
    let h = crate::gpt::parse_header(&head[512..1024])?;
    if !h.header_crc_ok {
        bail!("primary GPT header CRC is wrong");
    }
    let array = &head[1024..1024 + (h.entry_count * h.entry_size) as usize];
    if crc32fast::hash(array) != h.array_crc {
        bail!("primary GPT entry array CRC is wrong");
    }
    let entries = crate::gpt::parse_entries(array, h.entry_count, h.entry_size);
    if entries.len() != plan.gpt.entries.len() {
        bail!("GPT has {} partitions, expected {}", entries.len(), plan.gpt.entries.len());
    }
    for (a, b) in entries.iter().zip(plan.gpt.entries.iter()) {
        if a.name != b.name || a.first_lba != b.first_lba || a.last_lba != b.last_lba || a.attributes != b.attributes {
            bail!(
                "partition {} is {}..{} (attr {:#x}), expected {}..{} (attr {:#x})",
                a.name, a.first_lba, a.last_lba, a.attributes, b.first_lba, b.last_lba, b.attributes
            );
        }
    }
    let total = plan.total_sectors;
    let mut tail = vec![0u8; 33 * 512];
    t.read_at((total - 33) * 512, &mut tail)?;
    let bh = crate::gpt::parse_header(&tail[32 * 512..])?;
    if !bh.header_crc_ok {
        bail!("backup GPT header CRC is wrong");
    }
    if bh.my_lba != total - 1 || bh.alternate_lba != 1 || bh.disk_guid != h.disk_guid {
        bail!("backup GPT header does not match the primary");
    }
    if &tail[..32 * 512] != array {
        bail!("backup GPT entries differ from the primary");
    }
    Ok(())
}

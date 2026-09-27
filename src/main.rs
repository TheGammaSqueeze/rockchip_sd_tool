#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use rockchip_sd_tool::job::JobSpec;
use rockchip_sd_tool::rkfw::RkfwImage;
use rockchip_sd_tool::util::{human_bytes, parse_size};
use rockchip_sd_tool::writer::{Cancel, Progress};
use rockchip_sd_tool::{disks, elevate, gpt, plan};

mod gui;

#[derive(Parser)]
#[command(name = "rockchip_sd_tool", version, about = "Makes bootable SD cards from Rockchip RKFW firmware images")]
struct Cli {
    /// Firmware image to preload in the graphical interface.
    image: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Shows what an RKFW image contains and the card layout it produces.
    Info {
        image: PathBuf,
        /// Card size to plan for (e.g. 32G, 128GB, 250347520s); default 16 GiB.
        #[arg(long)]
        size: Option<String>,
        /// Also check the MD5 stored at the end of the image (reads the whole file).
        #[arg(long)]
        md5: bool,
        /// Print the parameter file.
        #[arg(long)]
        parameter: bool,
    },
    /// Lists the disks a card can be written to.
    List {
        /// Include internal and system disks.
        #[arg(long)]
        all: bool,
    },
    /// Writes an image to a card, a raw .img file or a compressed .img.xz file.
    Write {
        image: PathBuf,
        /// Device path (/dev/sdX, /dev/rdiskN, \\.\PhysicalDriveN) or output file (.img, .img.xz).
        #[arg(long)]
        to: String,
        /// Card size for file outputs (e.g. 32G, 64GB, 250347520s).
        #[arg(long)]
        size: Option<String>,
        /// Skip the full read-back verification after the write.
        #[arg(long)]
        no_verify: bool,
        /// Skip the per-block read-back (every block is normally flushed, read back and compared
        /// right after it is written, and rewritten up to 3 times on mismatch).
        #[arg(long)]
        no_block_verify: bool,
        /// Do not ask for confirmation before writing to a device.
        #[arg(short, long)]
        yes: bool,
        /// xz compression preset (0-9) for .img.xz outputs.
        #[arg(long, default_value_t = 3)]
        xz_level: u32,
        /// Write JSON progress lines to this file (used by the GUI helper).
        #[arg(long, hide = true)]
        progress_file: Option<PathBuf>,
    },
    /// Verifies that a card or a raw .img file holds the given image.
    Verify {
        image: PathBuf,
        /// Device path or raw .img file.
        #[arg(long)]
        from: String,
    },
}

fn main() {
    #[cfg(windows)]
    attach_console();
    let cli = Cli::parse();
    let code = match cli.cmd {
        None => match gui::run(cli.image) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e:#}");
                1
            }
        },
        Some(cmd) => match run_cmd(cmd) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e:#}");
                1
            }
        },
    };
    std::process::exit(code);
}

#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    if std::env::args().len() > 1 {
        unsafe {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn print_image(img: &RkfwImage) {
    let h = &img.header;
    println!("Image:        {}", img.path.display());
    println!("Size:         {} ({} bytes)", human_bytes(img.file_size), img.file_size);
    println!("Chip:         {}", rockchip_sd_tool::rkfw::chip_name(h.chip));
    println!("Version:      {}   built {}", rockchip_sd_tool::rkfw::format_version(h.version), h.time);
    println!("Model:        {}  (id {}, {})", img.af.model, img.af.id, img.af.manufacturer);
    println!("Parameter:    {} {} / {}", img.parameter.machine_model, img.parameter.firmware_ver, img.parameter.machine);
    println!("Layout type:  {}", img.parameter.part_type);
    println!("MD5 stored:   {}", img.md5_hex.as_deref().unwrap_or("(none)"));
    println!();
    println!("Loader ({}): {} entries, rc4 flag {}, released {}",
        String::from_utf8_lossy(&img.boot.magic).trim(),
        img.boot.entries_471.len() + img.boot.entries_472.len() + img.boot.entries_loader.len(),
        img.boot.rc4_flag,
        img.boot.time);
    for e in img.boot.entries_471.iter().chain(&img.boot.entries_472).chain(&img.boot.entries_loader) {
        println!("  {:<24} {:>10} bytes", e.name, e.size);
    }
    println!();
    println!("Firmware items:");
    for it in &img.af.items {
        let addr = if it.nand_addr == 0xffff_ffff { "-".to_string() } else { format!("0x{:x}", it.nand_addr) };
        println!("  {:<16} {:<28} {:>12} bytes  sector {}", it.name, it.file_name, it.size, addr);
    }
    println!();
    println!("Partitions (parameter):");
    for p in &img.parameter.partitions {
        match p.size {
            Some(s) => println!("  {:<16} sector {:>10}  size {:>10} ({})", p.name, p.offset, s, human_bytes(s * 512)),
            None => println!("  {:<16} sector {:>10}  grows to the end of the card", p.name, p.offset),
        }
    }
    let min = gpt::minimum_sectors(&img.parameter) * 512;
    println!();
    println!("Minimum card size: {} ({} bytes)", human_bytes(min), min);
}

fn run_cmd(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Info { image, size, md5, parameter } => {
            let img = RkfwImage::open(&image)?;
            print_image(&img);
            match img.check_loader_crc() {
                Ok(true) => println!("Loader CRC:   ok"),
                Ok(false) => println!("Loader CRC:   MISMATCH (the loader blob is damaged)"),
                Err(e) => println!("Loader CRC:   not checked ({e})"),
            }
            if parameter {
                println!("\n--- parameter ---\n{}", img.parameter_text);
            }
            let total = match size {
                Some(s) => parse_size(&s).ok_or_else(|| anyhow::anyhow!("bad size '{s}'"))?,
                None => 16 << 30,
            } / 512;
            match plan::build(&img, total) {
                Ok(p) => {
                    println!("\nPlan for a {} card ({} sectors): {} written",
                        human_bytes(total * 512), total, human_bytes(p.total_bytes()));
                    for n in &p.notes {
                        println!("  {n}");
                    }
                    println!("  GPT: {} partitions, last usable sector {}", p.gpt.entries.len(), gpt::last_usable_lba(total));
                }
                Err(e) => println!("\nPlan for a {} card: {e}", human_bytes(total * 512)),
            }
            if md5 {
                print!("Checking MD5...");
                std::io::stdout().flush().ok();
                match img.check_md5(|_, _| {})? {
                    Some(true) => println!(" ok"),
                    Some(false) => println!(" MISMATCH: the image file is corrupt or incomplete"),
                    None => println!(" no digest stored"),
                }
            }
        }
        Cmd::List { all } => {
            let list = disks::list();
            let mut shown = 0;
            for d in list.iter().filter(|d| all || d.is_candidate()) {
                shown += 1;
                println!(
                    "{:<24} {:>10}  {:<5} {}{}{}",
                    d.path,
                    human_bytes(d.size),
                    d.bus,
                    d.model,
                    if d.system { "  [SYSTEM DISK]" } else { "" },
                    if d.mounts.is_empty() { String::new() } else { format!("  mounted: {}", d.mounts.join(", ")) }
                );
            }
            if shown == 0 {
                println!("no removable disks found{}", if all { "" } else { " (use --all to list every disk)" });
            }
        }
        Cmd::Write { image, to, size, no_verify, no_block_verify, yes, xz_level, progress_file } => {
            let is_dev = disks::is_block_device_path(&to);
            let size_bytes = match &size {
                Some(s) => Some(parse_size(s).ok_or_else(|| anyhow::anyhow!("bad size '{s}'"))?),
                None => None,
            };
            if !is_dev && size_bytes.is_none() {
                bail!("--size is required when writing to a file (the size of the SD card the image is for)");
            }
            if is_dev {
                if !elevate::is_privileged() {
                    bail!("writing to {to} needs administrator rights (run with sudo)");
                }
                if let Some(d) = disks::find(&to) {
                    if d.system {
                        bail!("{to} is the system disk; refusing to write to it");
                    }
                    if !yes {
                        eprintln!("About to ERASE {} ({}, {}) and write {}.", d.path, d.model, human_bytes(d.size), image.display());
                    }
                } else if !yes {
                    eprintln!("About to ERASE {} and write {}.", to, image.display());
                }
                if !yes {
                    eprint!("Type 'yes' to continue: ");
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s)?;
                    if s.trim() != "yes" {
                        bail!("aborted");
                    }
                }
            }
            let spec = JobSpec { image, output: to, size: size_bytes, verify: !no_verify, xz_level, verify_blocks: !no_block_verify };
            let cancel = Cancel::new();
            let mut pf = match &progress_file {
                Some(p) => Some(std::fs::File::create(p).with_context(|| format!("cannot create {}", p.display()))?),
                None => None,
            };
            let cancel_marker = progress_file.as_ref().map(|p| elevate::cancel_path(p));
            let mut last = std::time::Instant::now();
            let mut last_step = String::new();
            let mut progress = |p: Progress| {
                if let Some(cm) = &cancel_marker {
                    if cm.exists() {
                        cancel.0.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                let now = std::time::Instant::now();
                if now.duration_since(last).as_millis() < 100 && p.done != p.total && p.step == last_step {
                    return;
                }
                last = now;
                last_step = p.step.clone();
                if let Some(f) = pf.as_mut() {
                    let _ = writeln!(f, "{}", serde_json::to_string(&p).unwrap());
                    let _ = f.flush();
                } else {
                    let pct = if p.total > 0 { p.done as f64 * 100.0 / p.total as f64 } else { 0.0 };
                    let r = if p.retries > 0 { format!("  ({} block retries)", p.retries) } else { String::new() };
                    eprint!("\r{:<7} {:<16} {:>6.1}%  {}{}      ", p.phase, p.step, pct, human_bytes(p.done), r);
                }
            };
            let res = rockchip_sd_tool::job::run(&spec, &mut progress, &cancel);
            match res {
                Ok(_) => {
                    if let Some(f) = pf.as_mut() {
                        let _ = writeln!(f, "{{\"result\":\"ok\"}}");
                    } else {
                        eprintln!("\ndone{}", if spec.verify { ", verified" } else { "" });
                    }
                }
                Err(e) => {
                    if let Some(f) = pf.as_mut() {
                        let _ = writeln!(f, "{}", serde_json::json!({ "error": format!("{e:#}") }));
                    } else {
                        eprintln!();
                    }
                    return Err(e);
                }
            }
        }
        Cmd::Verify { image, from } => {
            let cancel = Cancel::new();
            let mut progress = |p: Progress| {
                let pct = if p.total > 0 { p.done as f64 * 100.0 / p.total as f64 } else { 0.0 };
                eprint!("\r{:<7} {:<16} {:>6.1}%", p.phase, p.step, pct);
            };
            rockchip_sd_tool::job::verify_only(&image, &from, &mut progress, &cancel)?;
            eprintln!("\nverified: {from} holds {}", image.display());
        }
    }
    Ok(())
}

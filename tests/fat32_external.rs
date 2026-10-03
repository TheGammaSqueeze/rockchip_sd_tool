//! Checks the FAT32 writer against implementations that did not write it: `fsck.vfat` from
//! dosfstools and `mdir`/`mcopy` from mtools. Both are skipped when not installed, so the suite
//! still runs anywhere, but on a machine that has them this is the real proof that the filesystem
//! is well formed and that a reader finds the files with the right names and contents.

use std::io::{Seek, SeekFrom, Write};
use std::process::Command;

use rockchip_sd_tool::fat32::{self, FatFile};
use rockchip_sd_tool::plan::Source;

fn have(tool: &str) -> bool {
    Command::new("sh").arg("-c").arg(format!("command -v {tool}")).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Writes the pieces into a file of `sectors` sectors; everything not covered stays zero.
fn materialise(path: &std::path::Path, sectors: u64, pieces: &[fat32::Piece], payload: &[u8]) {
    let mut f = std::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(path).unwrap();
    f.set_len(sectors * 512).unwrap();
    for p in pieces {
        f.seek(SeekFrom::Start(p.offset)).unwrap();
        match &p.source {
            Source::Bytes(b) => {
                let n = std::cmp::min(b.len() as u64, p.len) as usize;
                f.write_all(&b[..n]).unwrap();
            }
            Source::File { offset, len } => {
                let s = *offset as usize;
                let e = s + *len as usize;
                f.write_all(&payload[s..e]).unwrap();
            }
            Source::Fill(pat) => {
                let buf: Vec<u8> = pat.iter().cycle().take(p.len as usize).copied().collect();
                f.write_all(&buf).unwrap();
            }
            Source::Zero => {}
        }
    }
    f.sync_all().unwrap();
}

#[test]
fn the_filesystem_passes_fsck_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("fat.img");
    let sectors = 1024 * 1024 * 1024 / 512; // 1 GiB
    let payload: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let config = b"#rockchip sdcard boot config file for factory\nfw_update = 1\n".to_vec();

    let files = vec![
        FatFile { name: "sdupdate.img".into(), len: payload.len() as u64, source: Source::File { offset: 0, len: payload.len() as u64 } },
        FatFile { name: "rksdfw.tag".into(), len: 4, source: Source::Bytes(b"RKFW".to_vec().into()) },
        FatFile { name: "sd_boot_config.config".into(), len: config.len() as u64, source: Source::Bytes(config.clone().into()) },
    ];
    let (_g, pieces) = fat32::build(sectors, "UPGRADE", files, 0xdead_beef).unwrap();
    materialise(&img, sectors, &pieces, &payload);

    if have("fsck.vfat") {
        let out = Command::new("fsck.vfat").arg("-n").arg(&img).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "fsck.vfat rejected the filesystem:\n{text}");
        assert!(!text.to_lowercase().contains("dirty"), "fsck.vfat reported problems:\n{text}");
    }

    if have("mdir") {
        let listing = Command::new("mdir")
            .env("MTOOLS_SKIP_CHECK", "1")
            .arg("-i")
            .arg(&img)
            .arg("::")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&listing.stdout).to_string();
        assert!(listing.status.success(), "mdir failed: {text}");
        // Every name must come back exactly as asked for, lower case included, not just as an
        // upper-case 8.3 alias.
        for name in ["sdupdate.img", "rksdfw.tag", "sd_boot_config.config"] {
            assert!(text.contains(name), "{name} missing from:\n{text}");
        }

        // And the contents must come back byte for byte.
        let got = dir.path().join("out.img");
        let st = Command::new("mcopy")
            .env("MTOOLS_SKIP_CHECK", "1")
            .args(["-i", img.to_str().unwrap(), "::sdupdate.img", got.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(st.success(), "mcopy could not read the payload back");
        assert_eq!(std::fs::read(&got).unwrap(), payload, "the payload read back differently");

        let got_cfg = dir.path().join("out.cfg");
        let st = Command::new("mcopy")
            .env("MTOOLS_SKIP_CHECK", "1")
            .args(["-i", img.to_str().unwrap(), "::sd_boot_config.config", got_cfg.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(st.success(), "mcopy could not read the config back by its long name");
        assert_eq!(std::fs::read(&got_cfg).unwrap(), config);
    }
}

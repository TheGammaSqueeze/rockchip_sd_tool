//! End-to-end tests: build a small synthetic RKFW image, write it to a raw .img and to a .img.xz,
//! then read the results back and check every structure the boot ROM, U-Boot and the kernel rely
//! on (loader placement and descrambling, partition placement, sparse expansion, GPT).

use std::path::PathBuf;

use rockchip_sd_tool::job::{self, JobSpec};
use rockchip_sd_tool::rkfw::RkfwImage;
use rockchip_sd_tool::sparse;
use rockchip_sd_tool::writer::Cancel;
use rockchip_sd_tool::{gpt, plan, rc4};

const PARAMETER: &str = "FIRMWARE_VER: 1.0\nMACHINE_MODEL: Test Board\nMACHINE_ID: 007\nMANUFACTURER: Test\nMAGIC: 0x5041524B\nATAG: 0x00200800\nMACHINE: rk3568\nTYPE: GPT\nCMDLINE:mtdparts=rk29xxnand:0x00000400@0x00002000(uboot),0x00000100@0x00002400(misc),0x00000800@0x00002500(boot:bootable),0x00001000@0x00002d00(super),-@0x00003d00(userdata:grow)\n";

fn scramble(mut d: Vec<u8>) -> Vec<u8> {
    rc4::rc4_sectors(&mut d);
    d
}

fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len).map(|i| ((i as u32).wrapping_mul(2654435761).wrapping_add(seed) >> 13) as u8).collect()
}

/// A loader blob: BOOT header, three loader entries (RC4 scrambled), Rockchip CRC32 at the end.
fn build_loader(head: &[u8], data: &[u8], boot: &[u8]) -> Vec<u8> {
    let entries = [("FlashHead", head), ("FlashData", data), ("FlashBoot", boot)];
    let esize = 57usize;
    let table_off = 0x66usize;
    let mut data_off = table_off + entries.len() * esize;
    let mut blob = vec![0u8; table_off];
    blob[0..4].copy_from_slice(b"LDR ");
    blob[4..6].copy_from_slice(&0x66u16.to_le_bytes());
    blob[6..10].copy_from_slice(&0x0101u32.to_le_bytes());
    blob[14..16].copy_from_slice(&2026u16.to_le_bytes());
    blob[21..25].copy_from_slice(&0x33353638u32.to_le_bytes());
    // 471 and 472 groups empty, loader group with three entries.
    let mut o = 25;
    for (count, off) in [(0u8, 0u32), (0, 0), (entries.len() as u8, table_off as u32)] {
        blob[o] = count;
        blob[o + 1..o + 5].copy_from_slice(&off.to_le_bytes());
        blob[o + 5] = esize as u8;
        o += 6;
    }
    blob[o] = 0; // sign flag
    blob[o + 1] = 1; // rc4 flag: the ROM wants plain data
    let mut table = Vec::new();
    let mut payload = Vec::new();
    for (name, d) in entries {
        let mut e = vec![0u8; esize];
        e[0] = esize as u8;
        e[1..5].copy_from_slice(&4u32.to_le_bytes());
        for (k, u) in name.encode_utf16().enumerate() {
            e[5 + k * 2..7 + k * 2].copy_from_slice(&u.to_le_bytes());
        }
        e[45..49].copy_from_slice(&(data_off as u32).to_le_bytes());
        e[49..53].copy_from_slice(&(d.len() as u32).to_le_bytes());
        table.extend_from_slice(&e);
        payload.extend_from_slice(&scramble(d.to_vec()));
        data_off += d.len();
    }
    blob.extend_from_slice(&table);
    blob.extend_from_slice(&payload);
    let crc = rockchip_sd_tool::rkcrc::crc32_rk(&blob);
    blob.extend_from_slice(&crc.to_le_bytes());
    blob
}

/// An RKAF container with the given items (name, nand_size, nand_addr, data).
fn build_af(items: &[(&str, u32, u32, Vec<u8>)]) -> Vec<u8> {
    let hdr_len = 0x8c + items.len() * 0x70;
    let mut pos = (hdr_len + 0x7ff) & !0x7ff;
    let mut h = vec![0u8; hdr_len];
    h[0..4].copy_from_slice(b"RKAF");
    h[8..18].copy_from_slice(b"Test Board");
    h[0x2a..0x2d].copy_from_slice(b"007");
    h[0x48..0x4c].copy_from_slice(b"Test");
    h[0x84..0x88].copy_from_slice(&0x01000000u32.to_le_bytes());
    h[0x88..0x8c].copy_from_slice(&(items.len() as u32).to_le_bytes());
    let mut offsets = Vec::new();
    for (i, (name, nand_size, nand_addr, d)) in items.iter().enumerate() {
        let e = 0x8c + i * 0x70;
        h[e..e + name.len()].copy_from_slice(name.as_bytes());
        let fname = format!("Image/{name}.img");
        h[e + 32..e + 32 + fname.len()].copy_from_slice(fname.as_bytes());
        let padded = (d.len() + 0x7ff) & !0x7ff;
        h[e + 92..e + 96].copy_from_slice(&nand_size.to_le_bytes());
        h[e + 96..e + 100].copy_from_slice(&(pos as u32).to_le_bytes());
        h[e + 100..e + 104].copy_from_slice(&nand_addr.to_le_bytes());
        h[e + 104..e + 108].copy_from_slice(&(padded as u32).to_le_bytes());
        h[e + 108..e + 112].copy_from_slice(&(d.len() as u32).to_le_bytes());
        offsets.push(pos);
        pos += padded;
    }
    let mut out = h;
    out.resize(offsets[0], 0);
    for (i, (_, _, _, d)) in items.iter().enumerate() {
        out.resize(offsets[i], 0);
        out.extend_from_slice(d);
    }
    out.resize(pos, 0);
    let len = out.len() as u32;
    out[4..8].copy_from_slice(&len.to_le_bytes());
    out
}

struct Fixture {
    dir: tempfile::TempDir,
    image: PathBuf,
    head: Vec<u8>,
    data: Vec<u8>,
    boot: Vec<u8>,
    uboot: Vec<u8>,
    misc: Vec<u8>,
    boot_img: Vec<u8>,
    super_sparse: Vec<u8>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut head = vec![0u8; 2048];
    head[0..4].copy_from_slice(b"RKNS");
    head[8..12].copy_from_slice(&0x00020180u32.to_le_bytes());
    let data = pattern(59392, 1); // 116 sectors, a multiple of 4
    let boot = pattern(258048 - 700, 2); // not sector aligned: exercises padding and the
                                          // plain trailing partial sector rule
    let uboot = pattern(0x400 * 512, 3);
    let misc = pattern(3000, 4);
    let boot_img = pattern(0x800 * 512 - 100, 5);
    // super: sparse, 4 KiB blocks, 0x1000 sectors partition = 512 blocks:
    // 3 raw + 100 dont care + 5 fill + 404 dont care.
    let raw = pattern(3 * 4096, 6);
    let super_sparse = sparse::build_sparse(
        4096,
        &[
            (sparse::CHUNK_RAW, 3, raw.clone()),
            (sparse::CHUNK_DONT_CARE, 100, vec![]),
            (sparse::CHUNK_FILL, 5, vec![0xde, 0xad, 0xbe, 0xef]),
            (sparse::CHUNK_DONT_CARE, 404, vec![]),
            (sparse::CHUNK_CRC32, 0, vec![0, 0, 0, 0]),
        ],
    );
    let loader = build_loader(&head, &data, &boot);
    let af = build_af(&[
        ("package-file", 0, 0xffff_ffff, b"# NAME Relative path\n".to_vec()),
        ("bootloader", 0, 0xffff_ffff, loader.clone()),
        ("parameter", 0x2000, 0, PARAMETER.as_bytes().to_vec()),
        ("uboot", 0x400, 0x2000, uboot.clone()),
        ("misc", 0x100, 0x2400, misc.clone()),
        ("boot", 0x800, 0x2500, boot_img.clone()),
        ("super", 0x1000, 0x2d00, super_sparse.clone()),
        ("backup", 0, 0xffff_ffff, vec![]),
    ]);
    // RKFW header.
    let mut rkfw = vec![0u8; 0x66];
    rkfw[0..4].copy_from_slice(b"RKFW");
    rkfw[4..6].copy_from_slice(&0x66u16.to_le_bytes());
    rkfw[6..10].copy_from_slice(&0x0e000000u32.to_le_bytes());
    rkfw[21..25].copy_from_slice(&0x33353638u32.to_le_bytes());
    rkfw[25..29].copy_from_slice(&0x66u32.to_le_bytes());
    rkfw[29..33].copy_from_slice(&(loader.len() as u32).to_le_bytes());
    let img_off = 0x66 + loader.len();
    rkfw[33..37].copy_from_slice(&(img_off as u32).to_le_bytes());
    rkfw[37..41].copy_from_slice(&(af.len() as u32).to_le_bytes());
    let mut file = rkfw;
    file.extend_from_slice(&loader);
    file.extend_from_slice(&af);
    let mut md5 = rockchip_sd_tool::md5::Md5::new();
    md5.update(&file);
    file.extend_from_slice(md5.finish_hex().as_bytes());
    let image = dir.path().join("test.img");
    std::fs::write(&image, &file).unwrap();
    Fixture { dir, image, head, data, boot, uboot, misc, boot_img, super_sparse }
}

fn read_sectors(r: &mut dyn rockchip_sd_tool::target::Target, sector: u64, count: u64) -> Vec<u8> {
    let mut v = vec![0u8; (count * 512) as usize];
    r.read_at(sector * 512, &mut v).unwrap();
    v
}

fn padded(d: &[u8], to: usize) -> Vec<u8> {
    let mut v = d.to_vec();
    v.resize((v.len() + to - 1) / to * to, 0);
    v
}

fn check_card(path: &PathBuf, total_sectors: u64, fx: &Fixture) {
    let mut r = rockchip_sd_tool::target::open_readable(&path.to_string_lossy()).unwrap();
    assert_eq!(r.size(), total_sectors * 512);
    let r = r.as_mut();
    // Protective MBR + primary GPT.
    let head = read_sectors(r, 0, 34);
    assert_eq!(&head[0x1fe..0x200], &[0x55, 0xaa]);
    assert_eq!(head[0x1be + 4], 0xee);
    let h = gpt::parse_header(&head[512..1024]).unwrap();
    assert!(h.header_crc_ok, "primary header crc");
    assert_eq!(h.my_lba, 1);
    assert_eq!(h.alternate_lba, total_sectors - 1);
    assert_eq!(h.first_usable, 34);
    assert_eq!(h.last_usable, gpt::last_usable_lba(total_sectors));
    assert_eq!(h.entries_lba, 2);
    assert_eq!((h.entry_count, h.entry_size), (128, 128));
    let array = &head[1024..];
    assert_eq!(crc32fast::hash(array), h.array_crc);
    let entries = gpt::parse_entries(array, 128, 128);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["uboot", "misc", "boot", "super", "userdata"]);
    assert_eq!((entries[0].first_lba, entries[0].last_lba), (0x2000, 0x23ff));
    assert_eq!(entries[2].attributes, gpt::ATTR_BOOTABLE);
    assert_eq!(entries[4].first_lba, 0x3d00);
    assert_eq!(entries[4].last_lba, gpt::grow_end(total_sectors) - 1);
    for e in &entries {
        assert_eq!(e.unique_guid[7] >> 4, 4, "uuid version");
        assert_eq!(e.unique_guid[8] & 0xc0, 0x80, "uuid variant");
    }
    // Loader at 64: header plain, DDR at 68, boot at 184 with the trailing partial sector plain
    // and zero padding to four sectors.
    assert_eq!(read_sectors(r, 64, 4), fx.head);
    assert_eq!(read_sectors(r, 68, 116), fx.data);
    let boot_on_card = read_sectors(r, 184, 504);
    assert_eq!(&boot_on_card[..fx.boot.len()], &fx.boot[..]);
    assert!(boot_on_card[fx.boot.len()..].iter().all(|&b| b == 0));
    // Partitions.
    assert_eq!(read_sectors(r, 0x2000, 0x400), fx.uboot);
    assert_eq!(read_sectors(r, 0x2400, 6), padded(&fx.misc, 512));
    assert_eq!(read_sectors(r, 0x2500, 0x800), padded(&fx.boot_img, 512));
    // Sparse super: raw, zeros, fill, zeros.
    let hdr = sparse::SparseHeader::parse(&fx.super_sparse).unwrap();
    assert_eq!(hdr.expanded_size(), 512 * 4096);
    let sup = read_sectors(r, 0x2d00, 0x1000);
    assert_eq!(&sup[..3 * 4096], &pattern(3 * 4096, 6)[..]);
    assert!(sup[3 * 4096..103 * 4096].iter().all(|&b| b == 0));
    let fill: Vec<u8> = [0xde, 0xad, 0xbe, 0xef].iter().cycle().take(5 * 4096).copied().collect();
    assert_eq!(&sup[103 * 4096..108 * 4096], &fill[..]);
    assert!(sup[108 * 4096..].iter().all(|&b| b == 0));
    // Backup GPT at the end.
    let tail = read_sectors(r, total_sectors - 33, 33);
    assert_eq!(&tail[..32 * 512], array);
    let bh = gpt::parse_header(&tail[32 * 512..]).unwrap();
    assert!(bh.header_crc_ok, "backup header crc");
    assert_eq!(bh.my_lba, total_sectors - 1);
    assert_eq!(bh.alternate_lba, 1);
    assert_eq!(bh.entries_lba, total_sectors - 33);
    assert_eq!(bh.disk_guid, h.disk_guid);
    assert_eq!(bh.array_crc, h.array_crc);
}

#[test]
fn parses_synthetic_image() {
    let fx = fixture();
    let img = RkfwImage::open(&fx.image).unwrap();
    assert_eq!(img.af.model, "Test Board");
    assert_eq!(img.boot.rc4_flag, 1);
    assert_eq!(img.boot.entries_loader.len(), 3);
    assert_eq!(img.parameter.partitions.len(), 5);
    assert!(img.check_loader_crc().unwrap());
    assert_eq!(img.check_md5(|_, _| {}).unwrap(), Some(true));
    let p = plan::build(&img, 0x20000).unwrap();
    assert_eq!(p.gpt.entries.len(), 5);
    let steps: Vec<&str> = p.ops.iter().map(|o| o.step.as_str()).collect();
    assert!(steps.starts_with(&["Clear MBR", "Loader", "Loader", "Loader", "uboot", "misc", "boot", "super"]));
    assert_eq!(&steps[steps.len() - 2..], &["GPT", "GPT"]);
}

#[test]
fn raw_image_roundtrip() {
    let fx = fixture();
    let total_sectors = 0x20000u64; // 64 MiB card
    let out = fx.dir.path().join("card.img");
    let spec = JobSpec { image: fx.image.clone(), output: out.to_string_lossy().to_string(), size: Some(total_sectors * 512), verify: true, xz_level: 1, verify_blocks: true };
    let mut last = None;
    job::run(&spec, &mut |p| last = Some(p), &Cancel::new()).unwrap();
    assert_eq!(last.unwrap().phase, "verify");
    assert_eq!(std::fs::metadata(&out).unwrap().len(), total_sectors * 512);
    check_card(&out, total_sectors, &fx);
    // The independent verify command agrees.
    job::verify_only(&fx.image, &out.to_string_lossy(), &mut |_| {}, &Cancel::new()).unwrap();
}

#[test]
fn xz_image_roundtrip() {
    let fx = fixture();
    let total_sectors = 0x800000u64 + 0x1000; // just over 4 GiB: 64-sector tail reserve
    let out = fx.dir.path().join("card.img.xz");
    let spec = JobSpec { image: fx.image.clone(), output: out.to_string_lossy().to_string(), size: Some(total_sectors * 512), verify: true, xz_level: 0, verify_blocks: true };
    job::run(&spec, &mut |_| {}, &Cancel::new()).unwrap();
    assert!(std::fs::metadata(&out).unwrap().len() < 4 << 20, "zero areas must compress away");
    assert_eq!(gpt::grow_end(total_sectors), total_sectors - 64);
    check_card(&out, total_sectors, &fx);
    // The verify command reads compressed images too.
    job::verify_only(&fx.image, &out.to_string_lossy(), &mut |_| {}, &Cancel::new()).unwrap();
    // A stock xz decoder accepts the file (a normal single-stream xz file).
    if let Ok(st) = std::process::Command::new("xz").arg("-t").arg(&out).status() {
        assert!(st.success(), "xz -t rejects the image");
    }
}

#[test]
fn rejects_too_small_card() {
    let fx = fixture();
    let out = fx.dir.path().join("small.img");
    let spec = JobSpec { image: fx.image.clone(), output: out.to_string_lossy().to_string(), size: Some(0x3d00 * 512), verify: false, xz_level: 1, verify_blocks: true };
    let err = job::run(&spec, &mut |_| {}, &Cancel::new()).unwrap_err();
    assert!(format!("{err:#}").contains("needs at least"), "{err:#}");
}

#[test]
fn cancel_stops_the_write() {
    let fx = fixture();
    let out = fx.dir.path().join("cancel.img");
    let spec = JobSpec { image: fx.image.clone(), output: out.to_string_lossy().to_string(), size: Some(0x20000 * 512), verify: true, xz_level: 1, verify_blocks: true };
    let cancel = Cancel::new();
    let flag = cancel.0.clone();
    let err = job::run(&spec, &mut |p| if p.step == "boot" { flag.store(true, std::sync::atomic::Ordering::Relaxed) }, &cancel).unwrap_err();
    assert!(format!("{err:#}").contains("cancelled"));
}

/// A target that silently corrupts what is written, either once per offset (flaky) or always.
struct FaultyTarget {
    inner: rockchip_sd_tool::target::FileTarget,
    corrupt_sector: u64,
    always: bool,
    hits: u32,
}

impl rockchip_sd_tool::target::Target for FaultyTarget {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> anyhow::Result<()> {
        let end = off + buf.len() as u64;
        let target_off = self.corrupt_sector * 512;
        if off <= target_off && target_off < end && (self.always || self.hits == 0) {
            self.hits += 1;
            let mut bad = buf.to_vec();
            let i = (target_off - off) as usize;
            bad[i] ^= 0xff;
            return self.inner.write_at(off, &bad);
        }
        self.inner.write_at(off, buf)
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.inner.read_at(off, buf)
    }
    fn zero_by_default(&self) -> bool {
        true
    }
    fn flush(&mut self) -> anyhow::Result<()> {
        self.inner.flush()
    }
    fn description(&self) -> String {
        "faulty".into()
    }
}

#[test]
fn flaky_block_is_rewritten() {
    use rockchip_sd_tool::writer::{self, WriteOptions};
    let fx = fixture();
    let total_sectors = 0x20000u64;
    let out = fx.dir.path().join("flaky.img");
    let img = RkfwImage::open(&fx.image).unwrap();
    let p = plan::build(&img, total_sectors).unwrap();
    let mut t = FaultyTarget {
        inner: rockchip_sd_tool::target::FileTarget::create(&out, total_sectors * 512).unwrap(),
        corrupt_sector: 0x2500 + 7, // inside boot
        always: false,
        hits: 0,
    };
    let opts = WriteOptions { retry_delay: std::time::Duration::from_millis(1), ..Default::default() };
    let mut last_retries = 0;
    let ops = writer::write_plan(&p, &img, &mut t, opts, &mut |pr| last_retries = pr.retries, &Cancel::new()).unwrap();
    assert_eq!(t.hits, 1);
    assert_eq!(last_retries, 1, "one block must have been rewritten");
    // The card ends up correct anyway.
    writer::verify_target(&ops, &img, &mut t, &mut |_| {}, &Cancel::new()).unwrap();
    check_card(&out, total_sectors, &fx);
}

#[test]
fn permanently_bad_block_fails_after_three_attempts() {
    use rockchip_sd_tool::writer::{self, WriteOptions};
    let fx = fixture();
    let total_sectors = 0x20000u64;
    let out = fx.dir.path().join("bad.img");
    let img = RkfwImage::open(&fx.image).unwrap();
    let p = plan::build(&img, total_sectors).unwrap();
    let mut t = FaultyTarget {
        inner: rockchip_sd_tool::target::FileTarget::create(&out, total_sectors * 512).unwrap(),
        corrupt_sector: 0x2000 + 3, // inside uboot
        always: true,
        hits: 0,
    };
    let opts = WriteOptions { retry_delay: std::time::Duration::from_millis(1), ..Default::default() };
    let err = writer::write_plan(&p, &img, &mut t, opts, &mut |_| {}, &Cancel::new()).unwrap_err();
    assert_eq!(t.hits, 3, "three attempts");
    let msg = format!("{err:#}");
    assert!(msg.contains("failed 3 times") && msg.contains("uboot"), "{msg}");
}

#[test]
fn block_verification_can_be_disabled() {
    use rockchip_sd_tool::writer::{self, WriteOptions};
    let fx = fixture();
    let total_sectors = 0x20000u64;
    let out = fx.dir.path().join("noverify.img");
    let img = RkfwImage::open(&fx.image).unwrap();
    let p = plan::build(&img, total_sectors).unwrap();
    let mut t = FaultyTarget {
        inner: rockchip_sd_tool::target::FileTarget::create(&out, total_sectors * 512).unwrap(),
        corrupt_sector: 0x2000 + 3,
        always: true,
        hits: 0,
    };
    let opts = WriteOptions { verify_blocks: false, ..Default::default() };
    let ops = writer::write_plan(&p, &img, &mut t, opts, &mut |_| {}, &Cancel::new()).unwrap();
    assert_eq!(t.hits, 1);
    // The final verification pass catches the corruption instead.
    let err = writer::verify_target(&ops, &img, &mut t, &mut |_| {}, &Cancel::new()).unwrap_err();
    assert!(format!("{err:#}").contains("verification failed in uboot"));
}

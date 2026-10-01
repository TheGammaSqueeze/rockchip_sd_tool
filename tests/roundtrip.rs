//! End-to-end tests: build a small synthetic RKFW image, write it to a raw .img and to a .img.xz,
//! then read the results back and check every structure the boot ROM, U-Boot and the kernel rely
//! on (loader placement and descrambling, partition placement, sparse expansion, GPT).

use std::path::PathBuf;

use rockchip_sd_tool::job::{self, JobSpec};
use rockchip_sd_tool::rkfw::RkfwImage;
use rockchip_sd_tool::sparse;
use rockchip_sd_tool::writer::Cancel;
use rockchip_sd_tool::{gpt, plan, rc4};

/// A second layout: boot is half the size and everything after it moves, so a card made from one
/// is not a valid upgrade target for the other.
const ALT_PARAMETER: &str = "FIRMWARE_VER: 1.0\nMACHINE_MODEL: Test Board\nMACHINE_ID: 007\nMANUFACTURER: Test\nMAGIC: 0x5041524B\nATAG: 0x00200800\nMACHINE: rk3568\nTYPE: GPT\nCMDLINE:mtdparts=rk29xxnand:0x00000400@0x00002000(uboot),0x00000100@0x00002400(misc),0x00000400@0x00002500(boot:bootable),0x00001000@0x00002900(super),-@0x00003900(userdata:grow)\n";

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
    fixture_param(PARAMETER)
}

fn fixture_param(parameter: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut head = vec![0u8; 2048];
    head[0..4].copy_from_slice(b"RKNS");
    head[8..12].copy_from_slice(&0x00020180u32.to_le_bytes());
    let data = pattern(59392, 1); // 116 sectors, a multiple of 4
    let boot = pattern(258048 - 700, 2); // not sector aligned: exercises padding and the
                                          // plain trailing partial sector rule
    let uboot = pattern(0x400 * 512, 3);
    // A misc image as Rockchip ships it: the same boot command at offset 0 and at 16 KiB.
    let misc = {
        let mut m = vec![0u8; 0xc000];
        for off in [0usize, 0x4000] {
            m[off..off + 13].copy_from_slice(b"boot-recovery");
            m[off + 64..off + 64 + 19].copy_from_slice(b"recovery\n--wipe_all");
        }
        m
    };
    // A boot image with a real Android header, so the bootloader (and this tool) can read the
    // Android version off it. os_version 0x1c000196 is Android 14.
    let boot_img = {
        let mut b = pattern(0x800 * 512 - 100, 5);
        b[0..8].copy_from_slice(b"ANDROID!");
        b[8..12].copy_from_slice(&40_000u32.to_le_bytes()); // kernel_size
        b[16..20].copy_from_slice(&2_000u32.to_le_bytes()); // ramdisk_size
        b[36..40].copy_from_slice(&2048u32.to_le_bytes()); // page_size
        b[40..44].copy_from_slice(&2u32.to_le_bytes()); // header_version
        b[44..48].copy_from_slice(&0x1c00_0196u32.to_le_bytes()); // os_version: Android 14
        b
    };
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
    // Item placement follows the parameter this fixture was built with.
    let parsed = rockchip_sd_tool::parameter::Parameter::parse(parameter).unwrap();
    let boot_part = parsed.partition("boot").unwrap().size.unwrap() as u32;
    let super_at = parsed.partition("super").unwrap().offset as u32;
    let boot_img = if boot_img.len() as u64 > boot_part as u64 * 512 { boot_img[..boot_part as usize * 512 - 100].to_vec() } else { boot_img };
    let loader = build_loader(&head, &data, &boot);
    let af = build_af(&[
        ("package-file", 0, 0xffff_ffff, b"# NAME Relative path\n".to_vec()),
        ("bootloader", 0, 0xffff_ffff, loader.clone()),
        ("parameter", 0x2000, 0, parameter.as_bytes().to_vec()),
        ("uboot", 0x400, 0x2000, uboot.clone()),
        ("misc", 0x100, 0x2400, misc.clone()),
        ("boot", boot_part, 0x2500, boot_img.clone()),
        ("super", 0x1000, super_at, super_sparse.clone()),
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
    check_card_ex(path, total_sectors, fx, true)
}

/// `misc_from_image` is false after an upgrade, which leaves the device's own state alone.
fn check_card_ex(path: &PathBuf, total_sectors: u64, fx: &Fixture, misc_from_image: bool) {
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
    if misc_from_image {
        // misc is written with only the boot command the bootloader reads: offset 0 here,
        // because the fixture's boot image says Android 14.
        let mut expect = fx.misc.clone();
        rockchip_sd_tool::plan::normalize_misc(&mut expect, rockchip_sd_tool::plan::BCB_OFFSET_GOOGLE);
        assert_eq!(read_sectors(r, 0x2400, 0x60), expect);
    }
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
    assert_eq!(p.entries.len(), 5);
    let steps: Vec<&str> = p.ops.iter().map(|o| o.step.as_str()).collect();
    assert!(steps.starts_with(&["Clear MBR", "Loader", "Loader", "Loader", "uboot", "misc", "boot", "super"]));
    assert_eq!(&steps[steps.len() - 2..], &["GPT", "GPT"]);
}

#[test]
fn raw_image_roundtrip() {
    let fx = fixture();
    let total_sectors = 0x20000u64; // 64 MiB card
    let out = fx.dir.path().join("card.img");
    let spec = JobSpec { image: fx.image.clone(), output: out.to_string_lossy().to_string(), size: Some(total_sectors * 512), verify: true, xz_level: 1, verify_blocks: true, upgrade: false };
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
    let spec = JobSpec { image: fx.image.clone(), output: out.to_string_lossy().to_string(), size: Some(total_sectors * 512), verify: true, xz_level: 0, verify_blocks: true, upgrade: false };
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
    let spec = JobSpec { image: fx.image.clone(), output: out.to_string_lossy().to_string(), size: Some(0x3d00 * 512), verify: false, xz_level: 1, verify_blocks: true, upgrade: false };
    let err = job::run(&spec, &mut |_| {}, &Cancel::new()).unwrap_err();
    assert!(format!("{err:#}").contains("needs at least"), "{err:#}");
}

#[test]
fn cancel_stops_the_write() {
    let fx = fixture();
    let out = fx.dir.path().join("cancel.img");
    let spec = JobSpec { image: fx.image.clone(), output: out.to_string_lossy().to_string(), size: Some(0x20000 * 512), verify: true, xz_level: 1, verify_blocks: true, upgrade: false };
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

#[test]
fn upgrade_keeps_user_data_and_the_partition_table() {
    let fx = fixture();
    let total_sectors = 0x20000u64;
    let out = fx.dir.path().join("card.img");
    let path = out.to_string_lossy().to_string();
    let full = JobSpec { image: fx.image.clone(), output: path.clone(), size: Some(total_sectors * 512), verify: true, xz_level: 1, verify_blocks: true, upgrade: false };
    job::run(&full, &mut |_| {}, &Cancel::new()).unwrap();

    // What an upgrade must preserve: the table (disk GUID and partition GUIDs included) and
    // everything the image does not carry, above all the user data.
    let table_before = std::fs::read(&out).unwrap()[..34 * 512].to_vec();
    let userdata_at = 0x3d00u64 * 512;
    let misc_at = 0x2400u64 * 512;
    let marker: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 253) as u8).collect();
    // The device's own bootloader control block, as it looks once the device has consumed the
    // factory command. Writing the image's misc over this is what tells the bootloader to enter
    // recovery and wipe the card, so an upgrade must leave it alone.
    let misc_state: Vec<u8> = (0..3000u32).map(|i| (i % 97) as u8).collect();
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&out).unwrap();
        f.seek(SeekFrom::Start(userdata_at)).unwrap();
        f.write_all(&marker).unwrap();
        f.seek(SeekFrom::Start(misc_at)).unwrap();
        f.write_all(&misc_state).unwrap();
        // A stale boot command at the offset the bootloader does not read: the recovery loop.
        f.seek(SeekFrom::Start(misc_at + 0x4000)).unwrap();
        f.write_all(b"boot-recovery\0").unwrap();
        // Something in a partition the image does carry, to prove it is rewritten.
        f.seek(SeekFrom::Start(0x2500 * 512)).unwrap();
        f.write_all(&[0xa5u8; 4096]).unwrap();
    }

    let upgrade = JobSpec { size: None, upgrade: true, ..full.clone() };
    let plan = job::run(&upgrade, &mut |_| {}, &Cancel::new()).unwrap();
    assert_eq!(plan.mode, rockchip_sd_tool::plan::Mode::Upgrade);
    assert!(plan.gpt.is_none(), "an upgrade writes no partition table");
    assert!(plan.ops.iter().all(|o| o.step != "GPT" && o.step != "Clear MBR"));
    // The only thing an upgrade may do to misc is clear the control block the bootloader does
    // not read; it must never write a command there.
    for op in plan.ops.iter().filter(|o| o.step == "misc") {
        assert_eq!(op.source, rockchip_sd_tool::plan::Source::Zero);
        assert_eq!(op.sector, 0x2400 + 0x4000 / 512, "only the unused control block");
        assert_eq!(op.sectors, 4);
    }

    let after = std::fs::read(&out).unwrap();
    assert_eq!(&after[..34 * 512], &table_before[..], "the partition table must be untouched");
    assert_eq!(&after[userdata_at as usize..userdata_at as usize + marker.len()], &marker[..], "user data must be untouched");
    assert_eq!(
        &after[misc_at as usize..misc_at as usize + misc_state.len()],
        &misc_state[..],
        "misc carries the boot-recovery/wipe command in the image, so an upgrade must not write it"
    );
    // The stale control block the bootloader ignores is cleared, which is what rescues a device
    // that is looping into recovery.
    assert!(
        after[misc_at as usize + 0x4000..misc_at as usize + 0x4800].iter().all(|&b| b == 0),
        "the unused boot control block must be cleared"
    );
    // Every firmware partition is back to the image contents.
    check_card_ex(&out, total_sectors, &fx, false);
}

#[test]
fn upgrade_refuses_a_card_with_a_different_layout() {
    let fx = fixture();
    let alt = fixture_param(ALT_PARAMETER);
    let total_sectors = 0x20000u64;
    let out = alt.dir.path().join("alt_card.img");
    let path = out.to_string_lossy().to_string();
    let full = JobSpec { image: alt.image.clone(), output: path.clone(), size: Some(total_sectors * 512), verify: false, xz_level: 1, verify_blocks: true, upgrade: false };
    job::run(&full, &mut |_| {}, &Cancel::new()).unwrap();

    let before = std::fs::read(&out).unwrap();
    let upgrade = JobSpec { image: fx.image.clone(), size: None, upgrade: true, ..full };
    let err = job::run(&upgrade, &mut |_| {}, &Cancel::new()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("boot") && msg.contains("write it in full"), "{msg}");
    assert_eq!(std::fs::read(&out).unwrap(), before, "a refused upgrade must not touch the card");
}

#[test]
fn upgrade_refuses_a_card_without_a_partition_table() {
    let fx = fixture();
    let out = fx.dir.path().join("blank.img");
    std::fs::write(&out, vec![0u8; 0x20000 * 512]).unwrap();
    let spec = JobSpec { image: fx.image.clone(), output: out.to_string_lossy().to_string(), size: None, verify: false, xz_level: 1, verify_blocks: true, upgrade: true };
    let err = job::run(&spec, &mut |_| {}, &Cancel::new()).unwrap_err();
    assert!(format!("{err:#}").contains("no GPT"), "{err:#}");
}

#[test]
fn a_full_write_still_writes_misc() {
    // The factory flash does write misc: that is what makes a fresh card run the vendor's
    // first-boot recovery step. Only an upgrade leaves it alone.
    let fx = fixture();
    let img = RkfwImage::open(&fx.image).unwrap();
    let full = plan::build(&img, 0x20000).unwrap();
    assert!(full.ops.iter().any(|o| o.step == "misc"));
    let entries = full.gpt.as_ref().unwrap().entries.clone();
    let upgrade = plan::build_upgrade(&img, 0x20000, &entries).unwrap();
    // An upgrade writes no misc content; the one misc op it may have is zeroing the control
    // block the bootloader does not read.
    for op in upgrade.ops.iter().filter(|o| o.step == "misc") {
        assert_eq!(op.source, rockchip_sd_tool::plan::Source::Zero);
    }
    assert!(!upgrade.ops.iter().any(|o| o.step == "misc" && o.source != rockchip_sd_tool::plan::Source::Zero));
    for kept in ["misc", "cache", "metadata", "userdata", "frp", "swap", "backup"] {
        assert!(rockchip_sd_tool::plan::upgrade_keeps(kept));
    }
    for written in ["uboot", "boot", "recovery", "super", "dtbo", "vbmeta", "baseparameter"] {
        assert!(!rockchip_sd_tool::plan::upgrade_keeps(written));
    }
}

#[test]
fn the_card_keeps_only_the_boot_command_the_bootloader_reads() {
    use rockchip_sd_tool::plan::{bcb_has_command, BCB_OFFSET_GOOGLE, BCB_OFFSET_ROCKCHIP};
    let fx = fixture();
    let img = RkfwImage::open(&fx.image).unwrap();
    assert_eq!(img.android_major_version(), Some(14));
    // The image itself carries the command twice, which is the hazard.
    assert!(bcb_has_command(&fx.misc, BCB_OFFSET_GOOGLE));
    assert!(bcb_has_command(&fx.misc, BCB_OFFSET_ROCKCHIP));

    let total_sectors = 0x20000u64;
    let out = fx.dir.path().join("bcb.img");
    let spec = JobSpec { image: fx.image.clone(), output: out.to_string_lossy().to_string(), size: Some(total_sectors * 512), verify: true, xz_level: 1, verify_blocks: true, upgrade: false };
    job::run(&spec, &mut |_| {}, &Cancel::new()).unwrap();

    let card = std::fs::read(&out).unwrap();
    let misc_at = 0x2400usize * 512;
    let on_card = &card[misc_at..misc_at + 0xc000];
    assert!(bcb_has_command(on_card, BCB_OFFSET_GOOGLE), "the first-boot command must still be there");
    assert!(!bcb_has_command(on_card, BCB_OFFSET_ROCKCHIP), "the copy the bootloader ignores must be cleared");
    assert_eq!(&on_card[..32], &fx.misc[..32], "the command itself is untouched");
}

#[test]
fn misc_is_left_alone_when_the_android_version_is_unknown() {
    // Without a readable Android boot header there is no way to know which control block the
    // bootloader reads, so the image's misc is written exactly as it is.
    let fx = fixture_param(PARAMETER);
    let img = RkfwImage::open(&fx.image).unwrap();
    assert_eq!(img.android_major_version(), Some(14));
    let mut data = fx.misc.clone();
    rockchip_sd_tool::plan::normalize_misc(&mut data, rockchip_sd_tool::plan::BCB_OFFSET_GOOGLE);
    assert_ne!(data, fx.misc);
    assert_eq!(rockchip_sd_tool::plan::bcb_offset_for(None), None);
}

#[test]
fn a_full_write_clears_the_filesystems_the_device_must_make_itself() {
    use rockchip_sd_tool::plan::{Source, ERASE_BYTES, ERASE_ON_FULL_WRITE};
    let fx = fixture();
    let img = RkfwImage::open(&fx.image).unwrap();
    let full = plan::build(&img, 0x20000).unwrap();
    // Of the partitions Android has to make for itself, this layout has userdata.
    let ud = full.entries.iter().find(|e| e.name == "userdata").unwrap().clone();
    let op = full
        .ops
        .iter()
        .find(|o| o.step == "userdata")
        .expect("a full write clears the start of userdata");
    assert_eq!(op.source, Source::Zero);
    assert_eq!(op.sector, ud.first_lba);
    assert_eq!(op.sectors, ERASE_BYTES / 512);
    // Only the start, never the whole partition.
    assert!(op.sectors < ud.sectors());
    assert!(ERASE_ON_FULL_WRITE.contains(&"userdata"));
}

#[test]
fn an_upgrade_never_clears_those_filesystems() {
    let fx = fixture();
    let img = RkfwImage::open(&fx.image).unwrap();
    let full = plan::build(&img, 0x20000).unwrap();
    let entries = full.gpt.as_ref().unwrap().entries.clone();
    let upgrade = plan::build_upgrade(&img, 0x20000, &entries).unwrap();
    for name in rockchip_sd_tool::plan::ERASE_ON_FULL_WRITE {
        // Only partitions this layout actually has can be cleared.
        if !entries.iter().any(|e| e.name == *name) {
            continue;
        }
        assert!(full.ops.iter().any(|o| o.step == *name), "a full write clears {name}");
        assert!(!upgrade.ops.iter().any(|o| o.step == *name), "an upgrade keeps {name}");
    }
}

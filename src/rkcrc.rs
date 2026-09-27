//! Rockchip's CRC32 (polynomial 0x04c10db7, MSB first, no reflection, initial value 0) used to
//! seal RKBOOT loader blobs and RKAF containers.

fn table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = (i as u32) << 24;
            for _ in 0..8 {
                c = if c & 0x8000_0000 != 0 { (c << 1) ^ 0x04c1_0db7 } else { c << 1 };
            }
            *e = c;
        }
        t
    })
}

pub fn crc32_rk_update(mut crc: u32, data: &[u8]) -> u32 {
    let t = table();
    for &b in data {
        crc = (crc << 8) ^ t[((crc >> 24) as u8 ^ b) as usize];
    }
    crc
}

pub fn crc32_rk(data: &[u8]) -> u32 {
    crc32_rk_update(0, data)
}

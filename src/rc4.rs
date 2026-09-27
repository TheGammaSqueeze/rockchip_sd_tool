//! Rockchip loader scrambling.
//!
//! Every loader entry stored inside an RKBOOT blob (the `LDR ` / `BOOT` section of an RKFW image)
//! is XOR-scrambled with RC4 using Rockchip's fixed key. The keystream is restarted for every
//! 512-byte sector, so the same 512-byte keystream repeats over the whole entry. The SD boot ROM of
//! the newer chips (RK356x and later, "RKNS" header) expects the plain data on the card, so the
//! entries are descrambled before they are written.

/// The fixed RC4 key used by every Rockchip flashing tool (rkdeveloptool, upgrade_tool,
/// SDDiskTool). The last two bytes are 23 and 17, not 13 and 10 as some copies claim.
pub const RK_RC4_KEY: [u8; 16] = [124, 78, 3, 4, 85, 5, 9, 7, 45, 44, 123, 56, 23, 13, 23, 17];

/// Size of one scrambling unit.
pub const SECTOR: usize = 512;

/// Computes the 512-byte keystream once. XOR-ing a sector with it descrambles (or scrambles) it.
pub fn keystream() -> [u8; SECTOR] {
    let mut s = [0u8; 256];
    for (i, v) in s.iter_mut().enumerate() {
        *v = i as u8;
    }
    let mut j: u8 = 0;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(RK_RC4_KEY[i & 15]);
        s.swap(i, j as usize);
    }
    let mut out = [0u8; SECTOR];
    let (mut i, mut j) = (0u8, 0u8);
    for o in out.iter_mut() {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        *o = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
    }
    out
}

/// Applies the per-sector RC4 keystream in place to every complete 512-byte sector. A trailing
/// partial sector is left untouched, exactly like rkdeveloptool and SDDiskTool (their loops run
/// `size / 512` times).
pub fn rc4_sectors(data: &mut [u8]) {
    let ks = keystream();
    for chunk in data.chunks_exact_mut(SECTOR) {
        for (b, k) in chunk.iter_mut().zip(ks.iter()) {
            *b ^= *k;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystream_matches_reference() {
        // First bytes of the keystream observed on a real SD card (sector 64 of a card made by
        // SDDiskTool v1.69 XOR the FlashHead entry of the same RKFW image).
        let ks = keystream();
        assert_eq!(&ks[..16], &[0x6e, 0x26, 0x2c, 0xf3, 0xbe, 0x9f, 0x9d, 0x51, 0xea, 0x30, 0x34, 0xce, 0x20, 0x51, 0x1f, 0x98]);
    }

    #[test]
    fn roundtrip() {
        let mut d: Vec<u8> = (0..1500u32).map(|x| (x * 7 % 251) as u8).collect();
        let orig = d.clone();
        rc4_sectors(&mut d);
        assert_ne!(d, orig);
        rc4_sectors(&mut d);
        assert_eq!(d, orig);
    }
}

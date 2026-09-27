//! Android sparse image format (`0xed26ff3a`), expanded on the fly while writing.

use anyhow::{bail, Result};

pub const SPARSE_MAGIC: u32 = 0xed26_ff3a;
pub const CHUNK_RAW: u16 = 0xCAC1;
pub const CHUNK_FILL: u16 = 0xCAC2;
pub const CHUNK_DONT_CARE: u16 = 0xCAC3;
pub const CHUNK_CRC32: u16 = 0xCAC4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseHeader {
    pub major: u16,
    pub minor: u16,
    pub file_hdr_size: u16,
    pub chunk_hdr_size: u16,
    pub block_size: u32,
    pub total_blocks: u32,
    pub total_chunks: u32,
    pub image_checksum: u32,
}

impl SparseHeader {
    pub fn parse(b: &[u8]) -> Option<SparseHeader> {
        if b.len() < 28 {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
        if u32_at(0) != SPARSE_MAGIC {
            return None;
        }
        Some(SparseHeader {
            major: u16_at(4),
            minor: u16_at(6),
            file_hdr_size: u16_at(8),
            chunk_hdr_size: u16_at(10),
            block_size: u32_at(12),
            total_blocks: u32_at(16),
            total_chunks: u32_at(20),
            image_checksum: u32_at(24),
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.major != 1 {
            bail!("unsupported sparse image major version {}", self.major);
        }
        if self.file_hdr_size < 28 || self.chunk_hdr_size < 12 {
            bail!("bad sparse header sizes");
        }
        if self.block_size == 0 || self.block_size % 512 != 0 {
            bail!("sparse block size {} is not a multiple of 512", self.block_size);
        }
        Ok(())
    }

    /// Size of the expanded image in bytes.
    pub fn expanded_size(&self) -> u64 {
        self.block_size as u64 * self.total_blocks as u64
    }
}

/// One chunk of a sparse image, resolved to absolute file positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunk {
    /// `len` bytes of raw data at file offset `data_offset`, placed at expanded offset `out`.
    Raw { out: u64, len: u64, data_offset: u64 },
    /// `len` bytes of the repeated 4-byte pattern.
    Fill { out: u64, len: u64, pattern: [u8; 4] },
    /// `len` bytes the image does not define.
    DontCare { out: u64, len: u64 },
}

impl Chunk {
    pub fn out(&self) -> u64 {
        match self {
            Chunk::Raw { out, .. } | Chunk::Fill { out, .. } | Chunk::DontCare { out, .. } => *out,
        }
    }
    pub fn len(&self) -> u64 {
        match self {
            Chunk::Raw { len, .. } | Chunk::Fill { len, .. } | Chunk::DontCare { len, .. } => *len,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Walks the chunk table of a sparse image. `read_at(offset, len)` reads from the image file;
/// `base` is the absolute file offset of the sparse header and `avail` the bytes after it.
pub fn walk_chunks(
    hdr: &SparseHeader,
    base: u64,
    avail: u64,
    mut read_at: impl FnMut(u64, usize) -> Result<Vec<u8>>,
) -> Result<Vec<Chunk>> {
    hdr.validate()?;
    let mut chunks = Vec::with_capacity(hdr.total_chunks as usize);
    let mut pos = base + hdr.file_hdr_size as u64;
    let mut out_blocks: u64 = 0;
    let end = base + avail;
    for i in 0..hdr.total_chunks {
        if pos + hdr.chunk_hdr_size as u64 > end {
            bail!("sparse chunk {i} header lies past the end of the item");
        }
        let ch = read_at(pos, hdr.chunk_hdr_size as usize)?;
        let ctype = u16::from_le_bytes([ch[0], ch[1]]);
        let cblocks = u32::from_le_bytes(ch[4..8].try_into().unwrap()) as u64;
        let total = u32::from_le_bytes(ch[8..12].try_into().unwrap()) as u64;
        let out = out_blocks * hdr.block_size as u64;
        let len = cblocks * hdr.block_size as u64;
        let data_len = total.checked_sub(hdr.chunk_hdr_size as u64).ok_or_else(|| anyhow::anyhow!("sparse chunk {i} has a bad total size"))?;
        if pos + total > end {
            bail!("sparse chunk {i} data lies past the end of the item");
        }
        match ctype {
            CHUNK_RAW => {
                if data_len != len {
                    bail!("sparse raw chunk {i}: {data_len} bytes for {cblocks} blocks");
                }
                chunks.push(Chunk::Raw { out, len, data_offset: pos + hdr.chunk_hdr_size as u64 });
            }
            CHUNK_FILL => {
                if data_len != 4 {
                    bail!("sparse fill chunk {i} has {data_len} bytes of pattern");
                }
                let p = read_at(pos + hdr.chunk_hdr_size as u64, 4)?;
                chunks.push(Chunk::Fill { out, len, pattern: [p[0], p[1], p[2], p[3]] });
            }
            CHUNK_DONT_CARE => {
                chunks.push(Chunk::DontCare { out, len });
            }
            CHUNK_CRC32 => {
                // Checksum chunk: carries no blocks.
                if cblocks != 0 {
                    bail!("sparse crc chunk {i} claims {cblocks} blocks");
                }
            }
            other => bail!("unknown sparse chunk type 0x{other:04x} in chunk {i}"),
        }
        out_blocks += cblocks;
        pos += total;
    }
    if out_blocks != hdr.total_blocks as u64 {
        bail!("sparse image covers {out_blocks} blocks but the header says {}", hdr.total_blocks);
    }
    Ok(chunks)
}

/// Builds a small sparse image in memory (used by the tests).
pub fn build_sparse(block_size: u32, chunks: &[(u16, u32, Vec<u8>)]) -> Vec<u8> {
    let total_blocks: u32 = chunks.iter().map(|c| c.1).sum();
    let mut v = Vec::new();
    v.extend_from_slice(&SPARSE_MAGIC.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&28u16.to_le_bytes());
    v.extend_from_slice(&12u16.to_le_bytes());
    v.extend_from_slice(&block_size.to_le_bytes());
    v.extend_from_slice(&total_blocks.to_le_bytes());
    v.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    for (t, blocks, data) in chunks {
        v.extend_from_slice(&t.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&blocks.to_le_bytes());
        v.extend_from_slice(&(12 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn walk() {
        let img = build_sparse(4096, &[
            (CHUNK_RAW, 1, vec![7u8; 4096]),
            (CHUNK_DONT_CARE, 2, vec![]),
            (CHUNK_FILL, 1, vec![1, 2, 3, 4]),
            (CHUNK_CRC32, 0, vec![0, 0, 0, 0]),
        ]);
        let hdr = SparseHeader::parse(&img).unwrap();
        assert_eq!(hdr.expanded_size(), 4 * 4096);
        let chunks = walk_chunks(&hdr, 0, img.len() as u64, |o, l| Ok(img[o as usize..o as usize + l].to_vec())).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], Chunk::Raw { out: 0, len: 4096, data_offset: 40 });
        assert_eq!(chunks[1], Chunk::DontCare { out: 4096, len: 8192 });
        assert_eq!(chunks[2], Chunk::Fill { out: 12288, len: 4096, pattern: [1, 2, 3, 4] });
    }
}

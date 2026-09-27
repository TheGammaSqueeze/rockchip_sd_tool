//! Block-wise xz image writing and random-access reading.
//!
//! The card image is written as one standard `.xz` stream made of independent blocks (the same
//! structure `xz -T` produces). Building the stream ourselves brings two things a plain encoder
//! cannot: an all-zero block is compressed once and its compressed bytes are reused for every
//! other zero block (a 128 GB card image is mostly zeros), and the stream index lets the verifier
//! seek to any block instead of decoding the whole file.
//!
//! Stream layout (xz file format 1.1.0): stream header (12 bytes), blocks, index, stream footer
//! (12 bytes). Every block here is produced by encoding it as its own single-block stream with
//! liblzma and splicing the block bytes out, so the compressed content is exactly what liblzma
//! makes.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use anyhow::{bail, Context, Result};

use crate::target::Target;

const XZ_MAGIC: [u8; 6] = [0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00];
const FOOTER_MAGIC: [u8; 2] = [0x59, 0x5a];
/// Stream flags: check type CRC64.
const STREAM_FLAGS: [u8; 2] = [0x00, 0x04];
/// Uncompressed bytes per block.
pub const BLOCK_SIZE: usize = 8 << 20;

fn varint_encode(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn varint_decode(b: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let Some(&c) = b.get(*pos) else { bail!("truncated xz varint") };
        *pos += 1;
        v |= ((c & 0x7f) as u64) << shift;
        if c & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            bail!("bad xz varint");
        }
    }
}

fn pad4(n: u64) -> u64 {
    (n + 3) & !3
}

/// One block as recorded in the stream index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRecord {
    /// Offset of the block header in the file.
    pub file_offset: u64,
    /// Header + compressed data + check, without padding.
    pub unpadded_size: u64,
    pub uncompressed_offset: u64,
    pub uncompressed_size: u64,
}

fn stream_header() -> Vec<u8> {
    let mut h = XZ_MAGIC.to_vec();
    h.extend_from_slice(&STREAM_FLAGS);
    h.extend_from_slice(&crc32fast::hash(&STREAM_FLAGS).to_le_bytes());
    h
}

fn index_bytes(records: &[(u64, u64)]) -> Vec<u8> {
    let mut idx = vec![0u8];
    varint_encode(records.len() as u64, &mut idx);
    for (unpadded, uncompressed) in records {
        varint_encode(*unpadded, &mut idx);
        varint_encode(*uncompressed, &mut idx);
    }
    while idx.len() % 4 != 0 {
        idx.push(0);
    }
    let crc = crc32fast::hash(&idx);
    idx.extend_from_slice(&crc.to_le_bytes());
    idx
}

fn stream_footer(index_len: usize) -> Vec<u8> {
    let backward = ((index_len / 4) - 1) as u32;
    let mut body = backward.to_le_bytes().to_vec();
    body.extend_from_slice(&STREAM_FLAGS);
    let mut f = crc32fast::hash(&body).to_le_bytes().to_vec();
    f.extend_from_slice(&body);
    f.extend_from_slice(&FOOTER_MAGIC);
    f
}

/// Parses the index of a stream whose bytes are given; returns (records, index_offset).
fn parse_stream_index(data: &[u8]) -> Result<Vec<BlockRecord>> {
    if data.len() < 12 + 12 + 8 || data[..6] != XZ_MAGIC {
        bail!("not an xz stream");
    }
    let footer = &data[data.len() - 12..];
    if footer[10..12] != FOOTER_MAGIC {
        bail!("xz stream footer missing (file truncated?)");
    }
    let backward = u32::from_le_bytes(footer[4..8].try_into().unwrap()) as usize;
    let index_len = (backward + 1) * 4;
    if crc32fast::hash(&footer[4..10]) != u32::from_le_bytes(footer[0..4].try_into().unwrap()) {
        bail!("xz stream footer CRC mismatch");
    }
    let index_off = data.len() - 12 - index_len;
    let idx = &data[index_off..data.len() - 12];
    if idx[0] != 0 {
        bail!("xz index indicator missing");
    }
    if crc32fast::hash(&idx[..idx.len() - 4]) != u32::from_le_bytes(idx[idx.len() - 4..].try_into().unwrap()) {
        bail!("xz index CRC mismatch");
    }
    let mut pos = 1;
    let count = varint_decode(idx, &mut pos)?;
    let mut records = Vec::with_capacity(count as usize);
    let mut file_offset = 12u64;
    let mut uoff = 0u64;
    for _ in 0..count {
        let unpadded = varint_decode(idx, &mut pos)?;
        let usize_ = varint_decode(idx, &mut pos)?;
        records.push(BlockRecord { file_offset, unpadded_size: unpadded, uncompressed_offset: uoff, uncompressed_size: usize_ });
        file_offset += pad4(unpadded);
        uoff += usize_;
    }
    if file_offset != index_off as u64 {
        bail!("xz index does not match the block sizes");
    }
    Ok(records)
}

/// Compresses `data` as one xz block (header + data + check, padded to 4 bytes) with liblzma's
/// easy encoder at `level`; also returns the unpadded size the index needs.
fn encode_block_rec(data: &[u8], level: u32) -> Result<(Vec<u8>, u64)> {
    let stream = liblzma::stream::Stream::new_easy_encoder(level, liblzma::stream::Check::Crc64)
        .context("cannot start the xz encoder")?;
    let mut enc = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    enc.write_all(data)?;
    let out = enc.finish()?;
    let records = parse_stream_index(&out)?;
    if records.len() != 1 || records[0].uncompressed_size != data.len() as u64 {
        bail!("unexpected xz block structure");
    }
    let padded = pad4(records[0].unpadded_size) as usize;
    Ok((out[12..12 + padded].to_vec(), records[0].unpadded_size))
}

enum Pending {
    Ready(Vec<u8>, u64, u64),
    Thread(JoinHandle<Result<(Vec<u8>, u64)>>, u64),
}

/// Writes a card image as a block-wise xz stream. Sequential only.
pub struct XzImageWriter {
    file: std::io::BufWriter<File>,
    path: PathBuf,
    level: u32,
    threads: usize,
    size: u64,
    /// Uncompressed position of the end of the data handed over so far.
    pos: u64,
    current: Vec<u8>,
    pending: VecDeque<Pending>,
    records: Vec<(u64, u64)>,
    zero_cache: HashMap<usize, (Vec<u8>, u64)>,
}

impl XzImageWriter {
    pub fn create(path: &Path, size: u64, level: u32, threads: usize) -> Result<XzImageWriter> {
        if size % 512 != 0 {
            bail!("image size must be a multiple of 512 bytes");
        }
        let mut file = std::io::BufWriter::with_capacity(4 << 20, File::create(path).with_context(|| format!("cannot create {}", path.display()))?);
        file.write_all(&stream_header())?;
        Ok(XzImageWriter {
            file,
            path: path.to_path_buf(),
            level: level.min(9),
            threads: threads.max(1),
            size,
            pos: 0,
            current: Vec::with_capacity(BLOCK_SIZE),
            pending: VecDeque::new(),
            records: Vec::new(),
            zero_cache: HashMap::new(),
        })
    }

    fn drain_one(&mut self) -> Result<()> {
        let Some(p) = self.pending.pop_front() else { return Ok(()) };
        let (bytes, unpadded, uncompressed) = match p {
            Pending::Ready(b, u, n) => (b, u, n),
            Pending::Thread(h, n) => {
                let (b, u) = h.join().map_err(|_| anyhow::anyhow!("xz worker panicked"))??;
                (b, u, n)
            }
        };
        self.file.write_all(&bytes)?;
        self.records.push((unpadded, uncompressed));
        Ok(())
    }

    fn push_block(&mut self, data: Vec<u8>) -> Result<()> {
        let n = data.len() as u64;
        if data.iter().all(|&b| b == 0) {
            let entry = match self.zero_cache.get(&data.len()) {
                Some(e) => e.clone(),
                None => {
                    let e = encode_block_rec(&data, self.level)?;
                    self.zero_cache.insert(data.len(), e.clone());
                    e
                }
            };
            self.pending.push_back(Pending::Ready(entry.0, entry.1, n));
        } else {
            let level = self.level;
            let h = std::thread::spawn(move || encode_block_rec(&data, level));
            self.pending.push_back(Pending::Thread(h, n));
        }
        while self.pending.len() > self.threads {
            self.drain_one()?;
        }
        Ok(())
    }

    fn flush_current(&mut self) -> Result<()> {
        if !self.current.is_empty() {
            let d = std::mem::replace(&mut self.current, Vec::with_capacity(BLOCK_SIZE));
            self.push_block(d)?;
        }
        Ok(())
    }

    fn append(&mut self, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let room = BLOCK_SIZE - self.current.len();
            let n = room.min(data.len());
            self.current.extend_from_slice(&data[..n]);
            data = &data[n..];
            self.pos += n as u64;
            if self.current.len() == BLOCK_SIZE {
                self.flush_current()?;
            }
        }
        Ok(())
    }

    fn fill_zero_to(&mut self, off: u64) -> Result<()> {
        let mut remaining = off - self.pos;
        while remaining > 0 {
            if self.current.is_empty() && remaining >= BLOCK_SIZE as u64 {
                // Whole zero block: reuse the cached compressed form.
                self.push_block(vec![0u8; BLOCK_SIZE])?;
                self.pos += BLOCK_SIZE as u64;
                remaining -= BLOCK_SIZE as u64;
            } else {
                let room = (BLOCK_SIZE - self.current.len()) as u64;
                let n = room.min(remaining) as usize;
                self.current.resize(self.current.len() + n, 0);
                self.pos += n as u64;
                remaining -= n as u64;
                if self.current.len() == BLOCK_SIZE {
                    self.flush_current()?;
                }
            }
        }
        Ok(())
    }
}

impl Target for XzImageWriter {
    fn size(&self) -> u64 {
        self.size
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        if off < self.pos {
            bail!("xz image: write at {} behind the stream position {}", off, self.pos);
        }
        if off + buf.len() as u64 > self.size {
            bail!("write of {} bytes at {} exceeds the image size {}", buf.len(), off, self.size);
        }
        self.fill_zero_to(off)?;
        self.append(buf)
    }
    fn read_at(&mut self, _off: u64, _buf: &mut [u8]) -> Result<()> {
        bail!("an xz image cannot be read back while it is being written")
    }
    fn sequential_only(&self) -> bool {
        true
    }
    fn zero_by_default(&self) -> bool {
        true
    }
    fn supports_block_verify(&self) -> bool {
        false
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.fill_zero_to(self.size)?;
        self.flush_current()?;
        while !self.pending.is_empty() {
            self.drain_one()?;
        }
        let idx = index_bytes(&self.records);
        self.file.write_all(&idx)?;
        self.file.write_all(&stream_footer(idx.len()))?;
        self.file.flush()?;
        self.file.get_mut().sync_all()?;
        Ok(())
    }
    fn description(&self) -> String {
        format!("compressed image {}", self.path.display())
    }
}

/// Random access reader over a block-wise xz image (any single-stream xz file works; the
/// granularity is the block size the file was made with).
pub struct XzImageReader {
    file: File,
    path: PathBuf,
    records: Vec<BlockRecord>,
    size: u64,
    cache: Option<(usize, Vec<u8>)>,
}

impl XzImageReader {
    pub fn open(path: &Path) -> Result<XzImageReader> {
        let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let len = file.metadata()?.len();
        if len < 32 {
            bail!("{} is too small to be an xz image", path.display());
        }
        // Footer, then the index it points at.
        let mut footer = [0u8; 12];
        file.seek(SeekFrom::Start(len - 12))?;
        file.read_exact(&mut footer)?;
        if footer[10..12] != FOOTER_MAGIC {
            bail!("{} has no xz stream footer (truncated or not a single-stream xz file)", path.display());
        }
        let backward = u32::from_le_bytes(footer[4..8].try_into().unwrap()) as u64;
        let index_len = (backward + 1) * 4;
        if index_len + 24 > len {
            bail!("xz index size is inconsistent");
        }
        // Read header + index + footer into a buffer shaped like a stream for the parser.
        let mut head = [0u8; 12];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut head)?;
        let mut idx = vec![0u8; index_len as usize];
        file.seek(SeekFrom::Start(len - 12 - index_len))?;
        file.read_exact(&mut idx)?;
        let records = parse_index_only(&head, &idx, &footer, len - 12 - index_len)?;
        let size = records.iter().map(|r| r.uncompressed_size).sum();
        Ok(XzImageReader { file, path: path.to_path_buf(), records, size, cache: None })
    }

    fn decode_block(&mut self, i: usize) -> Result<&[u8]> {
        if self.cache.as_ref().map(|c| c.0) != Some(i) {
            let r = self.records[i];
            let padded = pad4(r.unpadded_size) as usize;
            let mut block = vec![0u8; padded];
            self.file.seek(SeekFrom::Start(r.file_offset))?;
            self.file.read_exact(&mut block)?;
            // Wrap the block into a synthetic single-block stream and decode it.
            let mut stream = stream_header();
            stream.extend_from_slice(&block);
            let idx = index_bytes(&[(r.unpadded_size, r.uncompressed_size)]);
            stream.extend_from_slice(&idx);
            stream.extend_from_slice(&stream_footer(idx.len()));
            let mut dec = liblzma::read::XzDecoder::new(std::io::Cursor::new(stream));
            let mut out = Vec::with_capacity(r.uncompressed_size as usize);
            dec.read_to_end(&mut out).with_context(|| format!("xz block {i} is corrupt"))?;
            if out.len() as u64 != r.uncompressed_size {
                bail!("xz block {i} decoded to {} bytes, expected {}", out.len(), r.uncompressed_size);
            }
            self.cache = Some((i, out));
        }
        Ok(&self.cache.as_ref().unwrap().1)
    }
}

fn parse_index_only(head: &[u8], idx: &[u8], footer: &[u8], index_off: u64) -> Result<Vec<BlockRecord>> {
    if head[..6] != XZ_MAGIC {
        bail!("not an xz stream");
    }
    if head[6..8] != STREAM_FLAGS {
        // Other check types are fine for reading: we copy blocks verbatim into a synthetic stream
        // with our own flags, which only works when the check type matches. Refuse clearly.
        bail!("xz stream uses check type {} (only CRC64 images made by this tool can be verified by seeking)", head[7]);
    }
    if crc32fast::hash(&footer[4..10]) != u32::from_le_bytes(footer[0..4].try_into().unwrap()) {
        bail!("xz stream footer CRC mismatch");
    }
    if idx[0] != 0 || crc32fast::hash(&idx[..idx.len() - 4]) != u32::from_le_bytes(idx[idx.len() - 4..].try_into().unwrap()) {
        bail!("xz index CRC mismatch");
    }
    let mut pos = 1;
    let count = varint_decode(idx, &mut pos)?;
    let mut records = Vec::with_capacity(count as usize);
    let mut file_offset = 12u64;
    let mut uoff = 0u64;
    for _ in 0..count {
        let unpadded = varint_decode(idx, &mut pos)?;
        let usize_ = varint_decode(idx, &mut pos)?;
        records.push(BlockRecord { file_offset, unpadded_size: unpadded, uncompressed_offset: uoff, uncompressed_size: usize_ });
        file_offset += pad4(unpadded);
        uoff += usize_;
    }
    if file_offset != index_off {
        bail!("xz index does not match the block sizes");
    }
    Ok(records)
}

impl Target for XzImageReader {
    fn size(&self) -> u64 {
        self.size
    }
    fn write_at(&mut self, _off: u64, _buf: &[u8]) -> Result<()> {
        bail!("compressed image opened read-only")
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        if off + buf.len() as u64 > self.size {
            bail!("read of {} bytes at {} exceeds the image size {}", buf.len(), off, self.size);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            let i = match self.records.binary_search_by(|r| {
                if pos < r.uncompressed_offset {
                    std::cmp::Ordering::Greater
                } else if pos >= r.uncompressed_offset + r.uncompressed_size {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            }) {
                Ok(i) => i,
                Err(_) => bail!("offset {pos} is not covered by any xz block"),
            };
            let r = self.records[i];
            let data = self.decode_block(i)?;
            let start = (pos - r.uncompressed_offset) as usize;
            let n = (data.len() - start).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&data[start..start + n]);
            done += n;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn description(&self) -> String {
        format!("compressed image {}", self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints() {
        let mut v = Vec::new();
        varint_encode(300, &mut v);
        assert_eq!(v, vec![0xac, 0x02]);
        let mut p = 0;
        assert_eq!(varint_decode(&v, &mut p).unwrap(), 300);
    }

    #[test]
    fn write_and_seek() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.img.xz");
        let size = 3 * BLOCK_SIZE as u64 + 4096;
        let mut w = XzImageWriter::create(&path, size, 1, 2).unwrap();
        let data: Vec<u8> = (0..70000u32).map(|i| (i % 251) as u8).collect();
        w.write_at(1024, &data[..512]).unwrap();
        w.write_at(BLOCK_SIZE as u64 + 100 * 512, &data).unwrap();
        w.write_at(size - 512, &data[..512]).unwrap();
        w.finish().unwrap();
        let mut r = XzImageReader::open(&path).unwrap();
        assert_eq!(r.size(), size);
        assert_eq!(r.records.len(), 4);
        let mut b = vec![0u8; 512];
        r.read_at(1024, &mut b).unwrap();
        assert_eq!(b, &data[..512]);
        let mut b = vec![0u8; 70000];
        r.read_at(BLOCK_SIZE as u64 + 100 * 512, &mut b).unwrap();
        assert_eq!(b, data);
        let mut b = vec![0u8; 1024];
        r.read_at(512, &mut b).unwrap();
        assert!(b[..512].iter().all(|&x| x == 0));
        assert_eq!(&b[512..], &data[..512]);
        let mut b = vec![0u8; 512];
        r.read_at(size - 512, &mut b).unwrap();
        assert_eq!(b, &data[..512]);
        // The whole file decodes with a normal decoder too.
        let mut dec = liblzma::read::XzDecoder::new(File::open(&path).unwrap());
        let mut all = Vec::new();
        dec.read_to_end(&mut all).unwrap();
        assert_eq!(all.len() as u64, size);
        assert_eq!(&all[1024..1536], &data[..512]);
    }
}

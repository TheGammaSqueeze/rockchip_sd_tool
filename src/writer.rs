//! Executes a [`Plan`] on a [`Target`] and verifies the result by reading it back.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::plan::{Op, Plan, Source};
use crate::target::Target;

pub const CHUNK: usize = 8 << 20;

/// Progress report sent while writing or verifying.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Progress {
    /// `write` or `verify`.
    pub phase: String,
    /// Current step name (loader, partition name, GPT).
    pub step: String,
    pub done: u64,
    pub total: u64,
    /// Blocks that had to be written again because the read-back did not match.
    #[serde(default)]
    pub retries: u64,
}

/// How each block is written.
#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    /// After every block: flush, read it back from the device and compare; rewrite on mismatch.
    pub verify_blocks: bool,
    /// Attempts per block before giving up (the Windows tool uses 3).
    pub attempts: u32,
    /// Pause between attempts.
    pub retry_delay: std::time::Duration,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions { verify_blocks: true, attempts: 3, retry_delay: std::time::Duration::from_millis(250) }
    }
}

pub type ProgressFn<'a> = &'a mut dyn FnMut(Progress);

pub struct Cancel(pub Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Cancel {
        Cancel(Arc::new(AtomicBool::new(false)))
    }
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads image bytes for `File` sources.
struct ImageSource {
    file: File,
}

impl ImageSource {
    fn open(img: &crate::rkfw::RkfwImage) -> Result<ImageSource> {
        Ok(ImageSource { file: File::open(&img.path)? })
    }
    fn read(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(off))?;
        self.file.read_exact(buf).with_context(|| format!("image read failed at {off}"))?;
        Ok(())
    }
}

/// Produces the bytes of an op piece by piece.
fn op_pieces(
    op: &Op,
    src: &mut ImageSource,
    mut f: impl FnMut(u64, &[u8]) -> Result<()>,
    cancel: &Cancel,
) -> Result<()> {
    let total = op.bytes() as usize;
    let mut buf = vec![0u8; std::cmp::min(CHUNK, total)];
    let mut done = 0usize;
    while done < total {
        if cancel.is_set() {
            bail!("cancelled");
        }
        let n = std::cmp::min(buf.len(), total - done);
        let piece = &mut buf[..n];
        match &op.source {
            Source::Zero => piece.fill(0),
            Source::Fill(p) => {
                for (i, b) in piece.iter_mut().enumerate() {
                    *b = p[i & 3];
                }
            }
            Source::File { offset, len } => {
                let start = done as u64;
                if start < *len {
                    let avail = std::cmp::min(n as u64, len - start) as usize;
                    src.read(offset + start, &mut piece[..avail])?;
                    piece[avail..].fill(0);
                } else {
                    piece.fill(0);
                }
            }
            Source::Bytes(b) => {
                let avail = b.len().saturating_sub(done).min(n);
                piece[..avail].copy_from_slice(&b[done..done + avail]);
                piece[avail..].fill(0);
            }
        }
        f(op.sector * 512 + done as u64, piece)?;
        done += n;
    }
    Ok(())
}

/// Writes the plan. Random-access targets get the plan in the tool's order; sequential targets
/// and targets that read back as zero get the flattened plan (identical final content, no
/// double writes). Returns the list of ops written (for verification).
pub fn write_plan(
    plan: &Plan,
    img: &crate::rkfw::RkfwImage,
    target: &mut dyn Target,
    opts: WriteOptions,
    progress: ProgressFn,
    cancel: &Cancel,
) -> Result<Vec<Op>> {
    if target.size() < plan.total_sectors * 512 {
        bail!(
            "target is {} bytes but the plan was made for {} bytes",
            target.size(),
            plan.total_sectors * 512
        );
    }
    let flat = plan.flattened();
    let skip_zero = target.zero_by_default();
    let ops: Vec<Op> = if target.sequential_only() || skip_zero {
        flat.into_iter().filter(|o| !(skip_zero && matches!(o.source, Source::Zero))).collect()
    } else {
        // A real device: write everything in order, later ops override earlier ones just as the
        // Windows tool does. The flattened order is also ascending, which is fastest on cards,
        // and the content is identical, so use it but keep the zero ops.
        flat
    };
    let total: u64 = ops.iter().map(|o| o.bytes()).sum();
    let mut done = 0u64;
    let mut retries = 0u64;
    let verify_blocks = opts.verify_blocks && !target.sequential_only() && target.supports_block_verify();
    let attempts = opts.attempts.max(1);
    let mut src = ImageSource::open(img)?;
    let mut rb = vec![0u8; CHUNK];
    for op in &ops {
        let step = op.step.clone();
        progress(Progress { phase: "write".into(), step: step.clone(), done, total, retries });
        op_pieces(
            op,
            &mut src,
            |off, piece| {
                let mut attempt = 0;
                loop {
                    attempt += 1;
                    let result = write_block(target, off, piece, verify_blocks, &mut rb[..piece.len()]);
                    match result {
                        Ok(()) => break,
                        Err(e) if attempt < attempts => {
                            retries += 1;
                            progress(Progress { phase: "write".into(), step: step.clone(), done, total, retries });
                            std::thread::sleep(opts.retry_delay);
                            let _ = e;
                        }
                        Err(e) => {
                            return Err(e.context(format!(
                                "block at sector {} ({}) failed {} times; the card may be faulty or counterfeit",
                                off / 512,
                                step,
                                attempts
                            )));
                        }
                    }
                }
                done += piece.len() as u64;
                progress(Progress { phase: "write".into(), step: step.clone(), done, total, retries });
                Ok(())
            },
            cancel,
        )?;
    }
    progress(Progress { phase: "write".into(), step: "Flushing".into(), done: total, total, retries });
    target.finish()?;
    Ok(ops)
}

/// One attempt: write the block; when verifying, flush it to the medium, read it back through the
/// uncached path and compare byte for byte.
fn write_block(target: &mut dyn Target, off: u64, piece: &[u8], verify: bool, rb: &mut [u8]) -> Result<()> {
    target.write_at(off, piece).with_context(|| format!("write failed at sector {}", off / 512))?;
    if verify {
        target.flush().context("flush failed")?;
        target.read_at(off, rb).with_context(|| format!("read-back failed at sector {}", off / 512))?;
        if rb != piece {
            let bad = rb.iter().zip(piece.iter()).position(|(a, b)| a != b).unwrap_or(0);
            bail!("read-back mismatch at sector {} (byte {} differs)", (off + bad as u64) / 512, off + bad as u64);
        }
    }
    Ok(())
}

/// Reads every written range back from a random-access target and compares it.
pub fn verify_target(
    ops: &[Op],
    img: &crate::rkfw::RkfwImage,
    target: &mut dyn Target,
    progress: ProgressFn,
    cancel: &Cancel,
) -> Result<()> {
    let total: u64 = ops.iter().map(|o| o.bytes()).sum();
    let mut done = 0u64;
    let mut src = ImageSource::open(img)?;
    let mut rb = vec![0u8; CHUNK];
    for op in ops {
        let step = op.step.clone();
        op_pieces(
            op,
            &mut src,
            |off, piece| {
                let r = &mut rb[..piece.len()];
                target.read_at(off, r)?;
                if r != piece {
                    let bad = r.iter().zip(piece.iter()).position(|(a, b)| a != b).unwrap_or(0);
                    bail!(
                        "verification failed in {} at byte {} (sector {}): the card does not hold what was written",
                        step,
                        off + bad as u64,
                        (off + bad as u64) / 512
                    );
                }
                done += piece.len() as u64;
                progress(Progress { phase: "verify".into(), step: step.clone(), done, total, retries: 0 });
                Ok(())
            },
            cancel,
        )?;
    }
    Ok(())
}

//! A v7 image served straight from its file through a memory map.
//!
//! [`TurboQuantIndex::load`](crate::TurboQuantIndex::load) reads the whole
//! image into memory and turns it into the search cache; for an index
//! that only serves reads — a sealed shard, a segment — that copy is
//! the file's size in heap, paid at open, for bytes the page cache
//! already holds. A [`MappedImage`] maps the file instead and produces
//! the search cache one chunk at a time, on demand, from the mapped
//! pages: no bytes move at open beyond the superblock and the two
//! commit headers, and a scan touches the file through the page cache.
//!
//! Nothing about the scoring changes. A v7 block unit stores its 32
//! rows' codes in the sequential-blocked layout and its 32 scales right
//! behind them; a chunk gathers the code bytes of its blocks, applies
//! the redo ops the loaded commit header carries (exactly as
//! [`crate::io_v7::load_image`] does), and runs the same
//! stored-to-native transform the loader runs, so the kernel reads the
//! same bytes it would read from a loaded index and produces the same
//! scores bit for bit. The chunk cache is bounded ("paged"): chunks are
//! assembled when a scan reaches them and the least recently used ones
//! are dropped past the budget, so resident memory stays at the budget
//! plus whatever the page cache keeps, never the image.
//!
//! A mapped index is read-only. The file and its backing storage must not
//! change while it is mapped: the caller owns that safety guarantee (an
//! image that is being synced is not a candidate). The v7 format does not
//! carry a checksum for every block unit, so validation cannot substitute
//! for that immutability requirement.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::io_v7::{parse_image, Geo, ParsedImage};
use crate::pack;
use crate::BLOCK;

/// Default budget for assembled chunks held in memory: enough for a
/// handful of in-flight scans without holding the image.
pub const DEFAULT_CHUNK_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// One assembled chunk: whole blocks from `base`, in the kernel's
/// native layout, with their scales.
pub(crate) struct Chunk {
    /// `chunk_blocks * block_bytes` bytes, native layout.
    pub codes: Vec<u8>,
    /// `live` scales.
    pub scales: Vec<f32>,
}

impl Chunk {
    fn bytes(&self) -> usize {
        self.codes.len() + self.scales.len() * 4
    }
}

/// Least-recently-used chunk cache under a byte budget.
struct PagedCache {
    budget: usize,
    used: usize,
    order: VecDeque<(usize, usize)>,
    chunks: HashMap<(usize, usize), Arc<Chunk>>,
}

impl PagedCache {
    fn get(&mut self, key: (usize, usize)) -> Option<Arc<Chunk>> {
        let chunk = self.chunks.get(&key)?.clone();
        if let Some(pos) = self.order.iter().position(|k| *k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key);
        Some(chunk)
    }

    fn insert(&mut self, key: (usize, usize), chunk: Arc<Chunk>) {
        if self.chunks.contains_key(&key) {
            return;
        }
        self.used += chunk.bytes();
        self.chunks.insert(key, chunk);
        self.order.push_back(key);
        while self.used > self.budget && self.order.len() > 1 {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(dropped) = self.chunks.remove(&old) {
                self.used -= dropped.bytes();
            }
        }
    }
}

pub(crate) struct MappedImage {
    map: memmap2::Mmap,
    geo: Geo,
    dim: usize,
    bit_width: usize,
    n_vectors: usize,
    /// Committed whole blocks: units in the file.
    n_full_blocks: usize,
    /// The `n % 32` rows the commit header carries, as
    /// `(codes[row_bytes], scale f32 le, ids)` records.
    tail: Vec<u8>,
    tail_row: usize,
    /// Redo ops the loaded header carries, per block: absolute writes of
    /// `(slot, payload)`, applied over the unit's bytes.
    ops: Vec<(usize, Vec<(usize, Vec<u8>)>)>,
    tqplus_shift: Vec<f32>,
    tqplus_scale: Vec<f32>,
    generation: u64,
    src: String,
    cache: Mutex<PagedCache>,
}

impl std::fmt::Debug for MappedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedImage")
            .field("src", &self.src)
            .field("dim", &self.dim)
            .field("bit_width", &self.bit_width)
            .field("n_vectors", &self.n_vectors)
            .field("generation", &self.generation)
            .finish()
    }
}

impl MappedImage {
    /// Map `path` and parse its superblock and commit headers; the block
    /// units stay on their pages until a scan needs them.
    /// # Safety
    ///
    /// `path` and its backing storage must remain unchanged and untruncated
    /// for the lifetime of the returned mapping and all of its clones.
    pub(crate) unsafe fn open(path: &Path, cache_bytes: usize) -> io::Result<Self> {
        if !crate::io_v7::is_v7(path) {
            return Err(crate::io::legacy_format_error(path));
        }
        let file = std::fs::File::open(path)?;
        // SAFETY: the caller guarantees that the file and its backing storage
        // remain unchanged and untruncated for the mapping's lifetime.
        let map = unsafe { memmap2::Mmap::map(&file)? };
        let src = path.display().to_string();
        let parsed: ParsedImage = parse_image(&map[..], 0, &src)?;
        if parsed.kind != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "mapped serving covers positional images (kind 0) only",
            ));
        }
        Ok(MappedImage {
            map,
            geo: parsed.geo,
            dim: parsed.dim,
            bit_width: parsed.bit_width,
            n_vectors: parsed.n_vectors,
            n_full_blocks: parsed.n_blocks,
            tail: parsed.tail,
            tail_row: parsed.tail_row,
            ops: parsed.ops,
            tqplus_shift: parsed.tqplus_shift,
            tqplus_scale: parsed.tqplus_scale,
            generation: parsed.gen,
            src,
            cache: Mutex::new(PagedCache {
                budget: cache_bytes.max(1),
                used: 0,
                order: VecDeque::new(),
                chunks: HashMap::new(),
            }),
        })
    }

    pub(crate) fn dim(&self) -> usize {
        self.dim
    }

    pub(crate) fn bit_width(&self) -> usize {
        self.bit_width
    }

    pub(crate) fn n_vectors(&self) -> usize {
        self.n_vectors
    }

    pub(crate) fn calibration(&self) -> (Vec<f32>, Vec<f32>) {
        (self.tqplus_shift.clone(), self.tqplus_scale.clone())
    }

    fn bad(&self, message: impl std::fmt::Display) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {message}", self.src),
        )
    }

    /// The chunk of `live` rows from `base` (`base` on a block boundary),
    /// from the cache or assembled from the mapped pages.
    pub(crate) fn chunk(&self, base: usize, live: usize) -> io::Result<Arc<Chunk>> {
        debug_assert_eq!(base % BLOCK, 0);
        let key = (base, live);
        if let Some(chunk) = self.cache.lock().expect("chunk cache poisoned").get(key) {
            return Ok(chunk);
        }
        let chunk = Arc::new(self.assemble(base, live)?);
        self.cache
            .lock()
            .expect("chunk cache poisoned")
            .insert(key, Arc::clone(&chunk));
        Ok(chunk)
    }

    /// Gather the blocks' code bytes and scales from their units (and
    /// the header tail for the last, partial block), apply the pending
    /// redo ops, and transform into the kernel's native layout.
    fn assemble(&self, base: usize, live: usize) -> io::Result<Chunk> {
        let row_bytes = self.geo.row_bytes();
        let block_bytes = BLOCK * row_bytes;
        let first_block = base / BLOCK;
        let chunk_blocks = live.div_ceil(BLOCK);
        let mut codes = vec![0u8; chunk_blocks * block_bytes];
        let mut scales = Vec::with_capacity(live);
        for i in 0..chunk_blocks {
            let b = first_block + i;
            let out = &mut codes[i * block_bytes..(i + 1) * block_bytes];
            if b < self.n_full_blocks {
                let at = self.geo.unit_at(b);
                let unit = self
                    .map
                    .get(at..at + self.geo.unit_len())
                    .ok_or_else(|| self.bad(format!("truncated block unit {b}")))?;
                out.copy_from_slice(&unit[..block_bytes]);
                for lane in 0..BLOCK {
                    let so = block_bytes + lane * 4;
                    let v = f32::from_le_bytes(unit[so..so + 4].try_into().expect("4 bytes"));
                    if !v.is_finite() || !(0.0..=crate::io::MAX_VECTOR_SCALE).contains(&v) {
                        return Err(self.bad(format!("invalid per-vector scale in block {b}")));
                    }
                    scales.push(v);
                }
                if let Some((_, ops)) = self.ops.iter().find(|(block, _)| *block == b) {
                    for (slot, payload) in ops {
                        let lane = slot % BLOCK;
                        for g in 0..row_bytes {
                            out[g * BLOCK + lane] = payload[g];
                        }
                        let v = f32::from_le_bytes(
                            payload[row_bytes..row_bytes + 4]
                                .try_into()
                                .expect("4 bytes"),
                        );
                        if !v.is_finite() || !(0.0..=crate::io::MAX_VECTOR_SCALE).contains(&v) {
                            return Err(self.bad("invalid per-vector scale in a pending op"));
                        }
                        scales[i * BLOCK + lane] = v;
                    }
                }
            } else {
                // The partial last block: its rows ride the commit header.
                let n_tail = self.n_vectors % BLOCK;
                for k in 0..n_tail {
                    let row = self
                        .tail
                        .get(k * self.tail_row..(k + 1) * self.tail_row)
                        .ok_or_else(|| self.bad("truncated commit tail"))?;
                    for g in 0..row_bytes {
                        out[g * BLOCK + k] = row[g];
                    }
                    let v = f32::from_le_bytes(
                        row[row_bytes..row_bytes + 4].try_into().expect("4 bytes"),
                    );
                    if !v.is_finite() || !(0.0..=crate::io::MAX_VECTOR_SCALE).contains(&v) {
                        return Err(self.bad("invalid per-vector scale in the commit tail"));
                    }
                    scales.push(v);
                }
            }
        }
        scales.truncate(live);
        if scales.len() != live {
            return Err(self.bad(format!(
                "chunk at {base} holds {} scales for {live} rows",
                scales.len()
            )));
        }
        pack::apply_native_transform(&mut codes, self.bit_width, row_bytes);
        Ok(Chunk { codes, scales })
    }
}

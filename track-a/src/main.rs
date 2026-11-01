//! Track A: Ingest. For every (UTC hour, endpoint): count, nearest-rank p50/p95/p99,
//! 5xx count and floor parts-per-million.
//!
//! 1. Hand out 64 KB chunks of the log to the threads, each read with `pread` into
//!    the thread's one buffer. A chunk owns the lines that start in it.
//! 2. Each thread parses its lines into 4-byte records `endpoint << 21 | latency`,
//!    written into blocks it claims from its hour's mapping. Endpoints get global ids from one
//!    lock-free table shared by all threads, by hash alone. Larger latencies and 5xx lines go to small side
//!    lists of the hour. Timestamps are never fully parsed: only the date (cached,
//!    since consecutive lines share it) and the two hour digits matter.
//! 3. One task per hour, in cache: a counting sort of the hour's records by endpoint
//!    rank (names sorted bytewise, which is the output order), three nested selections
//!    per group, and the hour's lines formatted. Finished hours are written in order
//!    and each hour's records are freed as soon as they are sorted.

mod mmap;

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Barrier, Mutex, OnceLock};
use std::thread::{self, available_parallelism};

/// Every timestamp is in the 72 hours from 2026-10-30T18:00Z (spec section 2).
const HOURS: usize = 72;
/// Absolute hour (since the Unix epoch) of the span's first hour.
const BASE: i64 = days_from_civil(2026, 10, 30) * 24 + 18;
/// Records pack the endpoint id above a 21-bit latency; larger latencies go to `big`.
const LAT_BITS: u32 = 21;
const LAT_MASK: u32 = (1 << LAT_BITS) - 1;
const MAX_ENDPOINTS: usize = 1 << (32 - LAT_BITS);
/// Records are claimed from their hour's mapping in blocks of 1,024 (one 4 KB page).
const BLOCK: usize = 1 << 9;

/// One hour's records from every thread, in one mapping reserved at the start. A
/// thread claims a block of BLOCK records with one atomic add and fills it alone.
/// Once the hour is summarized the whole mapping goes back at once: one munmap, where
/// freeing each thread's own blocks took about 860, each a TLB shootdown on every
/// core (about 3.5 ms on the box). Pages never written cost nothing.
struct Records {
    base: *mut u32,
    cap: usize,
    next: AtomicUsize,
}

// SAFETY: threads share a mapping only through `claim`, which hands each block to
// exactly one thread, and `get`, which runs after every writer has been joined.
unsafe impl Sync for Records {}

impl Records {
    /// Room for `cap` records.
    fn new(cap: usize) -> Self {
        let base = mmap::reserve(cap * 4) as *mut u32;
        Records {
            base,
            cap,
            next: AtomicUsize::new(0),
        }
    }

    /// A fresh block for the calling thread alone: its start and its slots.
    fn claim(&self) -> (usize, &mut [u32]) {
        let start = self.next.fetch_add(BLOCK, Relaxed);
        assert!(start + BLOCK <= self.cap, "hour mapping full");
        // SAFETY: blocks never overlap (each start comes from one atomic add) and lie
        // inside the mapping, so the caller holds the only reference to these slots.
        (start, unsafe {
            std::slice::from_raw_parts_mut(self.base.add(start), BLOCK)
        })
    }

    /// The records `start..start + len`, once the threads that wrote them are joined.
    fn get(&self, start: usize, len: usize) -> &[u32] {
        assert!(start + len <= self.cap);
        // SAFETY: inside the mapping, and nothing writes it any more.
        unsafe { std::slice::from_raw_parts(self.base.add(start), len) }
    }
}

/// One thread's lines of one hour.
#[derive(Default)]
struct Hour {
    blocks: Vec<(usize, usize)>, // (start, length) of each block it filled
    big: Vec<(u32, u32)>,        // (endpoint, latency) for latencies of 2^21 and up
    errors: Vec<u16>,            // the endpoint of every 5xx line
}

/// Summary lines are written in hour order as hours finish, in any order.
struct Writer {
    next: usize,
    done: Vec<Option<Vec<u8>>>,
    busy: bool, // a thread is writing hours, outside the lock
}

fn main() {
    mmap::small_pages();
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().unwrap_or_else(|| "/data".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/out".into()));
    let input = File::open(data.join("requests.log")).expect("open requests.log");
    let len = input.metadata().expect("stat requests.log").len() as usize;
    let threads = available_parallelism().map_or(1, |n| n.get().min(12));

    // Many small chunks handed out on demand, so a slow core never holds up the rest.
    let next = AtomicUsize::new(0);
    let registry = Registry::new();
    // A line has at least 40 bytes, so no hour can hold more than len / 39 of them,
    // plus one partly filled block per thread.
    let cap = len / 39 + (threads + 1) * BLOCK;
    let records: Vec<Records> = (0..HOURS).map(|_| Records::new(cap)).collect();
    let by_hour: Vec<Mutex<Vec<Hour>>> = (0..HOURS).map(|_| Mutex::default()).collect();
    let file = File::create(out.join("summary.txt")).expect("create summary.txt");
    let writer = Mutex::new(Writer {
        next: 0,
        done: vec![None; HOURS],
        busy: false,
    });
    // One set of threads for both phases: starting a second set, and each thread's
    // exit unmapping its stack (a TLB shootdown on every core), costs more than the
    // barrier between the phases.
    let barrier = Barrier::new(threads);
    let sorted: OnceLock<Sorted> = OnceLock::new();
    let next_hour = AtomicUsize::new(0);
    thread::scope(|s| {
        for _ in 0..threads {
            let (input, next, registry, records) = (&input, &next, &registry, &records);
            let (by_hour, writer, barrier, sorted) = (&by_hour, &writer, &barrier, &sorted);
            let file = &file;
            let next_hour = &next_hour;
            s.spawn(move || {
                let hours = parse(input, next, registry, records);
                for (slot, hour) in by_hour.iter().zip(hours) {
                    slot.lock().unwrap().push(hour);
                }
                if barrier.wait().is_leader() {
                    let count = registry.count.load(Relaxed) as usize;
                    let names: Vec<&[u8]> = (registry.names[..count].iter())
                        .map(|n| n.get().unwrap().as_slice())
                        .collect();
                    // Endpoint ids in bytewise name order, which is exactly the spec's order.
                    let mut order: Vec<usize> = (0..count).collect();
                    order.sort_unstable_by(|&a, &b| names[a].cmp(names[b]));
                    sorted.set((names, order)).unwrap();
                }
                barrier.wait();
                let sorted = sorted.get().unwrap();
                let mut scratch = Scratch::new(sorted.0.len());
                while let h @ 0..HOURS = next_hour.fetch_add(1, Relaxed) {
                    let parts = std::mem::take(&mut *by_hour[h].lock().unwrap());
                    let lines = scratch.summarize(parts, &records[h], BASE + h as i64, sorted);
                    let mut w = writer.lock().unwrap();
                    w.done[h] = Some(lines);
                    if w.busy {
                        continue; // the writing thread will find it
                    }
                    // Write every hour that is next in order, outside the lock, so the
                    // threads depositing hours meanwhile never wait for a write.
                    w.busy = true;
                    loop {
                        let i = w.next;
                        let Some(lines) = w.done.get_mut(i).and_then(Option::take) else {
                            break;
                        };
                        w.next += 1;
                        drop(w);
                        (&*file).write_all(&lines).expect("write summary.txt");
                        w = writer.lock().unwrap();
                    }
                    w.busy = false;
                }
            });
        }
    });
}

/// The endpoint names by id, and the ids in bytewise name order (the output order).
type Sorted<'a> = (Vec<&'a [u8]>, Vec<usize>);

/// One summarizing thread's buffers, reused from hour to hour.
struct Scratch {
    count: Vec<u32>,
    errors: Vec<u32>,
    end: Vec<u32>,
    latencies: Vec<u32>,
}

impl Scratch {
    fn new(n: usize) -> Self {
        Scratch {
            count: vec![0; n],
            errors: vec![0; n],
            end: vec![0; n],
            latencies: Vec::new(),
        }
    }

    /// The summary lines of one hour, from every thread's records of that hour.
    /// `sorted` holds the endpoint names by id and the ids in name order.
    fn summarize(&mut self, parts: Vec<Hour>, recs: &Records, h: i64, sorted: &Sorted) -> Vec<u8> {
        let (names, order) = sorted;
        let prefix = hour_prefix(h);
        let (count, errors, end) = (&mut self.count, &mut self.errors, &mut self.end);
        count.fill(0);
        errors.fill(0);
        for p in &parts {
            for &(start, len) in &p.blocks {
                for &r in recs.get(start, len) {
                    count[(r >> LAT_BITS) as usize] += 1;
                }
            }
            for &(e, _) in &p.big {
                count[e as usize] += 1;
            }
            for &e in &p.errors {
                errors[e as usize] += 1;
            }
        }
        // Counting sort by rank: `end[e]` starts as endpoint e's first slot.
        let mut total = 0;
        for &e in order {
            end[e] = total;
            total += count[e];
        }
        let lat = &mut self.latencies;
        lat.clear();
        lat.resize(total as usize, 0);
        for p in parts {
            for &(start, len) in &p.blocks {
                for &r in recs.get(start, len) {
                    let at = &mut end[(r >> LAT_BITS) as usize];
                    lat[*at as usize] = r & LAT_MASK;
                    *at += 1;
                }
            }
            for (e, l) in p.big {
                let at = &mut end[e as usize];
                lat[*at as usize] = l;
                *at += 1;
            }
        }
        // SAFETY: this hour's task is the only user of its mapping, and its records are
        // all copied out: nothing reads them again.
        unsafe { mmap::release(recs.base as *mut u8, recs.cap * 4) };

        let mut out = Vec::with_capacity(1 << 18);
        for &e in order {
            let n = count[e];
            if n == 0 {
                continue;
            }
            let hi = end[e] as usize;
            let [p50, p95, p99] = percentiles(&mut lat[hi - n as usize..hi]);
            out.extend_from_slice(&prefix);
            out.extend_from_slice(names[e]);
            let errs = errors[e];
            let ppm = (errs as u64 * 1_000_000 / n as u64) as u32;
            for v in [n, p50, p95, p99, errs, ppm] {
                out.push(b' ');
                push_num(&mut out, v);
            }
            out.push(b'\n');
        }
        out
    }
}

/// Bytes of the file per chunk: a chunk owns the lines that start in it. Small enough
/// that a thread's buffer stays in its core's L2 between the read and the parse.
const CHUNK: usize = 1 << 16;
/// Read past a chunk's end, so its last line is whole: longer than any line, which is
/// at most 27 + 7 + 125 + 3 + 10 bytes plus five separators.
const SLACK: usize = 256;

/// Parses whole lines of the grammar in the spec:
/// `2026-10-30T18:07:18.569411Z GET /api/v2/products 200 8177`
///
/// The input is read with `pread` into one reused buffer per thread rather than
/// mapped: a mapping costs a page-table entry per 4 KB page to set up, and to tear
/// down serially when the process exits.
fn parse(input: &File, next: &AtomicUsize, reg: &Registry, recs: &[Records]) -> Vec<Hour> {
    let len = input.metadata().unwrap().len() as usize;
    let mut buf = vec![0u8; CHUNK + SLACK + 1];
    let mut hours: Vec<Hour> = (0..HOURS).map(|_| Hour::default()).collect();
    // Per hour: the start of the block being filled, its slots, and how many are used.
    let mut open: Vec<(usize, &mut [u32], usize)> =
        (0..HOURS).map(|_| (0, &mut [][..], 0)).collect();
    let (mut date, mut date_hours) = ((u64::MAX, 0u16), 0i64);
    loop {
        let start = next.fetch_add(1, Relaxed) * CHUNK;
        if start >= len {
            break;
        }
        // Also read the byte before the chunk: the first line starts after its first LF.
        let from = start.saturating_sub(1);
        let chunk = &mut buf[..(CHUNK + SLACK + 1).min(len - from)];
        input.read_exact_at(chunk, from as u64).unwrap();
        let chunk = &*chunk;
        let first_lf = chunk.iter().position(|&b| b == b'\n');
        let mut i = if start == 0 {
            0
        } else {
            first_lf.map_or(chunk.len(), |p| p + 1)
        };
        let own = start + CHUNK - from; // lines starting before this are ours
        while i < own.min(chunk.len()) {
            let line = &chunk[i..];
            // Lines arrive in time-ordered shards, so the date rarely changes between lines.
            let d = (word(line, 0), u16::from_le_bytes([line[8], line[9]]));
            if d != date {
                date = d;
                date_hours =
                    days_from_civil(num(&line[0..4]), num(&line[5..7]), num(&line[8..10])) * 24;
            }
            let hour = date_hours + num(&line[11..13]) - BASE;
            assert!((0..HOURS as i64).contains(&hour), "hour out of span");
            let hour = hour as usize;
            // The method starts at byte 28 and is at most 7 bytes, so the space after it
            // is in the next word: no branch on which method it is.
            let ep_start = 29 + first_byte(word(line, 28), SPACES);
            let (ep_len, h) = endpoint(line, ep_start);
            let ep_end = ep_start + ep_len;
            let id = reg.id(&line[ep_start..ep_end], mix(h, ep_len as u64) | 1);
            let (latency, j) = latency(line, ep_end + 5);
            i += j + 1;

            if line[ep_end + 1] == b'5' {
                hours[hour].errors.push(id as u16);
            }
            if latency > LAT_MASK {
                hours[hour].big.push((id, latency));
                continue;
            }
            let (start, block, used) = &mut open[hour];
            if *used == block.len() {
                if *used > 0 {
                    hours[hour].blocks.push((*start, *used));
                }
                (*start, *block) = recs[hour].claim();
                *used = 0;
            }
            block[*used] = id << LAT_BITS | latency;
            *used += 1;
        }
    }
    for (hour, (start, _, used)) in hours.iter_mut().zip(open) {
        if used > 0 {
            hour.blocks.push((start, used));
        }
    }
    hours
}

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;
const SPACES: u64 = 0x2020_2020_2020_2020;
const NEWLINES: u64 = 0x0a0a_0a0a_0a0a_0a0a;

/// Eight bytes at `i` as a little-endian word, zero-padded past the end of `b`.
#[inline]
fn word(b: &[u8], i: usize) -> u64 {
    match b.get(i..i + 8) {
        Some(w) => u64::from_le_bytes(w.try_into().unwrap()),
        None => {
            let mut w = [0u8; 8];
            let tail = &b[i.min(b.len())..];
            w[..tail.len()].copy_from_slice(tail);
            u64::from_le_bytes(w)
        }
    }
}

/// The endpoint starting at `b[at]`: its length and the hash of its bytes. With AVX2,
/// one compare of the 64 bytes at `at` finds the space after any name of up to 63
/// bytes (about 95% of lines), and the name is hashed in a fixed run of vector steps:
/// no branch depends on its length. Longer names, and builds without AVX2, are
/// scanned and hashed a word at a time. Which form hashes a name depends only on its
/// length, so a name always gets the same hash.
#[inline]
fn endpoint(b: &[u8], at: usize) -> (usize, u64) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    {
        let mut pad = [0u8; 64]; // near the end of the input: zeros are not spaces
        let block: &[u8; 64] = match b.get(at..at + 64) {
            Some(w) => w.try_into().unwrap(),
            None => {
                pad[..b.len() - at].copy_from_slice(&b[at..]);
                &pad
            }
        };
        if let Some(found) = short_endpoint(block) {
            return found;
        }
    }
    let (mut k, mut h) = (at, 0u64);
    loop {
        let w = word(b, k);
        let n = first_byte(w, SPACES);
        if n < 8 {
            return (k + n - at, mix(h, w & ((1u64 << (n * 8)) - 1)));
        }
        h = mix(h, w);
        k += 8;
    }
}

/// The length and hash of a name of up to 63 bytes ending at the first space in
/// `block`, or None if `block` has no space. The hash is one XXH3 accumulate step
/// over the eight words of the zero-padded name (each word xored with a key, the
/// product of its halves, plus its neighbour word), the lanes summed, then two
/// folded multiplies.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
fn short_endpoint(block: &[u8; 64]) -> Option<(usize, u64)> {
    use std::arch::x86_64::*;
    // SAFETY: AVX2 is enabled at compile time, both unaligned loads read inside the 64
    // bytes of `block`, and the transmute reads a 32-byte vector as four u64s, a type
    // of the same size for which every bit pattern is valid.
    unsafe {
        let v = [0, 32].map(|k| _mm256_loadu_si256(block.as_ptr().add(k) as *const __m256i));
        let sp = _mm256_set1_epi8(b' ' as i8);
        let spaces = v.map(|v| _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, sp)) as u32 as u64);
        let mask = spaces[0] | spaces[1] << 32;
        if mask == 0 {
            return None;
        }
        let len = mask.trailing_zeros() as usize;
        let lens = _mm256_set1_epi8(len as i8);
        let first = _mm256_setr_epi8(
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31,
        );
        let at = [first, _mm256_add_epi8(first, _mm256_set1_epi8(32))];
        // A different key for each word, so names whose words are permutations of each
        // other hash apart: one constant xored with the words' byte positions.
        let keys = at.map(|i| _mm256_xor_si256(i, _mm256_set1_epi64x(0x1cad_21f7_2c81_017c)));
        let mut acc = _mm256_setzero_si256();
        for k in 0..2 {
            let d = _mm256_and_si256(v[k], _mm256_cmpgt_epi8(lens, at[k])); // bytes below len
            let x = _mm256_xor_si256(d, keys[k]);
            let product = _mm256_mul_epu32(x, _mm256_srli_epi64::<32>(x));
            let swapped = _mm256_shuffle_epi32::<0b01_00_11_10>(d);
            acc = _mm256_add_epi64(acc, _mm256_add_epi64(product, swapped));
        }
        let [a, b, c, d]: [u64; 4] = std::mem::transmute(acc);
        Some((len, mix(mix(a, b), c ^ d.rotate_left(32))))
    }
}

/// Index of the first byte of `w` equal to the byte repeated in `pattern` (8 if none).
#[inline]
fn first_byte(w: u64, pattern: u64) -> usize {
    let x = w ^ pattern;
    (x.wrapping_sub(ONES) & !x & HIGHS).trailing_zeros() as usize / 8
}

/// The decimal latency at `p` and the position of the newline after it. Up to seven
/// digits (all but the rarest values) are found and converted with word arithmetic:
/// left-align the digits, then combine pairs, quads and octets by multiplication.
#[inline]
fn latency(line: &[u8], p: usize) -> (u32, usize) {
    let w = word(line, p);
    let n = first_byte(w, NEWLINES);
    // n == 8 means no newline in this word (eight or more digits): take the loop.
    if (1..8).contains(&n) {
        let digits = (w << (8 * (8 - n))) & 0x0f0f_0f0f_0f0f_0f0f; // leading zero bytes
        let v = digits.wrapping_mul(10 * (1 << 8) + 1) >> 8 & 0x00ff_00ff_00ff_00ff;
        let v = v.wrapping_mul(100 * (1 << 16) + 1) >> 16 & 0x0000_ffff_0000_ffff;
        let v = v.wrapping_mul(10000 * (1 << 32) + 1) >> 32;
        return (v as u32, p + n);
    }
    let mut j = p;
    let mut v = 0u32;
    while line[j] != b'\n' {
        v = v * 10 + (line[j] - b'0') as u32;
        j += 1;
    }
    (v, j)
}

/// One step of the endpoint hash per 8-byte word, and a last one over the length: the folded multiply of foldhash and
/// wyhash. The halves of the full 128-bit product are xored, so every input bit
/// reaches every output bit. The FxHash step this replaced, a plain 64-bit multiply,
/// only carries a change upward: names differing in their 8th and 9th bytes collided
/// in all 64 bits about once in 32 (track C met such a pair on seed 6). The fold
/// already spreads every bit both ways, so no separate finalizer is needed: on 4.9
/// million near-duplicate names (one or two bytes changed) neither form collided.
#[inline]
fn mix(h: u64, w: u64) -> u64 {
    let m = (h ^ w) as u128 * 0x9e37_79b9_7f4a_7c15;
    m as u64 ^ (m >> 64) as u64
}

/// Slots are `tag | (id + 1)`: the id in the low bits, hash bits 11..63 above it.
const ID_MASK: u64 = MAX_ENDPOINTS as u64 - 1;

/// Endpoint ids, in one lock-free table shared by all parsing threads and keyed by hash
/// alone: bytes are never compared. A slot is one word, the hash's top 53 bits above
/// `id + 1`; 0 is an empty slot, and a tag with no id is a claim in progress. The
/// 4,096 slots take 32 KB, read by every core and written once per name (1,842
/// times), so threads never wait on a lock. With the 12 bits that pick the first slot,
/// a name is identified by 61 hash bits (bit 63 is forced, so a tag is never 0): two
/// of the 1,842 names sharing them has odds of about 1e-12 per run.
struct Registry {
    slots: Vec<AtomicU64>,
    names: Vec<OnceLock<Vec<u8>>>, // by id
    count: AtomicU32,
}

impl Registry {
    fn new() -> Self {
        Registry {
            slots: (0..2 * MAX_ENDPOINTS).map(|_| AtomicU64::new(0)).collect(),
            names: (0..MAX_ENDPOINTS).map(|_| OnceLock::new()).collect(),
            count: AtomicU32::new(0),
        }
    }

    #[inline]
    fn id(&self, name: &[u8], h: u64) -> u32 {
        let mask = self.slots.len() - 1;
        let tag = (h | 1 << 63) & !ID_MASK;
        let mut i = h as usize & mask;
        loop {
            let slot = self.slots[i].load(Acquire);
            if slot & !ID_MASK == tag && slot != tag {
                return (slot & ID_MASK) as u32 - 1;
            }
            if slot == 0 {
                if let Some(id) = self.claim(i, tag, name) {
                    return id;
                }
            } else if slot == tag {
                std::hint::spin_loop(); // another thread is adding this very name
            } else {
                i = (i + 1) & mask;
            }
        }
    }

    /// Adds `name` in empty slot `i`, unless another thread takes the slot first. The
    /// name is stored before the slot is published, so whoever finds its id finds it.
    #[cold]
    #[inline(never)]
    fn claim(&self, i: usize, tag: u64, name: &[u8]) -> Option<u32> {
        let slot = &self.slots[i];
        slot.compare_exchange(0, tag, Acquire, Relaxed).ok()?;
        let id = self.count.fetch_add(1, Relaxed);
        assert!((id as u64) < ID_MASK, "too many distinct endpoints");
        self.names[id as usize].set(name.to_vec()).unwrap();
        slot.store(tag | (id as u64 + 1), Release);
        Some(id)
    }
}

/// Nearest-rank p50, p95 and p99: the values at 1-indexed rank ceil(p * n / 100).
/// Three selections instead of a sort, each on the part right of the last one: the
/// whole slice, then its upper half, then its top twentieth (about 1.55 slice lengths
/// of work, where selecting p99 first and nesting leftwards costs about 2.9).
fn percentiles(v: &mut [u32]) -> [u32; 3] {
    let n = v.len();
    let idx = |p: usize| (p * n).div_ceil(100) - 1;
    let (i50, i95, i99) = (idx(50), idx(95), idx(99));
    if n <= 32 {
        // Most groups are this small: one small sort beats three selections.
        v.sort_unstable();
        return [v[i50], v[i95], v[i99]];
    }
    // The value of rank `i` among `v[lo..]`, all at least as large as those before.
    let mut select = |lo: usize, i: usize| *v[lo..].select_nth_unstable(i - lo).1;
    let p50 = select(0, i50);
    let p95 = if i95 == i50 {
        p50
    } else {
        select(i50 + 1, i95)
    };
    let p99 = if i99 == i95 {
        p95
    } else {
        select(i95 + 1, i99)
    };
    [p50, p95, p99]
}

/// `YYYY-MM-DDTHH:00:00Z ` for an absolute hour count since the Unix epoch.
fn hour_prefix(abs_hour: i64) -> [u8; 21] {
    let (y, m, d) = civil_from_days(abs_hour.div_euclid(24));
    let h = abs_hour.rem_euclid(24) as u32;
    let mut p = *b"0000-00-00T00:00:00Z ";
    for (at, v, width) in [(0, y as u32, 4), (5, m, 2), (8, d, 2), (11, h, 2)] {
        let mut v = v;
        for k in (0..width).rev() {
            p[at + k] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    p
}

fn num(digits: &[u8]) -> i64 {
    digits
        .iter()
        .fold(0, |acc, &b| acc * 10 + (b - b'0') as i64)
}

fn push_num(out: &mut Vec<u8>, mut v: u32) {
    let mut buf = [0u8; 10];
    let digits = v.checked_ilog10().unwrap_or(0) as usize + 1;
    for k in (0..digits).rev() {
        buf[k] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out.extend_from_slice(&buf[..digits]);
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The inverse of `days_from_civil`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

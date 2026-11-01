//! Track A: Ingest. For every (UTC hour, endpoint): count, nearest-rank p50/p95/p99,
//! 5xx count and floor parts-per-million.
//!
//! 1. Hand out 128 KB chunks of the log to the threads, each read with `pread` into
//!    the thread's one buffer. A chunk owns the lines that start in it.
//! 2. Each thread parses its lines into 4-byte records `endpoint << 21 | latency`,
//!    appended to one block list per hour. Endpoints get global ids from a shared
//!    registry the first time a thread meets them; after that a thread-local table
//!    answers by 64-bit hash alone. Larger latencies and 5xx lines go to small side
//!    lists of the hour. Timestamps are never fully parsed: only the date (cached,
//!    since consecutive lines share it) and the two hour digits matter.
//! 3. One task per hour, in cache: a counting sort of the hour's records by endpoint
//!    rank (names sorted bytewise, which is the output order), three nested selections
//!    per group, and the hour's lines formatted. Finished hours are written in order
//!    and each hour's records are freed as soon as they are sorted.

mod mmap;

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::thread;

/// Every timestamp is in the 72 hours from 2026-10-30T18:00Z (spec section 2).
const HOURS: usize = 72;
/// Records pack the endpoint id above a 21-bit latency; larger latencies go to `big`.
const LAT_BITS: u32 = 21;
const LAT_MASK: u32 = (1 << LAT_BITS) - 1;
const MAX_ENDPOINTS: usize = 1 << (32 - LAT_BITS);
/// Records are kept in 256 KB blocks: above malloc's mmap threshold, so each block
/// is its own mapping, charged only for the pages written and returned to the
/// system when freed, instead of staying in a malloc arena.
const BLOCK: usize = 1 << 16;

/// One thread's lines of one hour.
#[derive(Default)]
struct Hour {
    blocks: Vec<Vec<u32>>,
    big: Vec<(u32, u32)>, // (endpoint, latency) for latencies of 2^21 and up
    errors: Vec<u16>,     // the endpoint of every 5xx line
}

/// Endpoint names by global id, shared by the parsing threads.
#[derive(Default)]
struct Registry {
    ids: HashMap<u64, u32>,
    names: Vec<Vec<u8>>,
}

/// Summary lines are written in hour order as hours finish, in any order.
struct Writer {
    next: usize,
    done: Vec<Option<Vec<u8>>>,
    file: File,
}

fn main() {
    mmap::small_pages();
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().unwrap_or_else(|| "/data".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/out".into()));
    let input = File::open(data.join("requests.log")).expect("open requests.log");
    let len = input.metadata().expect("stat requests.log").len() as usize;
    let threads = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(12);
    // Absolute hour (since the Unix epoch) of the span's first hour.
    let base = days_from_civil(2026, 10, 30) * 24 + 18;

    // Many small chunks handed out on demand, so a slow core never holds up the rest.
    let next = AtomicUsize::new(0);
    let registry = Mutex::new(Registry::default());
    let parts: Vec<Vec<Hour>> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (input, next, registry) = (&input, &next, &registry);
                s.spawn(move || parse(input, len, next, base, registry))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let names = registry.into_inner().unwrap().names;
    // Endpoint ids in bytewise name order, which is exactly the spec's order.
    let mut order: Vec<usize> = (0..names.len()).collect();
    order.sort_unstable_by(|&a, &b| names[a].cmp(&names[b]));

    let mut by_hour: Vec<Mutex<Vec<Hour>>> = (0..HOURS).map(|_| Mutex::default()).collect();
    for part in parts {
        for (slot, hour) in by_hour.iter_mut().zip(part) {
            slot.get_mut().unwrap().push(hour);
        }
    }
    let writer = Mutex::new(Writer {
        next: 0,
        done: vec![None; HOURS],
        file: File::create(out.join("summary.txt")).expect("create summary.txt"),
    });
    let next = AtomicUsize::new(0);
    thread::scope(|s| {
        for _ in 0..threads {
            let (names, order, by_hour, writer, next) = (&names, &order, &by_hour, &writer, &next);
            s.spawn(move || {
                let mut scratch = Scratch::new(names.len());
                loop {
                    let h = next.fetch_add(1, Relaxed);
                    if h >= HOURS {
                        break;
                    }
                    let parts = std::mem::take(&mut *by_hour[h].lock().unwrap());
                    let lines =
                        scratch.summarize(parts, hour_prefix(base + h as i64), names, order);
                    let mut w = writer.lock().unwrap();
                    let w = &mut *w;
                    w.done[h] = Some(lines);
                    while w.next < HOURS
                        && let Some(lines) = w.done[w.next].take()
                    {
                        w.file.write_all(&lines).expect("write summary.txt");
                        w.next += 1;
                    }
                }
            });
        }
    });
}

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
    fn summarize(
        &mut self,
        parts: Vec<Hour>,
        prefix: [u8; 21],
        names: &[Vec<u8>],
        order: &[usize],
    ) -> Vec<u8> {
        let (count, errors, end) = (&mut self.count, &mut self.errors, &mut self.end);
        count.fill(0);
        errors.fill(0);
        for p in &parts {
            for &r in p.blocks.iter().flatten() {
                count[(r >> LAT_BITS) as usize] += 1;
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
            for block in p.blocks {
                for r in block {
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

        let mut out = Vec::with_capacity(1 << 18);
        for &e in order {
            let n = count[e];
            if n == 0 {
                continue;
            }
            let hi = end[e] as usize;
            let [p50, p95, p99] = percentiles(&mut lat[hi - n as usize..hi]);
            out.extend_from_slice(&prefix);
            out.extend_from_slice(&names[e]);
            let errs = errors[e] as u64;
            for v in [
                n as u64,
                p50 as u64,
                p95 as u64,
                p99 as u64,
                errs,
                errs * 1_000_000 / n as u64,
            ] {
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
const CHUNK: usize = 1 << 17;
/// Read past a chunk's end, so its last line is whole: longer than any line, which is
/// at most 27 + 7 + 125 + 3 + 10 bytes plus five separators.
const SLACK: usize = 256;

/// Parses whole lines of the grammar in the spec:
/// `2026-10-30T18:07:18.569411Z GET /api/v2/products 200 8177`
///
/// The input is read with `pread` into one reused buffer per thread rather than
/// mapped: a mapping costs a page-table entry per 4 KB page to set up, and to tear
/// down serially when the process exits.
fn parse(
    input: &File,
    len: usize,
    next: &AtomicUsize,
    base: i64,
    registry: &Mutex<Registry>,
) -> Vec<Hour> {
    let mut buf = vec![0u8; CHUNK + SLACK + 1];
    let mut table = Endpoints {
        slots: vec![(0, 0); 2 * MAX_ENDPOINTS],
        registry,
    };
    let mut hours: Vec<Hour> = (0..HOURS).map(|_| Hour::default()).collect();
    let mut open: Vec<Vec<u32>> = vec![Vec::new(); HOURS];
    let (mut date, mut date_hours) = ((u64::MAX, 0u16), 0i64);
    loop {
        let start = next.fetch_add(1, Relaxed) * CHUNK;
        if start >= len {
            break;
        }
        // Also read the byte before the chunk: the first line starts after its first LF.
        let from = start.saturating_sub(1);
        let chunk = &mut buf[..(CHUNK + SLACK + 1).min(len - from)];
        input
            .read_exact_at(chunk, from as u64)
            .expect("read requests.log");
        let chunk = &*chunk;
        let mut i = if start == 0 {
            0
        } else {
            chunk
                .iter()
                .position(|&b| b == b'\n')
                .map_or(chunk.len(), |p| p + 1)
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
            let hour = date_hours + num(&line[11..13]) - base;
            assert!(
                (0..HOURS as i64).contains(&hour),
                "timestamp outside the 72-hour span"
            );
            let hour = hour as usize;
            // The method starts at byte 28 and is at most 7 bytes, so the space after it
            // is in the next word: no branch on which method it is.
            let ep_start = 29 + first_byte(word(line, 28), SPACES);
            let (ep_len, h) = endpoint(line, ep_start);
            let ep_end = ep_start + ep_len;
            let id = table.id(&line[ep_start..ep_end], mix(h, ep_len as u64) | 1);
            let (latency, j) = latency(line, ep_end + 5);
            i += j + 1;

            if line[ep_end + 1] == b'5' {
                hours[hour].errors.push(id as u16);
            }
            if latency > LAT_MASK {
                hours[hour].big.push((id, latency));
                continue;
            }
            let block = &mut open[hour];
            if block.len() == block.capacity() {
                let full = std::mem::replace(block, Vec::with_capacity(BLOCK));
                if !full.is_empty() {
                    hours[hour].blocks.push(full);
                }
            }
            block.push(id << LAT_BITS | latency);
        }
    }
    for (hour, block) in hours.iter_mut().zip(open) {
        if !block.is_empty() {
            hour.blocks.push(block);
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
    // SAFETY: AVX2 is enabled at compile time, and both unaligned loads read inside
    // the 64 bytes of `block`.
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
        let keys = [
            _mm256_setr_epi64x(
                0x1cad_21f7_2c81_017cu64 as i64,
                0xdb97_9083_e96d_d4deu64 as i64,
                0x1f67_b3b7_a4a4_4072u64 as i64,
                0x78e5_c0cc_4ee6_79cbu64 as i64,
            ),
            _mm256_setr_epi64x(
                0x8172_6f7d_c2b5_d2b8u64 as i64,
                0x3b9a_6d9e_4b6b_a1c5u64 as i64,
                0xf64d_5a5f_2b1f_0b8du64 as i64,
                0x9e6c_5a3e_7c4d_2f19u64 as i64,
            ),
        ];
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

/// A thread's endpoint table, keyed by a strong 64-bit hash of the name alone: bytes
/// are never compared on the hot path. Two of the 1,842 names sharing a hash has odds
/// of about 1e-13 per run. New names take a global id from the shared registry, which
/// does compare bytes, so a collision between names it sees is caught.
struct Endpoints<'a> {
    slots: Vec<(u64, u32)>, // (hash, id); hash 0 marks an empty slot
    registry: &'a Mutex<Registry>,
}

impl Endpoints<'_> {
    #[inline]
    fn id(&mut self, name: &[u8], h: u64) -> u32 {
        let mask = self.slots.len() - 1;
        let mut i = h as usize & mask;
        loop {
            let (sh, id) = self.slots[i];
            if sh == h {
                return id;
            }
            if sh == 0 {
                break;
            }
            i = (i + 1) & mask;
        }
        let mut r = self.registry.lock().unwrap();
        let id = match r.ids.get(&h) {
            Some(&id) => {
                assert!(
                    r.names[id as usize] == name,
                    "64-bit endpoint hash collision"
                );
                id
            }
            None => {
                let id = r.names.len() as u32;
                assert!((id as usize) < MAX_ENDPOINTS, "too many distinct endpoints");
                r.names.push(name.to_vec());
                r.ids.insert(h, id);
                id
            }
        };
        self.slots[i] = (h, id);
        id
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
    let p50 = *v.select_nth_unstable(i50).1;
    let p95 = if i95 == i50 {
        p50
    } else {
        *v[i50 + 1..].select_nth_unstable(i95 - i50 - 1).1
    };
    let p99 = if i99 == i95 {
        p95
    } else {
        *v[i95 + 1..].select_nth_unstable(i99 - i95 - 1).1
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

fn push_num(out: &mut Vec<u8>, mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
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

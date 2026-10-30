//! Track A: Ingest. For every (UTC hour, endpoint): count, nearest-rank p50/p95/p99,
//! 5xx count and floor parts-per-million.
//!
//! 1. mmap the log and cut it into one chunk per thread at line boundaries.
//! 2. Each thread parses its lines into compact (key, latency) records, interning
//!    endpoints in a thread-local table. Timestamps are never fully parsed: only the
//!    date (cached, since consecutive lines share it) and the two hour digits matter.
//! 3. The endpoint tables merge into one bytewise-sorted list, so the dense group id
//!    `hour * endpoints + rank` is already the output order.
//! 4. A parallel counting sort scatters every latency into its group's slice.
//! 5. Per group, `select_nth_unstable` finds the three ranks without a full sort,
//!    and threads format disjoint group ranges that are written out in order.

mod mmap;

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::thread;

/// Record key layout: endpoint (local id) in bits 0..16, hour in bits 16..31, 5xx in bit 31.
const EP_BITS: u32 = 16;
const HOUR_MASK: u32 = 0x7fff;
const ERR_BIT: u32 = 1 << 31;

/// Records are kept in blocks of this many, so the scatter can free each block as
/// soon as it is consumed and the records and the sorted copy never both peak.
const BLOCK: usize = 1 << 16;

/// One thread's parse result. Endpoint slices borrow from the mapped input.
struct Parsed {
    endpoints: Vec<&'static [u8]>,
    records: Vec<Vec<(u32, u32)>>,
    hour_min: u32,
    hour_max: u32,
}

fn main() {
    mmap::small_pages();
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().unwrap_or_else(|| "/data".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/out".into()));
    let input = mmap::map(&data.join("requests.log"));
    let threads = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(12);
    // Hours are stored relative to 2026-10-30T00:00Z; every timestamp is in the 72 h after 18:00.
    let base = days_from_civil(2026, 10, 30) * 24;

    // Many small chunks handed out on demand, so a slow core never holds up the
    // rest. Each thread still produces one result, keeping per-thread state small.
    let chunks = split_lines(input, (input.len() >> 21).max(threads));
    let next = AtomicUsize::new(0);
    let mut parts: Vec<Parsed> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (chunks, next) = (&chunks, &next);
                s.spawn(move || parse(|| chunks.get(next.fetch_add(1, Relaxed)).copied(), base))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Bytewise order of `&[u8]` is exactly the spec's endpoint order.
    let mut names: Vec<&[u8]> = parts
        .iter()
        .flat_map(|p| p.endpoints.iter().copied())
        .collect();
    names.sort_unstable();
    names.dedup();
    let n_ep = names.len();
    let hour_min = parts.iter().map(|p| p.hour_min).min().unwrap_or(0);
    let hour_max = parts.iter().map(|p| p.hour_max).max().unwrap_or(0);
    let groups = if n_ep == 0 {
        0
    } else {
        (hour_max - hour_min + 1) as usize * n_ep
    };
    assert!(groups < u32::MAX as usize);

    // Rewrite each record's key to its global group id, counting per thread.
    let errors: Vec<AtomicU32> = (0..groups).map(|_| AtomicU32::new(0)).collect();
    let mut cursors: Vec<Vec<u32>> = thread::scope(|s| {
        let (names, errors) = (&names, &errors);
        let handles: Vec<_> = parts
            .iter_mut()
            .map(|p| {
                s.spawn(move || {
                    let rank: Vec<usize> = p
                        .endpoints
                        .iter()
                        .map(|e| names.binary_search(e).unwrap())
                        .collect();
                    let mut count = vec![0u32; groups];
                    for rec in p.records.iter_mut().flatten() {
                        let key = rec.0;
                        let hour = ((key >> EP_BITS) & HOUR_MASK) - hour_min;
                        let g = hour as usize * n_ep + rank[(key & ((1 << EP_BITS) - 1)) as usize];
                        if key & ERR_BIT != 0 {
                            errors[g].fetch_add(1, Relaxed);
                        }
                        count[g] += 1;
                        rec.0 = g as u32;
                    }
                    count
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    std::fs::write("/out/summary.txt", b"").unwrap(); // TIMING PROBE
    std::process::exit(0);
    // Prefix sums: `starts[g]` is group g's first slot; each thread's counts become its
    // private write cursors inside every group, so the scatter needs no synchronisation.
    // In parallel over ranges of groups: each range's total first, then each range
    // turns its counts into cursors from its own starting offset.
    let ranges: Vec<(usize, usize)> = (0..threads)
        .map(|k| (groups * k / threads, groups * (k + 1) / threads))
        .collect();
    let range_totals: Vec<u32> = thread::scope(|s| {
        let handles: Vec<_> = ranges
            .iter()
            .map(|&(g0, g1)| {
                let cursors = &cursors;
                s.spawn(move || {
                    cursors
                        .iter()
                        .map(|c| c[g0..g1].iter().sum::<u32>())
                        .sum::<u32>()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut starts = vec![0u32; groups + 1];
    let total: u32 = range_totals.iter().sum();
    starts[groups] = total;
    let mut pieces: Vec<Vec<&mut [u32]>> = ranges.iter().map(|_| Vec::new()).collect();
    for c in cursors
        .iter_mut()
        .map(Vec::as_mut_slice)
        .chain([&mut starts[..groups]])
    {
        let mut rest = c;
        for (piece, &(g0, g1)) in pieces.iter_mut().zip(&ranges) {
            let (mine, tail) = rest.split_at_mut(g1 - g0);
            piece.push(mine);
            rest = tail;
        }
    }
    thread::scope(|s| {
        let mut offset = 0u32;
        for (mut piece, &range_total) in pieces.into_iter().zip(&range_totals) {
            let mut at = offset;
            offset += range_total;
            s.spawn(move || {
                let starts = piece.pop().unwrap();
                for (i, start) in starts.iter_mut().enumerate() {
                    *start = at;
                    for c in piece.iter_mut() {
                        let n = c[i];
                        c[i] = at;
                        at += n;
                    }
                }
            });
        }
    });

    // Zeroed memory: pages are charged only as the scatter first writes them.
    let mut latencies = vec![0u32; total as usize];
    let dst = Scatter(latencies.as_mut_ptr());
    thread::scope(|s| {
        for (p, mut cur) in parts.into_iter().zip(cursors) {
            let dst = &dst;
            s.spawn(move || {
                // Each block is dropped as soon as it is scattered.
                for (g, latency) in p.records.into_iter().flatten() {
                    let at = &mut cur[g as usize];
                    // SAFETY: the cursors give every (thread, group) pair a disjoint range
                    // inside `latencies`, sized by the counts above, so writes never alias
                    // and never go out of bounds.
                    unsafe { dst.put(*at as usize, latency) };
                    *at += 1;
                }
            });
        }
    });

    // Cut the groups into ranges of roughly equal record counts and format them in parallel.
    let per = (total as usize).div_ceil(threads).max(1);
    let mut cuts: Vec<usize> = (1..threads)
        .map(|k| {
            starts
                .partition_point(|&x| (x as usize) < k * per)
                .min(groups)
        })
        .collect();
    cuts.insert(0, 0);
    cuts.push(groups);
    cuts.dedup();

    // First the percentiles of every group and the byte length of every range, so
    // that each thread can then write its lines straight to their final offset in
    // small pieces, with no copy of the whole output in memory.
    let mut stats = vec![[0u32; 3]; groups];
    let lengths: Vec<usize> = thread::scope(|s| {
        let (mut rest, mut stats_rest) = (latencies.as_mut_slice(), stats.as_mut_slice());
        let mut handles = Vec::new();
        for w in cuts.windows(2) {
            let (g0, g1) = (w[0], w[1]);
            let (mine, tail) = rest.split_at_mut((starts[g1] - starts[g0]) as usize);
            let (my_stats, stats_tail) = stats_rest.split_at_mut(g1 - g0);
            (rest, stats_rest) = (tail, stats_tail);
            let (starts, names, errors) = (&starts, &names, &errors);
            handles.push(s.spawn(move || {
                let mut bytes = 0;
                for g in g0..g1 {
                    let (lo, hi) = (
                        (starts[g] - starts[g0]) as usize,
                        (starts[g + 1] - starts[g0]) as usize,
                    );
                    if lo < hi {
                        my_stats[g - g0] = percentiles(&mut mine[lo..hi]);
                        let line = fields(
                            (hi - lo) as u64,
                            my_stats[g - g0],
                            errors[g].load(Relaxed) as u64,
                        );
                        bytes += 21
                            + names[g % n_ep].len()
                            + line.iter().map(|&v| 1 + digits(v)).sum::<usize>()
                            + 1;
                    }
                }
                bytes
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    drop(latencies);

    let file = File::create(out.join("summary.txt")).expect("create summary.txt");
    thread::scope(|s| {
        let mut offset = 0u64;
        for (w, &len) in cuts.windows(2).zip(&lengths) {
            let (g0, g1) = (w[0], w[1]);
            let (file, starts, names, errors, stats) = (&file, &starts, &names, &errors, &stats);
            let mut at = offset;
            offset += len as u64;
            s.spawn(move || {
                const PIECE: usize = 1 << 16;
                let mut buf = Vec::with_capacity(PIECE + 512);
                let mut prefix = [0u8; 21];
                let mut prefix_hour = usize::MAX;
                for g in g0..g1 {
                    let count = (starts[g + 1] - starts[g]) as u64;
                    if count == 0 {
                        continue;
                    }
                    if g / n_ep != prefix_hour {
                        prefix_hour = g / n_ep;
                        prefix = hour_prefix(base + (hour_min as usize + prefix_hour) as i64);
                    }
                    buf.extend_from_slice(&prefix);
                    buf.extend_from_slice(names[g % n_ep]);
                    for v in fields(count, stats[g], errors[g].load(Relaxed) as u64) {
                        buf.push(b' ');
                        push_num(&mut buf, v);
                    }
                    buf.push(b'\n');
                    if buf.len() >= PIECE {
                        file.write_all_at(&buf, at).expect("write summary.txt");
                        at += buf.len() as u64;
                        buf.clear();
                    }
                }
                file.write_all_at(&buf, at).expect("write summary.txt");
            });
        }
    });
}

/// The six numbers after `hour endpoint`: count, p50, p95, p99, errors, error_ppm.
fn fields(count: u64, [p50, p95, p99]: [u32; 3], errs: u64) -> [u64; 6] {
    [
        count,
        p50 as u64,
        p95 as u64,
        p99 as u64,
        errs,
        errs * 1_000_000 / count,
    ]
}

fn digits(v: u64) -> usize {
    v.checked_ilog10().map_or(1, |d| d as usize + 1)
}

/// Raw output pointer shared by the scatter threads. Each thread writes only the slots
/// its cursors own (see the SAFETY note at the call site).
struct Scatter(*mut u32);

// SAFETY: the pointer is only used for the disjoint writes described above.
unsafe impl Sync for Scatter {}

impl Scatter {
    /// # Safety
    /// `i` must be in bounds and written by exactly one thread.
    unsafe fn put(&self, i: usize, v: u32) {
        unsafe { self.0.add(i).write(v) }
    }
}

/// Splits `data` into `n` chunks that each end just after a newline.
fn split_lines(data: &'static [u8], n: usize) -> Vec<&'static [u8]> {
    let mut chunks = Vec::with_capacity(n);
    let mut start = 0;
    for k in 1..=n {
        let mut end = data.len();
        if k < n {
            let guess = (data.len() * k / n).max(start);
            end = data[guess..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(data.len(), |p| guess + p + 1);
        }
        chunks.push(&data[start..end]);
        start = end;
    }
    chunks
}

/// Parses whole lines of the grammar in the spec:
/// `2026-10-30T18:07:18.569411Z GET /api/v2/products 200 8177`
fn parse(mut next_chunk: impl FnMut() -> Option<&'static [u8]>, base: i64) -> Parsed {
    let mut table = Endpoints::default();
    let mut records = Vec::new();
    let mut block = Vec::with_capacity(BLOCK);
    let (mut hour_min, mut hour_max) = (u32::MAX, 0);
    let (mut date, mut date_hours) = ((u64::MAX, 0u16), 0i64);
    while let Some(chunk) = next_chunk() {
        let mut i = 0;
        while i < chunk.len() {
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
                (0..=HOUR_MASK as i64).contains(&hour),
                "timestamp outside the supported span"
            );
            let hour = hour as u32;
            // The method starts at byte 28 and is at most 7 bytes, so the space after it
            // is in the next word: no branch on which method it is.
            let ep_start = 29 + first_byte(word(line, 28), SPACES);
            // Find the space after the endpoint eight bytes at a time, hashing each word
            // on the way, so the endpoint is scanned once for both jobs.
            let (mut k, mut h) = (ep_start, 0u64);
            let ep_end = loop {
                let w = word(line, k);
                let x = w ^ SPACES;
                let spaces = x.wrapping_sub(ONES) & !x & HIGHS; // high bit set in each 0x20 byte
                if spaces != 0 {
                    let n = spaces.trailing_zeros() as usize / 8;
                    h = mix(h, w & ((1u64 << (n * 8)) - 1));
                    break k + n;
                }
                h = mix(h, w);
                k += 8;
            };
            let id = table.id(&line[ep_start..ep_end], h);
            let is_error = line[ep_end + 1] == b'5';
            let (latency, j) = latency(line, ep_end + 5);
            i += j + 1;

            assert!(id < 1 << EP_BITS, "too many distinct endpoints");
            hour_min = hour_min.min(hour);
            hour_max = hour_max.max(hour);
            if block.len() == BLOCK {
                records.push(std::mem::replace(&mut block, Vec::with_capacity(BLOCK)));
            }
            block.push((
                id | hour << EP_BITS | if is_error { ERR_BIT } else { 0 },
                latency,
            ));
        }
    }
    records.push(block);
    Parsed {
        endpoints: table.names,
        records,
        hour_min,
        hour_max,
    }
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

#[inline]
fn mix(h: u64, w: u64) -> u64 {
    (h.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95)
}

/// A thread's endpoint table: open addressing on the full 64-bit hash, so bytes are
/// compared only when the hashes already agree.
struct Endpoints {
    slots: Vec<(u64, u32)>, // (hash, id + 1); id 0 marks an empty slot
    names: Vec<&'static [u8]>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            slots: vec![(0, 0); 4096],
            names: Vec::new(),
        }
    }
}

impl Endpoints {
    #[inline]
    fn id(&mut self, name: &'static [u8], h: u64) -> u32 {
        let mask = self.slots.len() - 1;
        let mut i = (h >> 40) as usize & mask;
        loop {
            let (sh, id) = self.slots[i];
            if id == 0 {
                break;
            }
            if sh == h && self.names[id as usize - 1] == name {
                return id - 1;
            }
            i = (i + 1) & mask;
        }
        self.names.push(name);
        self.slots[i] = (h, self.names.len() as u32);
        if self.names.len() * 2 > self.slots.len() {
            let cap = self.slots.len() * 2;
            let old = std::mem::replace(&mut self.slots, vec![(0, 0); cap]);
            let mask = self.slots.len() - 1;
            for (h, id) in old.into_iter().filter(|&(_, id)| id != 0) {
                let mut i = (h >> 40) as usize & mask;
                while self.slots[i].1 != 0 {
                    i = (i + 1) & mask;
                }
                self.slots[i] = (h, id);
            }
        }
        self.names.len() as u32 - 1
    }
}

/// Nearest-rank p50, p95 and p99: the values at 1-indexed rank ceil(p * n / 100).
/// Three nested selections instead of a sort, each on the part left of the last one.
fn percentiles(v: &mut [u32]) -> [u32; 3] {
    let n = v.len();
    let idx = |p: usize| (p * n).div_ceil(100) - 1;
    let (i50, i95, i99) = (idx(50), idx(95), idx(99));
    let (left, &mut p99, _) = v.select_nth_unstable(i99);
    let p95 = if i95 == i99 {
        p99
    } else {
        *left.select_nth_unstable(i95).1
    };
    let p50 = if i50 == i95 {
        p95
    } else {
        *left[..i95].select_nth_unstable(i50).1
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

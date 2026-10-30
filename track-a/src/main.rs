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

use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::thread;

/// FxHash: one multiply per 8 bytes. Endpoint strings are short and not adversarial.
#[derive(Default)]
struct Fx(u64);

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(K);
        }
        for &b in chunks.remainder() {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(K);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Record key layout: endpoint (local id) in bits 0..16, hour in bits 16..31, 5xx in bit 31.
const EP_BITS: u32 = 16;
const HOUR_MASK: u32 = 0x7fff;
const ERR_BIT: u32 = 1 << 31;

/// One thread's parse result. Endpoint slices borrow from the mapped input.
struct Parsed {
    endpoints: Vec<&'static [u8]>,
    records: Vec<(u32, u32)>,
    hour_min: u32,
    hour_max: u32,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().unwrap_or_else(|| "/data".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/out".into()));
    let input = mmap::map(&data.join("requests.log"));
    let threads = thread::available_parallelism().map_or(1, |n| n.get()).min(12);
    // Hours are stored relative to 2026-10-30T00:00Z; every timestamp is in the 72 h after 18:00.
    let base = days_from_civil(2026, 10, 30) * 24;

    let mut parts: Vec<Parsed> = thread::scope(|s| {
        let handles: Vec<_> = split_lines(input, threads)
            .into_iter()
            .map(|chunk| s.spawn(move || parse(chunk, base)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Bytewise order of `&[u8]` is exactly the spec's endpoint order.
    let mut names: Vec<&[u8]> = parts.iter().flat_map(|p| p.endpoints.iter().copied()).collect();
    names.sort_unstable();
    names.dedup();
    let n_ep = names.len();
    let hour_min = parts.iter().map(|p| p.hour_min).min().unwrap_or(0);
    let hour_max = parts.iter().map(|p| p.hour_max).max().unwrap_or(0);
    let groups = if n_ep == 0 { 0 } else { (hour_max - hour_min + 1) as usize * n_ep };
    assert!(groups < u32::MAX as usize);

    // Rewrite each record's key to its global group id, counting per thread.
    let errors: Vec<AtomicU32> = (0..groups).map(|_| AtomicU32::new(0)).collect();
    let mut cursors: Vec<Vec<u32>> = thread::scope(|s| {
        let (names, errors) = (&names, &errors);
        let handles: Vec<_> = parts
            .iter_mut()
            .map(|p| {
                s.spawn(move || {
                    let rank: Vec<usize> = p.endpoints.iter().map(|e| names.binary_search(e).unwrap()).collect();
                    let mut count = vec![0u32; groups];
                    for rec in &mut p.records {
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

    // Prefix sums: `starts[g]` is group g's first slot; each thread's counts become its
    // private write cursors inside every group, so the scatter needs no synchronisation.
    let mut starts = vec![0u32; groups + 1];
    let mut total = 0u32;
    for g in 0..groups {
        starts[g] = total;
        for c in cursors.iter_mut() {
            let n = c[g];
            c[g] = total;
            total += n;
        }
    }
    starts[groups] = total;

    let mut latencies = vec![0u32; total as usize];
    let dst = Scatter(latencies.as_mut_ptr());
    thread::scope(|s| {
        for (p, mut cur) in parts.iter().zip(cursors) {
            let dst = &dst;
            s.spawn(move || {
                for &(g, latency) in &p.records {
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
    drop(parts);

    // Cut the groups into ranges of roughly equal record counts and format them in parallel.
    let per = (total as usize).div_ceil(threads).max(1);
    let mut cuts: Vec<usize> = (1..threads)
        .map(|k| starts.partition_point(|&x| (x as usize) < k * per).min(groups))
        .collect();
    cuts.insert(0, 0);
    cuts.push(groups);
    cuts.dedup();

    let chunks: Vec<Vec<u8>> = thread::scope(|s| {
        let mut rest = latencies.as_mut_slice();
        let mut handles = Vec::new();
        for w in cuts.windows(2) {
            let (g0, g1) = (w[0], w[1]);
            let (mine, tail) = rest.split_at_mut((starts[g1] - starts[g0]) as usize);
            rest = tail;
            let (starts, names, errors) = (&starts, &names, &errors);
            handles.push(s.spawn(move || {
                let mut buf = Vec::with_capacity(mine.len() * 4 + 1024);
                let mut prefix = [0u8; 21];
                let mut prefix_hour = usize::MAX;
                for g in g0..g1 {
                    let (lo, hi) = ((starts[g] - starts[g0]) as usize, (starts[g + 1] - starts[g0]) as usize);
                    if lo == hi {
                        continue;
                    }
                    if g / n_ep != prefix_hour {
                        prefix_hour = g / n_ep;
                        prefix = hour_prefix(base + (hour_min as usize + prefix_hour) as i64);
                    }
                    let [p50, p95, p99] = percentiles(&mut mine[lo..hi]);
                    let count = (hi - lo) as u64;
                    let errs = errors[g].load(Relaxed) as u64;
                    buf.extend_from_slice(&prefix);
                    buf.extend_from_slice(names[g % n_ep]);
                    for v in [count, p50 as u64, p95 as u64, p99 as u64, errs, errs * 1_000_000 / count] {
                        buf.push(b' ');
                        push_num(&mut buf, v);
                    }
                    buf.push(b'\n');
                }
                buf
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut file = File::create(out.join("summary.txt")).expect("create summary.txt");
    for chunk in &chunks {
        file.write_all(chunk).expect("write summary.txt");
    }
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
            end = data[guess..].iter().position(|&b| b == b'\n').map_or(data.len(), |p| guess + p + 1);
        }
        chunks.push(&data[start..end]);
        start = end;
    }
    chunks
}

/// Parses whole lines of the grammar in the spec:
/// `2026-10-30T18:07:18.569411Z GET /api/v2/products 200 8177`
fn parse(chunk: &'static [u8], base: i64) -> Parsed {
    let mut ids: HashMap<&[u8], u32, BuildHasherDefault<Fx>> = HashMap::default();
    let mut endpoints = Vec::new();
    let mut records = Vec::with_capacity(chunk.len() / 70);
    let (mut hour_min, mut hour_max) = (u32::MAX, 0);
    let mut date: &[u8] = &[];
    let mut date_hours = 0i64;
    let mut i = 0;
    while i < chunk.len() {
        let line = &chunk[i..];
        // Lines arrive in time-ordered shards, so the date rarely changes between lines.
        if line[..10] != *date {
            date = &line[..10];
            date_hours = days_from_civil(num(&line[0..4]), num(&line[5..7]), num(&line[8..10])) * 24;
        }
        let hour = date_hours + num(&line[11..13]) - base;
        assert!((0..=HOUR_MASK as i64).contains(&hour), "timestamp outside the supported span");
        let hour = hour as u32;
        // The method starts at byte 28; its length follows from its first one or two letters.
        let method_len = match line[28] {
            b'G' => 3,          // GET
            b'H' => 4,          // HEAD
            b'D' => 6,          // DELETE
            b'O' => 7,          // OPTIONS
            _ => match line[29] {
                b'O' => 4,      // POST
                b'U' => 3,      // PUT
                _ => 5,         // PATCH
            },
        };
        let ep_start = 29 + method_len;
        let ep_len = line[ep_start..].iter().position(|&b| b == b' ').expect("endpoint");
        let endpoint = &line[ep_start..ep_start + ep_len];
        let status = ep_start + ep_len + 1;
        let is_error = line[status] == b'5';
        let mut j = status + 4;
        let mut latency = 0u32;
        while line[j] != b'\n' {
            latency = latency * 10 + (line[j] - b'0') as u32;
            j += 1;
        }
        i += j + 1;

        let id = *ids.entry(endpoint).or_insert_with(|| {
            endpoints.push(endpoint);
            endpoints.len() as u32 - 1
        });
        assert!(id < 1 << EP_BITS, "too many distinct endpoints");
        hour_min = hour_min.min(hour);
        hour_max = hour_max.max(hour);
        records.push((id | hour << EP_BITS | if is_error { ERR_BIT } else { 0 }, latency));
    }
    Parsed { endpoints, records, hour_min, hour_max }
}

/// Nearest-rank p50, p95 and p99: the values at 1-indexed rank ceil(p * n / 100).
/// Three nested selections instead of a sort, each on the part left of the last one.
fn percentiles(v: &mut [u32]) -> [u32; 3] {
    let n = v.len();
    let idx = |p: usize| (p * n).div_ceil(100) - 1;
    let (i50, i95, i99) = (idx(50), idx(95), idx(99));
    let (left, &mut p99, _) = v.select_nth_unstable(i99);
    let p95 = if i95 == i99 { p99 } else { *left.select_nth_unstable(i95).1 };
    let p50 = if i50 == i95 { p95 } else { *left[..i95].select_nth_unstable(i50).1 };
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
    digits.iter().fold(0, |acc, &b| acc * 10 + (b - b'0') as i64)
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

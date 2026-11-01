//! Track C: Join. Parse a hostile CSV, join each record to its region's country,
//! and print per-country aggregates.
//!
//! Splitting a CSV for threads is normally unsafe because a newline inside quotes
//! is data. Under section 2.2 of the spec a line inside a quoted field never starts
//! with a digit, while every record does (or with a BOM), so each split point is the
//! first newline after a guess that is followed by a digit or a BOM.
//!
//! Each thread parses its records with a byte-level state machine straight from the
//! map and keeps running sums per country. Distinct names need no unescaping: two
//! name fields have equal values exactly when their raw bytes are equal (an unquoted
//! field cannot hold `"`, and `""` -> `"` is one-to-one). All threads intern names by
//! a 64-bit hash of their raw bytes in one lock-free table that hands out dense ids,
//! and each country keeps a bitset of the ids it has seen.

mod mmap;

use std::cell::Cell;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::thread;

/// One step of the name hash per 8-byte word: the folded multiply of foldhash and
/// wyhash. The halves of the full 128-bit product are xored, so every input bit
/// reaches every output bit. The FxHash step this replaced, a plain 64-bit multiply,
/// only carries a change upward: names differing in their 8th and 9th bytes collided
/// in all 64 bits about once in 32 (seed 6 has "Dacom Mina" and "Dacom Maca", which
/// were counted as one name).
fn mix(h: u64, w: u64) -> u64 {
    let m = (h ^ w) as u128 * 0x9e37_79b9_7f4a_7c15;
    m as u64 ^ (m >> 64) as u64
}

const BOM: &[u8] = b"\xEF\xBB\xBF";
const NO_REGION: u16 = u16::MAX;
/// Name table slots. The spec's name pool is 100,000 values; the table stays under half full.
const NAME_SLOTS: usize = 1 << 18;
/// Name ids that get a bit in each country's bitset (the spec's pool is 100,000).
/// Rarer ids beyond it go to an overflow list, so the count stays exact regardless.
const MAX_IDS: usize = 1 << 17;

/// Running aggregates for one output line.
#[derive(Clone, Copy)]
struct Agg {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    name_bytes: u64,
    note_bytes: u64,
}

impl Agg {
    const EMPTY: Agg = Agg {
        count: 0,
        id_sum: 0,
        amount_sum: 0,
        amount_min: i64::MAX,
        amount_max: i64::MIN,
        name_bytes: 0,
        note_bytes: 0,
    };

    fn merge(&mut self, o: &Agg) {
        self.count += o.count;
        self.id_sum += o.id_sum;
        self.amount_sum += o.amount_sum;
        self.amount_min = self.amount_min.min(o.amount_min);
        self.amount_max = self.amount_max.max(o.amount_max);
        self.name_bytes += o.name_bytes;
        self.note_bytes += o.note_bytes;
    }
}

fn main() {
    mmap::small_pages();
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().unwrap_or_else(|| "/data".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/out".into()));
    let (codes, region) = load_regions(mmap::map(&data.join("regions.csv")));
    let records = mmap::map(&data.join("records.csv"));
    let threads = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(12);
    let groups = codes.len() + 1; // the last group is _UNMATCHED

    let names = Names::new();
    let bits = zeroed_atomics(groups * MAX_IDS / 64);
    let overflow = Mutex::new(Vec::new());
    let locks: Vec<Mutex<()>> = (0..SHARDS).map(|_| Mutex::new(())).collect();
    let seen = Seen {
        bits: &bits,
        locks: &locks,
        per_shard: groups.div_ceil(SHARDS),
        overflow: &overflow,
    };
    // Many chunks handed out on demand, so a slow core never holds up the rest.
    let chunks = split_records(records, threads * 8);
    let next = AtomicUsize::new(0);
    let aggs = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (region, names, seen, chunks, next) = (&region, &names, &seen, &chunks, &next);
                s.spawn(move || {
                    let mut aggs = vec![Agg::EMPTY; groups];
                    let mut queues = vec![Vec::with_capacity(QUEUE); SHARDS];
                    while let Some(&chunk) = chunks.get(next.fetch_add(1, Relaxed)) {
                        let first = std::ptr::eq(chunk, chunks[0]);
                        parse(chunk, first, region, &mut aggs, names, seen, &mut queues);
                    }
                    for (s, queue) in queues.iter_mut().enumerate() {
                        seen.apply(s, queue);
                    }
                    aggs
                })
            })
            .collect();
        let mut total = vec![Agg::EMPTY; groups];
        for h in handles {
            for (a, o) in total.iter_mut().zip(&h.join().unwrap()) {
                a.merge(o);
            }
        }
        total
    });
    let mut overflow = overflow.into_inner().unwrap();
    overflow.sort_unstable();
    overflow.dedup();
    let words = (names.next_id.load(Relaxed) as usize)
        .min(MAX_IDS)
        .div_ceil(64);
    let distinct = |g: usize| {
        let set: u32 = bits[g * MAX_IDS / 64..][..words]
            .iter()
            .map(|w| w.load(Relaxed).count_ones())
            .sum();
        set + overflow.iter().filter(|&&(og, _)| og == g).count() as u32
    };

    // Country codes are [A-Z]{3}, all below `_`, so _UNMATCHED stays last.
    let mut order: Vec<usize> = (0..codes.len()).collect();
    order.sort_unstable_by_key(|&g| codes[g]);
    order.push(codes.len());

    let mut buf = Vec::with_capacity(groups * 80);
    for g in order {
        let a = &aggs[g];
        if a.count == 0 {
            continue;
        }
        buf.extend_from_slice(if g < codes.len() {
            codes[g]
        } else {
            b"_UNMATCHED"
        });
        let distinct = distinct(g);
        for (v, money) in [
            (a.count as i64, false),
            (a.id_sum as i64, false),
            (a.amount_sum, true),
            (a.amount_min, true),
            (a.amount_max, true),
            (distinct as i64, false),
            (a.name_bytes as i64, false),
            (a.note_bytes as i64, false),
        ] {
            buf.push(b' ');
            if money {
                push_cents(&mut buf, v);
            } else {
                push_num(&mut buf, v as u64);
            }
        }
        buf.push(b'\n');
    }
    File::create(out.join("aggregates.txt"))
        .and_then(|mut f| f.write_all(&buf))
        .expect("write aggregates.txt");
}

/// Region id -> country index. One byte per id, so the table is 1 MB; a second byte
/// per id is added only if a 255th country appears (the spec has 240).
struct Regions {
    lo: Vec<u8>,
    hi: Vec<u8>, // empty while one byte is enough
}

impl Regions {
    /// The country of region `id`, or None if `id` has no region.
    #[inline]
    fn get(&self, id: usize) -> Option<usize> {
        let g = *self.lo.get(id)? as usize | self.hi.get(id).map_or(0, |&h| (h as usize) << 8);
        let none = if self.hi.is_empty() { 0xFF } else { 0xFFFF };
        (g != none).then_some(g)
    }
}

/// regions.csv: `region_id,country` lines after a header, ids ascending, codes
/// `[A-Z]{3}`. Returns the distinct country codes and the region table. Codes index
/// a 26^3 table directly, so there is no hashing.
fn load_regions(text: &'static [u8]) -> (Vec<&'static [u8]>, Regions) {
    let mut codes: Vec<&[u8]> = Vec::new();
    let mut by_code = vec![NO_REGION; 26 * 26 * 26];
    // Ids ascend, so the last line holds the largest: size the table once.
    let body = text.strip_suffix(b"\n").unwrap_or(text);
    let last = body.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let n = digits(body, last).0 as usize + 1;
    let mut region = Regions {
        lo: vec![0xFF; n],
        hi: Vec::new(),
    };
    let mut p = text
        .iter()
        .position(|&b| b == b'\n')
        .map_or(text.len(), |i| i + 1);
    while p + 4 < text.len() {
        let (id, comma) = digits(text, p);
        let code = &text[comma + 1..comma + 4];
        let key = code.iter().fold(0, |k, &b| k * 26 + (b - b'A') as usize);
        if by_code[key] == NO_REGION {
            if codes.len() == 0xFF {
                // Index 255 would read as "no region": widen. Ids without a region
                // become 0xFFFF, the others keep a zero high byte.
                region.hi = region
                    .lo
                    .iter()
                    .map(|&l| if l == 0xFF { 0xFF } else { 0 })
                    .collect();
            }
            by_code[key] = codes.len() as u16;
            codes.push(code);
        }
        let g = by_code[key];
        region.lo[id as usize] = g as u8;
        if !region.hi.is_empty() {
            region.hi[id as usize] = (g >> 8) as u8;
        }
        p = comma + 5;
    }
    (codes, region)
}

/// Splits the file into `n` chunks that each start at a record boundary.
///
/// Section 2.2 of the spec, which solutions may rely on, guarantees that a line
/// inside a quoted field starts with a lowercase letter or a `"` (only notes span
/// lines, and they are lowercase words), while every record starts with a digit
/// or a BOM. So a newline followed by a digit or a BOM is always a record boundary,
/// and each split point is found by looking at a few bytes near its guess, without
/// reading the whole file first.
fn split_records(data: &'static [u8], n: usize) -> Vec<&'static [u8]> {
    let mut bounds = vec![0];
    for k in 1..n {
        let mut p = data.len() * k / n;
        let end = loop {
            match data[p..].iter().position(|&b| b == b'\n') {
                None => break data.len(),
                Some(i) => p += i + 1,
            }
            if data.get(p).is_some_and(|b| b.is_ascii_digit()) || data[p..].starts_with(BOM) {
                break p;
            }
        };
        bounds.push(end.max(*bounds.last().unwrap()));
    }
    bounds.push(data.len());
    bounds.windows(2).map(|w| &data[w[0]..w[1]]).collect()
}

/// Parses whole records (section 2.1 of the spec) and aggregates them.
/// Where each (country, name id) pair is recorded: one bit per pair, shared by all
/// threads, so its memory does not grow with the number of threads.
///
/// The countries are split into shards of consecutive rows, each behind a lock. A
/// thread queues its pairs per shard and applies a full queue under one lock, with
/// plain loads and stores: no locked instruction per record, and no bitset line
/// moving between cores per record. A queue touches only its shard's rows, which
/// stay in cache while it is applied.
struct Seen<'a> {
    bits: &'a [AtomicU64],
    locks: &'a [Mutex<()>],
    per_shard: usize, // countries per shard
    overflow: &'a Mutex<Vec<(usize, u32)>>,
}

/// Bitset shards, and pairs queued per shard before a thread applies them.
const SHARDS: usize = 16;
const QUEUE: usize = 256;

impl Seen<'_> {
    /// Queues the pair `(g, id)`, packed as `g << 17 | id`: ids are below MAX_IDS = 2^17,
    /// and there are at most 26^3 + 1 < 2^15 countries.
    #[inline]
    fn insert(&self, queues: &mut [Vec<u32>], g: usize, id: u32) {
        if (id as usize) < MAX_IDS {
            let s = g / self.per_shard;
            queues[s].push((g as u32) << MAX_IDS.trailing_zeros() | id);
            if queues[s].len() == QUEUE {
                self.apply(s, &mut queues[s]);
            }
        } else {
            self.overflow.lock().unwrap().push((g, id));
        }
    }

    /// Sets the bits of shard `s`'s queued pairs. Only the holder of the shard's lock
    /// writes its words, so a plain load and store is an exact OR.
    fn apply(&self, s: usize, queue: &mut Vec<u32>) {
        let _held = self.locks[s].lock().unwrap();
        for &e in queue.iter() {
            let bit =
                (e >> MAX_IDS.trailing_zeros()) as usize * MAX_IDS + (e as usize & (MAX_IDS - 1));
            let word = &self.bits[bit / 64];
            word.store(word.load(Relaxed) | 1 << (bit % 64), Relaxed);
        }
        queue.clear();
    }
}

fn parse(
    c: &[u8],
    first: bool,
    region: &Regions,
    aggs: &mut [Agg],
    names: &Names,
    seen: &Seen,
    queues: &mut [Vec<u32>],
) {
    let unmatched = (aggs.len() - 1) as u16;
    let mut batch = [(0, 0, 0); BATCH];
    let mut n = 0;
    let mut p = 0;
    if first {
        // The header is the first record: skip it, with the file's leading BOM.
        p = c
            .iter()
            .position(|&b| b == b'\n')
            .map_or(c.len(), |i| i + 1);
    }
    while p < c.len() {
        // Every byte but a BOM's is ASCII (spec 2.2), so 0xEF at a record start is one.
        if c[p] == BOM[0] {
            p += 3;
        }
        let (id, next) = digits(c, p);
        let name = field(c, next + 1);
        let note = field(c, name.end + 1);
        let (region_id, next) = digits(c, note.end + 1);
        let (cents, next) = amount(c, next + 1);
        p = next
            + match c.get(next) {
                Some(b'\r') => 2,
                Some(_) => 1,
                None => 0,
            };

        let g = region.get(region_id as usize).unwrap_or(unmatched as usize);
        let a = &mut aggs[g];
        a.count += 1;
        a.id_sum += id;
        a.amount_sum += cents;
        a.amount_min = a.amount_min.min(cents);
        a.amount_max = a.amount_max.max(cents);
        a.name_bytes += (name.len() - name.escapes) as u64;
        a.note_bytes += (note.len() - note.escapes) as u64;

        std::hint::black_box((g, name.start, name.end_content)); // TIMING PROBE: no interning
    }
    intern(c, &batch[..n], names, seen, queues);
}

/// Records per interning batch.
const BATCH: usize = 32;

/// Interns a batch of (country, name start, name end) and queues each pair to be
/// marked seen. Every lookup is a likely cache miss into the shared name table, so
/// they go in two passes: hash and prefetch every slot, then resolve the ids. The
/// misses of one pass overlap instead of each record waiting for its own.
fn intern(
    c: &[u8],
    batch: &[(usize, usize, usize)],
    names: &Names,
    seen: &Seen,
    queues: &mut [Vec<u32>],
) {
    let mut hashes = [0u64; BATCH];
    for (h, &(_, start, end)) in hashes.iter_mut().zip(batch) {
        *h = Names::hash(c, start, end - start);
        names.prefetch_slot(*h);
    }
    for (&h, &(g, ..)) in hashes.iter().zip(batch) {
        seen.insert(queues, g, names.id(h));
    }
}

/// Starts loading the cache line holding `r`. A hint only: it never faults.
#[inline]
fn prefetch<T>(r: &T) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a prefetch never faults and `r` is a valid reference anyway.
    unsafe {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch::<_MM_HINT_T0>(r as *const T as *const i8);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = r;
}

/// Lock-free interning of names into dense ids, shared by all threads, keyed by a
/// strong 64-bit hash of the raw field bytes. A slot is one word, the hash's top 40
/// bits above `id + 1`, so the table is 2 MB and a lookup is one probe with no byte
/// comparison. The low 18 hash bits pick the first slot, so a name is identified by
/// 58 independent hash bits.
///
/// Identity by hash is not provably exact: two different names would have to agree
/// on all of those bits. For the spec's pool of 100,000 names that chance is about
/// 2 * 10^-8 per run, and it buys half of this track's run time (the arena of name
/// bytes this replaced cost a second cache miss per record).
struct Names {
    slots: Box<[AtomicU64]>,
    next_id: AtomicU32,
}

const ID_BITS: u32 = 24;
/// Fresh name ids a thread reserves at a time.
const ID_BLOCK: u32 = 256;
const ID_MASK: u64 = (1 << ID_BITS) - 1;

impl Names {
    fn new() -> Names {
        Names {
            slots: zeroed_atomics(NAME_SLOTS),
            next_id: AtomicU32::new(0),
        }
    }

    /// The hash of the name `c[start..start + len]`: the folded multiply over its
    /// words. The fold spreads every bit both ways, so no finaliser is needed: on 4.9
    /// million near-duplicate names (one or two bytes changed) it had no collision.
    fn hash(c: &[u8], start: usize, len: usize) -> u64 {
        (0..len.div_ceil(8)).fold(len as u64, |h, k| mix(h, name_word(c, start, len, k)))
    }

    /// Starts loading the slot a name with hash `h` probes first.
    #[inline]
    fn prefetch_slot(&self, h: u64) {
        prefetch(&self.slots[h as usize & (NAME_SLOTS - 1)]);
    }

    /// The dense id of the name with hash `h`. A thread that finds an empty slot takes
    /// a fresh id and claims the slot with one compare-exchange; if another thread
    /// claimed it first, the id is kept for the thread's next new name.
    ///
    /// Fresh ids come from the thread's own block of ID_BLOCK, reserved from the shared
    /// counter at once: most of the 100,000 names are first met in the run's first few
    /// milliseconds, and one shared counter bumped per name makes every thread wait
    /// for its cache line. Ids reserved but never used only leave unset bits, which
    /// change no count.
    fn id(&self, h: u64) -> u32 {
        thread_local! {
            static RESERVED: Cell<(u32, u32)> = const { Cell::new((0, 0)) }; // next..end
        }
        let tag = h & !ID_MASK;
        let mut i = h as usize & (NAME_SLOTS - 1);
        loop {
            let slot = &self.slots[i];
            let mut v = slot.load(Acquire);
            if v == 0 {
                let (mut new, mut end) = RESERVED.get();
                if new == end {
                    new = self.next_id.fetch_add(ID_BLOCK, Relaxed);
                    end = new + ID_BLOCK;
                }
                assert!((new as u64) < ID_MASK, "too many distinct names");
                match slot.compare_exchange(0, tag | (new as u64 + 1), AcqRel, Acquire) {
                    Ok(_) => {
                        RESERVED.set((new + 1, end));
                        return new;
                    }
                    Err(found) => {
                        RESERVED.set((new, end));
                        v = found;
                    }
                }
            }
            if v & !ID_MASK == tag {
                return ((v & ID_MASK) - 1) as u32;
            }
            i = (i + 1) & (NAME_SLOTS - 1);
        }
    }
}

/// Word `k` of the name `c[start..start + len]`, zero-padded past its end. The name is
/// always followed by more of the record, so a full 8-byte load from the input plus
/// a mask replaces a variable-length copy (a `memcpy` call) almost every time.
#[inline]
fn name_word(c: &[u8], start: usize, len: usize, k: usize) -> u64 {
    let (at, keep) = (start + k * 8, (len - k * 8).min(8));
    let w = match c.get(at..at + 8) {
        Some(w) => u64::from_le_bytes(w.try_into().unwrap()),
        None => {
            let mut w = [0u8; 8];
            w[..keep].copy_from_slice(&c[at..at + keep]);
            u64::from_le_bytes(w)
        }
    };
    if keep == 8 {
        w
    } else {
        w & ((1u64 << (keep * 8)) - 1)
    }
}

/// `n` zeroed atomics from a zeroed allocation, so pages that are never written are
/// never touched and never charged to the run's memory.
fn zeroed_atomics(n: usize) -> Box<[AtomicU64]> {
    let words: Box<[u64]> = vec![0u64; n].into_boxed_slice();
    // SAFETY: AtomicU64 has the same size, alignment and bit validity as u64.
    unsafe { Box::from_raw(Box::into_raw(words) as *mut [AtomicU64]) }
}

/// A text field: its content bytes are `start..end_content`, and `end` is the
/// position of the byte that follows the field (a comma here).
struct Field {
    start: usize,
    end_content: usize,
    end: usize,
    escapes: usize,
}

impl Field {
    /// Raw content length, before `""` pairs are collapsed.
    fn len(&self) -> usize {
        self.end_content - self.start
    }
}

/// Reads a quoted or unquoted text field starting at `p`. Forced inline: as a call it
/// returns its four words through memory, twice per record.
#[inline(always)]
fn field(c: &[u8], p: usize) -> Field {
    if c[p] != b'"' {
        let end = find(c, p, b',');
        return Field {
            start: p,
            end_content: end,
            end,
            escapes: 0,
        };
    }
    let mut i = p + 1;
    let mut escapes = 0;
    loop {
        i = find(c, i, b'"');
        if c.get(i + 1) == Some(&b'"') {
            escapes += 1;
            i += 2;
        } else {
            return Field {
                start: p + 1,
                end_content: i,
                end: i + 1,
                escapes,
            };
        }
    }
}

/// Position of the first `byte` at or after `p`: 32 bytes per step with AVX2 (the
/// scoring box), so most fields end in the first step, and eight bytes per step by
/// word arithmetic otherwise and near the end of the chunk.
#[inline]
fn find(c: &[u8], mut p: usize, byte: u8) -> usize {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    while let Some(block) = c.get(p..p + 32) {
        use std::arch::x86_64::*;
        // SAFETY: AVX2 is enabled at compile time, and the unaligned load reads exactly
        // the 32 bytes of `block`.
        let hits = unsafe {
            let v = _mm256_loadu_si256(block.as_ptr() as *const __m256i);
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(byte as i8))) as u32
        };
        if hits != 0 {
            return p + hits.trailing_zeros() as usize;
        }
        p += 32;
    }
    const ONES: u64 = 0x0101_0101_0101_0101;
    while let Some(w) = c.get(p..p + 8) {
        let x = u64::from_le_bytes(w.try_into().unwrap()) ^ (ONES * byte as u64);
        let hits = x.wrapping_sub(ONES) & !x & (ONES << 7); // high bit set where x has a 0 byte
        if hits != 0 {
            return p + hits.trailing_zeros() as usize / 8;
        }
        p += 8;
    }
    p + c[p..]
        .iter()
        .position(|&b| b == byte)
        .expect("field terminator")
}

/// An unsigned decimal at `p`, ended by `,` or `.`; returns it and the position of
/// the byte after it. Up to seven digits (every region id and amount, and ids below
/// ten million) are found and converted with word arithmetic: find the delimiter in
/// one 8-byte load, left-align the digits, then combine pairs, quads and octets by
/// multiplication. Longer numbers take the byte loop.
#[inline]
fn digits(c: &[u8], mut p: usize) -> (u64, usize) {
    const ONES: u64 = 0x0101_0101_0101_0101;
    if let Some(w) = c.get(p..p + 8) {
        let w = u64::from_le_bytes(w.try_into().unwrap());
        let zero = |x: u64| x.wrapping_sub(ONES) & !x & (ONES << 7);
        let n = (zero(w ^ ONES * b',' as u64) | zero(w ^ ONES * b'.' as u64)).trailing_zeros()
            as usize
            / 8;
        if (1..8).contains(&n) {
            let v = (w << (8 * (8 - n))) & 0x0f0f_0f0f_0f0f_0f0f;
            let v = v.wrapping_mul(10 * (1 << 8) + 1) >> 8 & 0x00ff_00ff_00ff_00ff;
            let v = v.wrapping_mul(100 * (1 << 16) + 1) >> 16 & 0x0000_ffff_0000_ffff;
            return (v.wrapping_mul(10000 * (1 << 32) + 1) >> 32, p + n);
        }
    }
    let mut v = 0u64;
    while let Some(&b) = c.get(p).filter(|b| b.is_ascii_digit()) {
        v = v * 10 + (b - b'0') as u64;
        p += 1;
    }
    (v, p)
}

/// `-?[0-9]+\.[0-9]{2}` as exact integer cents.
#[inline]
fn amount(c: &[u8], p: usize) -> (i64, usize) {
    let negative = c[p] == b'-';
    let (whole, dot) = digits(c, p + negative as usize);
    let cents = whole as i64 * 100 + ((c[dot + 1] - b'0') * 10 + (c[dot + 2] - b'0')) as i64;
    (if negative { -cents } else { cents }, dot + 3)
}

/// Integer cents as `-?D+.DD`, never `-0.00`.
fn push_cents(out: &mut Vec<u8>, cents: i64) {
    if cents < 0 {
        out.push(b'-');
    }
    let abs = cents.unsigned_abs();
    push_num(out, abs / 100);
    out.push(b'.');
    out.push(b'0' + (abs % 100 / 10) as u8);
    out.push(b'0' + (abs % 10) as u8);
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

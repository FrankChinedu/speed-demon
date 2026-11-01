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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, available_parallelism};

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
    let regions = mmap::map(&data.join("regions.csv"));
    let records = mmap::map(&data.join("records.csv"));
    let threads = available_parallelism().map_or(1, |n| n.get().min(12));
    let names = Names::new();
    // Sized for every possible country (26^3 codes and _UNMATCHED) before regions.csv
    // says how many there are: rows of countries that do not occur are never touched,
    // and untouched pages cost nothing.
    let bits = zeroed_atomics((26 * 26 * 26 + 1) * MAX_IDS / 64);
    let overflow = Mutex::new(Vec::new());
    let locks: Vec<Mutex<()>> = (0..SHARDS).map(|_| Mutex::new(())).collect();
    let setup = OnceLock::new(); // country codes and the region table
    // Many chunks handed out on demand, so a slow core never holds up the rest.
    let chunks = split_records(records, threads * 8);
    let next = AtomicUsize::new(0);
    let aggs = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (setup, chunks, next) = (&setup, &chunks, &next);
                let (names, bits, locks, overflow) = (&names, &bits, &locks, &overflow);
                s.spawn(move || {
                    // While the main thread reads regions.csv, fault in this thread's
                    // share of records.csv (the kernel maps 64 KB per fault), so the
                    // parse later finds those pages mapped.
                    let share = records.len() * t / threads..records.len() * (t + 1) / threads;
                    for page in records[share].chunks(1 << 16) {
                        if setup.get().is_some() {
                            break;
                        }
                        std::hint::black_box(page[0]);
                    }
                    let (codes, region): &(Vec<&[u8]>, Regions) = setup.wait();
                    let groups = codes.len() + 1; // the last group is _UNMATCHED
                    let shared = Shared {
                        region,
                        names,
                        bits,
                        locks,
                        // Shards of a power of two of countries: a pair finds its shard by a shift.
                        shard_shift: groups.div_ceil(SHARDS).next_power_of_two().trailing_zeros(),
                        overflow,
                    };
                    let mut aggs = vec![Agg::EMPTY; groups];
                    let mut queues = vec![Vec::with_capacity(QUEUE); SHARDS];
                    while let Some(&chunk) = chunks.get(next.fetch_add(1, Relaxed)) {
                        let first = std::ptr::eq(chunk, chunks[0]);
                        parse(chunk, first, &shared, &mut aggs, &mut queues);
                    }
                    for (s, queue) in queues.iter_mut().enumerate() {
                        shared.apply(s, queue);
                    }
                    aggs
                })
            })
            .collect();
        setup.set(load_regions(regions)).ok();
        let groups = setup.get().unwrap().0.len() + 1;
        let mut total = vec![Agg::EMPTY; groups];
        for h in handles {
            for (a, o) in total.iter_mut().zip(&h.join().unwrap()) {
                a.merge(o);
            }
        }
        total
    });
    let (codes, _) = setup.into_inner().unwrap();
    let groups = codes.len() + 1;
    let mut overflow = overflow.into_inner().unwrap();
    overflow.sort_unstable();
    overflow.dedup();
    let ids = names.next_id.load(Relaxed) as usize;
    let words = ids.min(MAX_IDS).div_ceil(64);
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
        buf.extend_from_slice(codes.get(g).copied().unwrap_or(b"_UNMATCHED"));
        for v in [a.count, a.id_sum] {
            push_num(&mut buf, v);
        }
        for v in [a.amount_sum, a.amount_min, a.amount_max] {
            push_cents(&mut buf, v);
        }
        for v in [distinct(g) as u64, a.name_bytes, a.note_bytes] {
            push_num(&mut buf, v);
        }
        buf.push(b'\n');
    }
    std::fs::write(out.join("aggregates.txt"), &buf).expect("write aggregates.txt");
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
    let mut p = after_lf(text, 0);
    while p + 4 < text.len() {
        let (id, comma) = digits(text, p);
        let code = &text[comma + 1..comma + 4];
        let key = code.iter().fold(0, |k, &b| k * 26 + (b - b'A') as usize);
        if by_code[key] == NO_REGION {
            if codes.len() == 0xFF {
                // Index 255 would read as "no region": widen. Ids without a region
                // become 0xFFFF, the others keep a zero high byte.
                region.hi = region.lo.iter().map(|&l| l / 0xFF * 0xFF).collect();
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

/// The position just after the first LF at or after `p` (or the end).
fn after_lf(c: &[u8], p: usize) -> usize {
    c[p..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(c.len(), |i| p + i + 1)
}

/// Where each (country, name id) pair is recorded: one bit per pair, shared by all
/// threads, so its memory does not grow with the number of threads.
///
/// The countries are split into shards of consecutive rows, each behind a lock. A
/// thread queues its pairs per shard and applies a full queue under one lock, with
/// plain loads and stores: no locked instruction per record, and no bitset line
/// moving between cores per record. A queue touches only its shard's rows, which
/// stay in cache while it is applied.
struct Shared<'a> {
    region: &'a Regions,
    names: &'a Names,
    bits: &'a [AtomicU64],
    locks: &'a [Mutex<()>],
    shard_shift: u32, // a country's shard is its index shifted right by this
    overflow: &'a Mutex<Vec<(usize, u32)>>,
}

/// Bitset shards, and pairs queued per shard before a thread applies them.
const SHARDS: usize = 16;
const QUEUE: usize = 256;

impl Shared<'_> {
    /// Queues the pair `(g, id)`, packed as `g << 17 | id`: ids are below MAX_IDS = 2^17,
    /// and there are at most 26^3 + 1 < 2^15 countries.
    #[inline]
    fn insert(&self, queues: &mut [Vec<u32>], g: usize, id: u32) {
        if (id as usize) < MAX_IDS {
            let s = g >> self.shard_shift;
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

/// Parses whole records (section 2.1 of the spec) and aggregates them.
///
/// The parse is driven by the record separators, found 64 bytes at a time: commas and
/// LFs that are outside quotes. Inside-quote bytes are those after an odd number of
/// quotes (a `""` escape toggles twice, so it changes nothing), which is the prefix
/// XOR of the quote bits. Every record has exactly five separators (four commas and
/// its LF), so a record's fields are read straight from the next five positions,
/// with no scan per field. Each separator also carries the number of quotes before
/// it, so a text field's quotes are counted by one subtraction.
fn parse(c: &[u8], first: bool, shared: &Shared, aggs: &mut [Agg], queues: &mut [Vec<u32>]) {
    let unmatched = aggs.len() - 1;
    let mut batch = [(0, 0, 0); BATCH];
    let mut n = 0;
    // The header is the first record: skip it, with the file's leading BOM.
    let mut start = if first { after_lf(c, 0) } else { 0 };
    let mut seps = vec![(0u32, 0u32); SEPS + 64]; // (position, quotes before it)
    let (mut len, mut block, mut inside, mut quotes) = (0, start, 0u64, 0u32);
    loop {
        while len <= SEPS && block < c.len() {
            let (q, mut s) = masks(c, block);
            let mut quoted = q;
            for k in [1, 2, 4, 8, 16, 32] {
                quoted ^= quoted << k;
            }
            quoted ^= inside;
            inside = ((quoted as i64) >> 63) as u64;
            s &= !quoted;
            while s != 0 {
                let bit = s.trailing_zeros();
                seps[len] = (block as u32 + bit, quotes + (q & ((1 << bit) - 1)).count_ones());
                len += 1;
                s &= s - 1;
            }
            quotes += q.count_ones();
            block += 64;
            // The final record may have no terminator: end it at the end of the chunk.
            if block >= c.len() && c.last() != Some(&b'\n') {
                seps[len] = (c.len() as u32, quotes);
                len += 1;
            }
        }
        let mut i = 0;
        while let Some(&[c1, c2, c3, c4, end]) = seps[..len].get(i..i + 5) {
            // Every byte but a BOM's is ASCII (spec 2.2), so 0xEF at a record start is one.
            let p = start + 3 * (c[start] == BOM[0]) as usize;
            let id = digits(c, p).0;
            let (name, name_bytes) = text(c1, c2);
            let (_, note_bytes) = text(c2, c3);
            let region_id = digits(c, c3.0 as usize + 1).0;
            let cents = amount(c, c4.0 as usize + 1).0;
            start = end.0 as usize + 1;
            i += 5;

            let g = shared.region.get(region_id as usize).unwrap_or(unmatched);
            let a = &mut aggs[g];
            a.count += 1;
            a.id_sum += id;
            a.amount_sum += cents;
            a.amount_min = a.amount_min.min(cents);
            a.amount_max = a.amount_max.max(cents);
            a.name_bytes += name_bytes;
            a.note_bytes += note_bytes;

            batch[n] = (g, name.start, name.end);
            n += 1;
            if n == BATCH {
                intern(c, &batch, shared, queues);
                n = 0;
            }
        }
        seps.copy_within(i..len, 0);
        len -= i;
        if block >= c.len() {
            break;
        }
    }
    intern(c, &batch[..n], shared, queues);
}

/// Separators gathered before the records among them are read.
const SEPS: usize = 1 << 12;

/// The text field between the separators `a` and `b`: the range of its value's raw
/// bytes, inside the quotes, and the length of its value. A field is quoted exactly
/// when it holds a quote (an unquoted field cannot), and its value is its raw bytes
/// less the two delimiting quotes and one quote of each `""` pair.
#[inline(always)]
fn text(a: (u32, u32), b: (u32, u32)) -> (std::ops::Range<usize>, u64) {
    let (q, quoted) = (b.1 - a.1, (b.1 > a.1) as u32);
    let bytes = b.0 - a.0 - 1 - q / 2 - quoted;
    ((a.0 + 1 + quoted) as usize..(b.0 - quoted) as usize, bytes as u64)
}

/// Bit masks of the quotes and of the commas and LFs in the 64 bytes at `at`, zero
/// past the end of `c`.
#[inline(always)]
fn masks(c: &[u8], at: usize) -> (u64, u64) {
    let mut tail = [0u8; 64];
    let b: &[u8] = match c.get(at..at + 64) {
        Some(b) => b,
        None => {
            tail[..c.len() - at].copy_from_slice(&c[at..]);
            &tail
        }
    };
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    {
        use std::arch::x86_64::*;
        // SAFETY: AVX2 is enabled at compile time, and the two unaligned loads read
        // exactly the 64 bytes of `b`.
        unsafe {
            let bits = |v: __m256i, byte: u8| {
                _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(byte as i8))) as u32
                    as u64
            };
            let lo = _mm256_loadu_si256(b.as_ptr() as *const __m256i);
            let hi = _mm256_loadu_si256(b.as_ptr().add(32) as *const __m256i);
            let q = bits(lo, b'"') | bits(hi, b'"') << 32;
            let s = bits(lo, b',') | bits(lo, b'\n') | (bits(hi, b',') | bits(hi, b'\n')) << 32;
            (q, s)
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    b.iter().enumerate().fold((0, 0), |(q, s), (k, &x)| {
        (q | ((x == b'"') as u64) << k, s | ((x == b',' || x == b'\n') as u64) << k)
    })
}
/// Records per interning batch.
const BATCH: usize = 32;

/// Interns a batch of (country, name start, name end) and queues each pair to be
/// marked seen. Every lookup is a likely cache miss into the shared name table, so
/// they go in two passes: hash and prefetch every slot, then resolve the ids. The
/// misses of one pass overlap instead of each record waiting for its own.
fn intern(c: &[u8], batch: &[(usize, usize, usize)], shared: &Shared, queues: &mut [Vec<u32>]) {
    let mut hashes = [0u64; BATCH];
    for (h, &(_, start, end)) in hashes.iter_mut().zip(batch) {
        *h = Names::hash(c, start, end - start);
        shared.names.prefetch_slot(*h);
    }
    for (&h, &(g, ..)) in hashes.iter().zip(batch) {
        shared.insert(queues, g, shared.names.id(h));
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
    w & u64::MAX >> (64 - 8 * keep) // keep is 1..=8
}

/// `n` zeroed atomics from a zeroed allocation, so pages that are never written are
/// never touched and never charged to the run's memory.
fn zeroed_atomics(n: usize) -> Box<[AtomicU64]> {
    let words: Box<[u64]> = vec![0u64; n].into_boxed_slice();
    // SAFETY: AtomicU64 has the same size, alignment and bit validity as u64.
    unsafe { Box::from_raw(Box::into_raw(words) as *mut [AtomicU64]) }
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
        let ends = zero(w ^ ONES * b',' as u64) | zero(w ^ ONES * b'.' as u64);
        let n = ends.trailing_zeros() as usize / 8;
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

/// A space, then integer cents as `-?D+.DD`, never `-0.00`.
fn push_cents(out: &mut Vec<u8>, cents: i64) {
    let (at, abs) = (out.len(), cents.unsigned_abs());
    push_num(out, abs / 100);
    if cents < 0 {
        out.insert(at + 1, b'-'); // after the space
    }
    out.extend_from_slice(&[b'.', b'0' + (abs % 100 / 10) as u8, b'0' + (abs % 10) as u8]);
}

/// A space, then `v` in decimal.
fn push_num(out: &mut Vec<u8>, mut v: u64) {
    let mut buf = [b' '; 21];
    let digits = v.checked_ilog10().unwrap_or(0) as usize + 1;
    for k in (1..=digits).rev() {
        buf[k] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out.extend_from_slice(&buf[..=digits]);
}

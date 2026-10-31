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
//! and each country keeps a bitset of the ids it has seen, written only by the
//! thread that owns the country.

mod mmap;

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::thread;

/// FxHash step: one multiply per 8-byte word. Names are short and not adversarial.
fn mix(h: u64, w: u64) -> u64 {
    (h.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95)
}

const BOM: &[u8] = b"\xEF\xBB\xBF";
const NO_REGION: u8 = u8::MAX;
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
    let inboxes: Vec<_> = (0..threads).map(|_| Mutex::new(Vec::new())).collect();
    let seen = Seen {
        bits: &bits,
        inboxes: &inboxes,
        overflow: &overflow,
    };
    // Many chunks handed out on demand, so a slow core never holds up the rest.
    let chunks = split_records(records, threads * 8);
    let next = AtomicUsize::new(0);
    let aggs = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|me| {
                let (region, names, seen, chunks, next) = (&region, &names, &seen, &chunks, &next);
                s.spawn(move || {
                    let mut aggs = vec![Agg::EMPTY; groups];
                    let mut outbox = Outbox {
                        me,
                        bufs: vec![Vec::with_capacity(PAIRS); threads],
                    };
                    while let Some(&chunk) = chunks.get(next.fetch_add(1, Relaxed)) {
                        let first = std::ptr::eq(chunk, chunks[0]);
                        parse(chunk, first, region, &mut aggs, names, seen, &mut outbox);
                        seen.drain(me);
                    }
                    for o in 0..threads {
                        seen.flush(&mut outbox, o);
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
    // Pairs handed over after their owner's last chunk.
    thread::scope(|s| {
        for me in 0..threads {
            let seen = &seen;
            s.spawn(move || seen.drain(me));
        }
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

/// regions.csv: `region_id,country` lines after a header, ids ascending, codes
/// `[A-Z]{3}`. Returns the distinct country codes and a dense region id -> country
/// index table. Codes index a 26^3 table directly, so there is no hashing.
fn load_regions(text: &'static [u8]) -> (Vec<&'static [u8]>, Vec<u8>) {
    let mut codes: Vec<&[u8]> = Vec::new();
    let mut by_code = vec![NO_REGION; 26 * 26 * 26];
    // Ids ascend, so the last line holds the largest: size the table once.
    let body = text.strip_suffix(b"\n").unwrap_or(text);
    let last = body.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut region = vec![NO_REGION; digits(body, last).0 as usize + 1];
    let mut p = text
        .iter()
        .position(|&b| b == b'\n')
        .map_or(text.len(), |i| i + 1);
    while p + 4 < text.len() {
        let (id, comma) = digits(text, p);
        let code = &text[comma + 1..comma + 4];
        let key = code.iter().fold(0, |k, &b| k * 26 + (b - b'A') as usize);
        if by_code[key] == NO_REGION {
            assert!(codes.len() < NO_REGION as usize, "too many countries");
            by_code[key] = codes.len() as u8;
            codes.push(code);
        }
        region[id as usize] = by_code[key];
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
/// Where each (country, name id) pair is recorded: one bit per pair, in one table
/// for all threads. Country g's row belongs to thread `g % threads`, the only one that
/// writes it, so a bit is set with a plain load and store: no locked instruction and
/// no cache line moving between cores. Other threads hand pairs to the owner in blocks.
struct Seen<'a> {
    bits: &'a [AtomicU64],
    inboxes: &'a [Mutex<Vec<Vec<u32>>>],
    overflow: &'a Mutex<Vec<(usize, u32)>>,
}

/// Pairs per handed-over block.
const PAIRS: usize = 1024;

/// One thread's pairs waiting for each owner, packed `country << 17 | id`.
struct Outbox {
    me: usize,
    bufs: Vec<Vec<u32>>,
}

impl Seen<'_> {
    #[inline]
    fn send(&self, out: &mut Outbox, g: usize, id: u32) {
        if id as usize >= MAX_IDS {
            self.overflow.lock().unwrap().push((g, id));
            return;
        }
        let o = g % self.inboxes.len();
        out.bufs[o].push((g as u32) << MAX_IDS.trailing_zeros() | id);
        if out.bufs[o].len() == PAIRS {
            self.flush(out, o);
        }
    }

    fn flush(&self, out: &mut Outbox, o: usize) {
        let pairs = std::mem::replace(&mut out.bufs[o], Vec::with_capacity(PAIRS));
        if o == out.me {
            self.apply(&pairs);
        } else if !pairs.is_empty() {
            self.inboxes[o].lock().unwrap().push(pairs);
        }
    }

    /// Sets the bits of every pair handed to thread `me` so far.
    fn drain(&self, me: usize) {
        for pairs in std::mem::take(&mut *self.inboxes[me].lock().unwrap()) {
            self.apply(&pairs);
        }
    }

    /// Only the owner of the pairs' countries calls this.
    fn apply(&self, pairs: &[u32]) {
        for &v in pairs {
            let bit =
                (v as usize >> MAX_IDS.trailing_zeros()) * MAX_IDS + (v as usize & (MAX_IDS - 1));
            let word = &self.bits[bit / 64];
            word.store(word.load(Relaxed) | 1 << (bit % 64), Relaxed);
        }
    }
}

fn parse(
    c: &[u8],
    first: bool,
    region: &[u8],
    aggs: &mut [Agg],
    names: &Names,
    seen: &Seen,
    outbox: &mut Outbox,
) {
    let unmatched = aggs.len() - 1;
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
        if c[p..].starts_with(BOM) {
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

        let g = match region.get(region_id as usize) {
            Some(&r) if r != NO_REGION => r as usize,
            _ => unmatched,
        };
        let a = &mut aggs[g];
        a.count += 1;
        a.id_sum += id;
        a.amount_sum += cents;
        a.amount_min = a.amount_min.min(cents);
        a.amount_max = a.amount_max.max(cents);
        a.name_bytes += (name.len() - name.escapes) as u64;
        a.note_bytes += (note.len() - note.escapes) as u64;

        batch[n] = (g, name.start, name.end_content);
        n += 1;
        if n == BATCH {
            intern(c, &batch, names, seen, outbox);
            n = 0;
        }
    }
    intern(c, &batch[..n], names, seen, outbox);
}

/// Records per interning batch.
const BATCH: usize = 32;

/// Interns a batch of (country, name start, name end) and hands each pair to its
/// country's owner. Every lookup is a likely cache miss into the shared table, so it
/// goes in passes: hash and prefetch every slot, then resolve the ids. The misses of
/// one pass overlap instead of each record waiting for its own.
fn intern(
    c: &[u8],
    batch: &[(usize, usize, usize)],
    names: &Names,
    seen: &Seen,
    outbox: &mut Outbox,
) {
    let mut hashes = [0u64; BATCH];
    for (h, &(_, start, end)) in hashes.iter_mut().zip(batch) {
        *h = Names::hash(c, start, end - start);
        names.prefetch_slot(*h);
    }
    let mut ids = [0u32; BATCH];
    for ((id, &h), &(g, ..)) in ids.iter_mut().zip(&hashes).zip(batch) {
        *id = names.id(h);
        seen.send(outbox, g, *id);
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
const ID_MASK: u64 = (1 << ID_BITS) - 1;

impl Names {
    fn new() -> Names {
        Names {
            slots: zeroed_atomics(NAME_SLOTS),
            next_id: AtomicU32::new(0),
        }
    }

    /// The hash of the name `c[start..start + len]`: FxHash over its words, then a
    /// full avalanche (MurmurHash3's finaliser).
    fn hash(c: &[u8], start: usize, len: usize) -> u64 {
        let mut h =
            (0..len.div_ceil(8)).fold(len as u64, |h, k| mix(h, name_word(c, start, len, k)));
        h = (h ^ h >> 33).wrapping_mul(0xff51_afd7_ed55_8ccd);
        h = (h ^ h >> 33).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^ h >> 33
    }

    /// Starts loading the slot a name with hash `h` probes first.
    #[inline]
    fn prefetch_slot(&self, h: u64) {
        prefetch(&self.slots[h as usize & (NAME_SLOTS - 1)]);
    }

    /// The dense id of the name with hash `h`. A thread that finds an empty slot takes
    /// a fresh id and claims the slot with one compare-exchange; if another thread
    /// claimed it first, that id is simply never used (bits are only set for ids in
    /// slots, so a skipped id changes no count).
    fn id(&self, h: u64) -> u32 {
        let tag = h & !ID_MASK;
        let mut i = h as usize & (NAME_SLOTS - 1);
        loop {
            let slot = &self.slots[i];
            let mut v = slot.load(Acquire);
            if v == 0 {
                let new = self.next_id.fetch_add(1, Relaxed);
                assert!((new as u64) < ID_MASK, "too many distinct names");
                match slot.compare_exchange(0, tag | (new as u64 + 1), AcqRel, Acquire) {
                    Ok(_) => return new,
                    Err(found) => v = found,
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

/// Reads a quoted or unquoted text field starting at `p`.
#[inline]
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

/// Position of the first `byte` at or after `p`, eight bytes at a time.
#[inline]
fn find(c: &[u8], mut p: usize, byte: u8) -> usize {
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

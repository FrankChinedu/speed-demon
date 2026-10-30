//! Track C: Join. Parse a hostile CSV, join each record to its region's country,
//! and print per-country aggregates.
//!
//! Splitting a CSV for threads is normally unsafe because a newline inside quotes
//! is data. Under the spec's grammar it is decidable: unquoted fields never contain
//! `"` and every `""` escape is a pair, so a byte is inside quotes exactly when an odd
//! number of `"` bytes precede it. Each thread counts quotes in its slice, a prefix
//! sum gives the quote state at every split guess, and each chunk then starts just
//! after the first record terminator outside quotes.
//!
//! Each thread parses its records with a byte-level state machine straight from the
//! map and keeps running sums per country. Distinct names need no unescaping: two
//! name fields have equal values exactly when their raw bytes are equal (an unquoted
//! field cannot hold `"`, and `""` -> `"` is one-to-one). All threads intern raw names
//! in one lock-free table that hands out dense ids, and each country keeps a bitset
//! of the ids it has seen.

mod mmap;

use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::thread;

/// FxHash: one multiply per 8 bytes. Names and codes are short and not adversarial.
#[derive(Default)]
struct Fx(u64);

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.0 =
                (self.0.rotate_left(5) ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(K);
        }
        let mut tail = [0u8; 8];
        tail[..chunks.remainder().len()].copy_from_slice(chunks.remainder());
        self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(tail)).wrapping_mul(K);
    }

    fn finish(&self) -> u64 {
        self.0 ^ self.0 >> 29
    }
}

const BOM: &[u8] = b"\xEF\xBB\xBF";
const NO_REGION: u16 = u16::MAX;
/// Name table slots. The spec's name pool is 100,000 values; the table stays under half full.
const NAME_SLOTS: usize = 1 << 19;
/// Name ids that get a bit in each country's bitset (the spec's pool is 100,000).
/// Rarer ids beyond it go to an overflow list, so the count stays exact regardless.
const MAX_IDS: usize = 1 << 17;
/// Arena for distinct names: about 2 MB is used; only touched pages are charged.
const ARENA_BYTES: usize = 64 << 20;

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

    let names = Names::new(ARENA_BYTES.min(records.len() + 4 * (records.len() / 8 + 1)));
    let seen = zeroed_atomics(groups * MAX_IDS / 64);
    let overflow = Mutex::new(Vec::new());
    // Many chunks handed out on demand, so a slow core never holds up the rest.
    let chunks = split_records(records, threads * 8);
    let next = AtomicUsize::new(0);
    let aggs: Vec<Agg> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (region, names, seen, overflow, chunks, next) =
                    (&region, &names, &seen, &overflow, &chunks, &next);
                s.spawn(move || {
                    let mut aggs = vec![Agg::EMPTY; groups];
                    loop {
                        let k = next.fetch_add(1, Relaxed);
                        let Some(&chunk) = chunks.get(k) else {
                            break aggs;
                        };
                        let sink = Seen {
                            bits: seen,
                            overflow,
                        };
                        parse(chunk, k == 0, region, &mut aggs, names, &sink);
                    }
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

    // Country codes are [A-Z]{3}, all below `_`, so _UNMATCHED stays last.
    let mut order: Vec<usize> = (0..codes.len()).collect();
    order.sort_unstable_by_key(|&g| codes[g]);
    order.push(codes.len());

    let used_words = (names.next_id.load(Relaxed) as usize)
        .min(MAX_IDS)
        .div_ceil(64);
    let mut overflow = overflow.into_inner().unwrap();
    overflow.sort_unstable();
    overflow.dedup();
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
        let words = &seen[g * MAX_IDS / 64..][..used_words];
        let distinct = words
            .iter()
            .map(|w| w.load(Relaxed).count_ones())
            .sum::<u32>()
            + overflow.iter().filter(|&&(og, _)| og == g).count() as u32;
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

/// regions.csv: `region_id,country` lines after a header. Returns the distinct
/// country codes and a dense region id -> country index table.
fn load_regions(text: &'static [u8]) -> (Vec<&'static [u8]>, Vec<u16>) {
    let mut codes: Vec<&[u8]> = Vec::new();
    let mut index: HashMap<&[u8], u16, BuildHasherDefault<Fx>> = HashMap::default();
    let mut region = Vec::new();
    for line in text
        .split(|&b| b == b'\n')
        .skip(1)
        .filter(|l| !l.is_empty())
    {
        let comma = line.iter().position(|&b| b == b',').expect("region line");
        let id = line[..comma]
            .iter()
            .fold(0usize, |acc, &b| acc * 10 + (b - b'0') as usize);
        let code = &line[comma + 1..];
        let c = *index.entry(code).or_insert_with(|| {
            codes.push(code);
            codes.len() as u16 - 1
        });
        if region.len() <= id {
            region.resize(id + 1, NO_REGION);
        }
        region[id] = c;
    }
    (codes, region)
}

/// Splits the file into `n` chunks that each start at a record boundary.
fn split_records(data: &'static [u8], n: usize) -> Vec<&'static [u8]> {
    let guesses: Vec<usize> = (0..=n).map(|k| data.len() * k / n).collect();
    let quotes: Vec<usize> = thread::scope(|s| {
        let handles: Vec<_> = guesses
            .windows(2)
            .map(|w| {
                let part = &data[w[0]..w[1]];
                s.spawn(move || part.iter().filter(|&&b| b == b'"').count())
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut bounds = vec![0];
    let mut before = 0;
    for k in 1..n {
        before += quotes[k - 1];
        // Walk forward from the guess until a terminator LF that is outside quotes.
        let mut inside = before % 2 == 1;
        let mut end = data.len();
        for (i, &b) in data[guesses[k]..].iter().enumerate() {
            if b == b'"' {
                inside = !inside;
            } else if b == b'\n' && !inside {
                end = guesses[k] + i + 1;
                break;
            }
        }
        bounds.push(end.max(*bounds.last().unwrap()));
    }
    bounds.push(data.len());
    bounds.windows(2).map(|w| &data[w[0]..w[1]]).collect()
}

/// Parses whole records (section 2.1 of the spec) and aggregates them.
/// Where each (country, name id) pair is recorded.
struct Seen<'a> {
    bits: &'a [AtomicU64],
    overflow: &'a Mutex<Vec<(usize, u32)>>,
}

impl Seen<'_> {
    #[inline]
    fn insert(&self, g: usize, id: u32) {
        if (id as usize) < MAX_IDS {
            let bit = g * MAX_IDS + id as usize;
            self.bits[bit / 64].fetch_or(1 << (bit % 64), Relaxed);
        } else {
            self.overflow.lock().unwrap().push((g, id));
        }
    }
}

fn parse(c: &[u8], first: bool, region: &[u16], aggs: &mut [Agg], names: &Names, seen: &Seen) {
    let unmatched = (aggs.len() - 1) as u16;
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
            _ => unmatched as usize,
        };
        let a = &mut aggs[g];
        a.count += 1;
        a.id_sum += id;
        a.amount_sum += cents;
        a.amount_min = a.amount_min.min(cents);
        a.amount_max = a.amount_max.max(cents);
        a.name_bytes += (name.len() - name.escapes) as u64;
        a.note_bytes += (note.len() - note.escapes) as u64;

        seen.insert(g, names.id(&c[name.start..name.end_content]));
    }
}

/// Lock-free interning of raw name bytes into dense ids, shared by all threads.
///
/// A slot is 0 when empty, else `arena offset << 32 | length << 16 | hash tag`. An
/// arena entry is a 4-byte id followed by the name. A thread that finds an empty
/// slot writes its entry first and then publishes it with one compare-exchange, so a
/// reader that sees the slot also sees the bytes. If two threads race for one slot,
/// the loser re-reads the winner's entry, and only a little arena space and an id
/// number are wasted (the id is never set in any bitset, so it counts nothing).
struct Names {
    slots: Box<[AtomicU64]>,
    arena: Box<[AtomicU8]>,
    used: AtomicUsize,
    next_id: AtomicU32,
}

impl Names {
    fn new(arena_bytes: usize) -> Names {
        Names {
            slots: zeroed_atomics(NAME_SLOTS),
            arena: {
                let bytes: Box<[u8]> = vec![0u8; arena_bytes].into_boxed_slice();
                // SAFETY: AtomicU8 has the same size, alignment and bit validity as u8.
                unsafe { Box::from_raw(Box::into_raw(bytes) as *mut [AtomicU8]) }
            },
            used: AtomicUsize::new(0),
            next_id: AtomicU32::new(0),
        }
    }

    /// The dense id of the name whose raw field bytes are `raw`.
    fn id(&self, raw: &[u8]) -> u32 {
        let mut h = Fx::default();
        h.write(raw);
        let h = h.finish();
        let tag = (h >> 48) | 1; // never 0, so a filled slot is never 0
        let mut i = h as usize & (NAME_SLOTS - 1);
        let mut mine = None;
        loop {
            let s = self.slots[i].load(Acquire);
            if s == 0 {
                let entry = *mine.get_or_insert_with(|| self.store(raw, tag));
                match self.slots[i].compare_exchange(0, entry, AcqRel, Acquire) {
                    Ok(_) => return self.id_at(entry),
                    Err(_) => continue, // lost the race: look at what was published
                }
            }
            if s & 0xffff == tag && self.equals(s, raw) {
                return self.id_at(s);
            }
            i = (i + 1) & (NAME_SLOTS - 1);
        }
    }

    fn store(&self, raw: &[u8], tag: u64) -> u64 {
        assert!(raw.len() < 1 << 16, "name too long");
        let off = self.used.fetch_add(4 + raw.len(), Relaxed);
        let id = self.next_id.fetch_add(1, Relaxed);
        assert!(off + 4 + raw.len() <= self.arena.len(), "name arena full");
        for (dst, &b) in self.arena[off..]
            .iter()
            .zip(id.to_le_bytes().iter().chain(raw))
        {
            dst.store(b, Relaxed);
        }
        (off as u64) << 32 | (raw.len() as u64) << 16 | tag
    }

    fn equals(&self, s: u64, raw: &[u8]) -> bool {
        let (off, len) = ((s >> 32) as usize + 4, (s >> 16 & 0xffff) as usize);
        len == raw.len()
            && self.arena[off..off + len]
                .iter()
                .zip(raw)
                .all(|(a, &b)| a.load(Relaxed) == b)
    }

    fn id_at(&self, s: u64) -> u32 {
        let off = (s >> 32) as usize;
        u32::from_le_bytes(std::array::from_fn(|k| self.arena[off + k].load(Relaxed)))
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

/// An unsigned decimal at `p`; returns it and the position of the byte after it.
#[inline]
fn digits(c: &[u8], mut p: usize) -> (u64, usize) {
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

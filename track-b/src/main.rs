//! Track B: Index. Answer 50,000 BOOL and PHRASE queries over a 250,000-document corpus.
//!
//! The queries are known before the corpus is read, so only their terms are indexed:
//! 1. Parse the queries and give every distinct term a slot in a compact hash table.
//! 2. One pass over the corpus, in chunks of consecutive documents. Each document is
//!    tokenized into term slots (tokens no query uses become NONE). Phrases are
//!    matched right there, each only where its least-used term occurs, so no
//!    positions are ever stored. Each document's distinct BOOL terms become postings,
//!    scattered at the end of the chunk into 16-bit offsets from the chunk's first
//!    document, or bitmaps for terms in more than 1/64 of its documents.
//! 3. As soon as a chunk is indexed, every BOOL query is evaluated on it, strictly
//!    left to right, while the index is in cache; then the index is dropped. Each
//!    thread keeps a running summary of every BOOL query, and the threads' summaries
//!    are merged per query at the end.

mod mmap;

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread;

/// A token that no query uses, or a term that is not in any BOOL query.
const NONE: u32 = u32::MAX;
/// No query term is longer than 32 bytes, so a longer token can never match.
const MAX_TERM: usize = 32;

/// Token bytes are `a-z` and `0-9` after ASCII lowercasing; every other byte separates.
const CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let b = i as u8;
        t[i] = match b {
            b'a'..=b'z' | b'0'..=b'9' => b,
            b'A'..=b'Z' => b + 32,
            _ => 0,
        };
        i += 1;
    }
    t
};

#[derive(Clone, Copy)]
enum Op {
    And,
    Or,
    Not,
}

enum Query {
    /// First BOOL term, then (operator, BOOL term) pairs applied left to right.
    Bool(u32, Vec<(Op, u32)>),
    Phrase(usize),
}

/// Everything known from queries.txt before the corpus is read.
struct Plan {
    table: Table,
    n_bool: usize,
    queries: Vec<Query>,
    phrases: Vec<Vec<u32>>,
    /// Each phrase is checked only where its key term occurs: the phrase's term used
    /// by the fewest queries, a proxy for the rarest in the corpus (almost every
    /// phrase is built around a rare anchor). A term's keyed phrases, with the key's
    /// position inside each, are `keyed[key_first[t]..key_first[t + 1]]`, where t is
    /// the term's slot in the table (which is also its id everywhere after the plan).
    keyed: Vec<(u32, u32)>,
    key_first: Vec<u32>,
}

/// Posting sets of the BOOL terms for one chunk of documents, as offsets from `base`.
/// A sparse term is an ascending list, `docs[offsets[b]..offsets[b + 1]]`. A term in
/// more than 1/16 of the chunk's documents is a bitmap instead (never larger than
/// its list would be), listed in `dense` by term with its first word in `bits`, and
/// marked in `is_dense`, so a term absent from the chunk (most rare terms, in most
/// chunks) is answered without searching `dense`.
struct ChunkIndex {
    base: u32,
    words: usize,
    offsets: Vec<u32>,
    docs: Vec<u16>,
    dense: Vec<(u32, u32)>,
    is_dense: Vec<u64>,
    bits: Vec<u64>,
}

#[derive(Clone, Copy)]
enum Set<'a> {
    List(&'a [u16]),
    Bits(&'a [u64]),
}

impl ChunkIndex {
    #[inline]
    fn set(&self, b: u32) -> Set<'_> {
        let list =
            &self.docs[self.offsets[b as usize] as usize..self.offsets[b as usize + 1] as usize];
        if !list.is_empty() || !has(&self.is_dense, b as usize) {
            return Set::List(list);
        }
        match self.dense.binary_search_by_key(&b, |&(t, _)| t) {
            Ok(i) => Set::Bits(&self.bits[self.dense[i].1 as usize..][..self.words]),
            Err(_) => Set::List(list),
        }
    }
}

/// The running result of a BOOL query inside one chunk, kept as a list or a bitmap
/// depending on what it last met. Buffers are reused across queries.
#[derive(Default)]
struct Acc {
    is_bits: bool,
    list: Vec<u16>,
    tmp: Vec<u16>,
    bits: Vec<u64>,
}

#[inline]
fn has(bits: &[u64], d: impl Into<usize>) -> bool {
    let d = d.into();
    bits[d / 64] >> (d % 64) & 1 == 1
}

impl Acc {
    fn start(&mut self, s: Set) {
        match s {
            Set::List(l) => {
                self.is_bits = false;
                self.list.clear();
                self.list.extend_from_slice(l);
            }
            Set::Bits(b) => {
                self.is_bits = true;
                self.bits.clear();
                self.bits.extend_from_slice(b);
            }
        }
    }

    /// `self = self op s`.
    fn apply(&mut self, op: Op, s: Set) {
        match (self.is_bits, s) {
            (false, Set::List(l)) => {
                apply(op, &self.list, l, &mut self.tmp);
                std::mem::swap(&mut self.list, &mut self.tmp);
            }
            (false, Set::Bits(b)) => match op {
                Op::And => self.list.retain(|&d| has(b, d)),
                Op::Not => self.list.retain(|&d| !has(b, d)),
                Op::Or => {
                    self.bits.clear();
                    self.bits.extend_from_slice(b);
                    for &d in &self.list {
                        self.bits[d as usize / 64] |= 1 << (d % 64);
                    }
                    self.is_bits = true;
                }
            },
            (true, Set::List(l)) => match op {
                Op::And => {
                    self.list.clear();
                    self.list
                        .extend(l.iter().copied().filter(|&d| has(&self.bits, d)));
                    self.is_bits = false;
                }
                Op::Or => l
                    .iter()
                    .for_each(|&d| self.bits[d as usize / 64] |= 1 << (d % 64)),
                Op::Not => l
                    .iter()
                    .for_each(|&d| self.bits[d as usize / 64] &= !(1 << (d % 64))),
            },
            (true, Set::Bits(b)) => {
                for (x, &y) in self.bits.iter_mut().zip(b) {
                    *x = match op {
                        Op::And => *x & y,
                        Op::Or => *x | y,
                        Op::Not => *x & !y,
                    };
                }
            }
        }
    }

    /// Feeds the chunk's result to `out`, ascending.
    fn emit(&self, base: u32, out: &mut Summary) {
        emit(
            if self.is_bits {
                Set::Bits(&self.bits)
            } else {
                Set::List(&self.list)
            },
            base,
            out,
        );
    }
}

fn emit(s: Set, base: u32, out: &mut Summary) {
    // Documents one by one until the ten smallest are known, then only the count and
    // sum, in bulk: a list's sum vectorises, and a bitmap word adds its popcount and
    // the sum of its set bit positions (six masked popcounts).
    const POS: [u64; 6] = [
        0xaaaa_aaaa_aaaa_aaaa,
        0xcccc_cccc_cccc_cccc,
        0xf0f0_f0f0_f0f0_f0f0,
        0xff00_ff00_ff00_ff00,
        0xffff_0000_ffff_0000,
        0xffff_ffff_0000_0000,
    ];
    match s {
        Set::List(l) => {
            let k = l.len().min(10 - out.count().min(10) as usize);
            l[..k].iter().for_each(|&d| out.add(base + d as u32));
            let rest = &l[k..];
            let n = rest.len() as u64;
            out.packed += (n << 40) + base as u64 * n + rest.iter().map(|&d| d as u64).sum::<u64>();
        }
        Set::Bits(b) => {
            for (k, &w) in b.iter().enumerate() {
                let (mut w, at) = (w, base + (k * 64) as u32);
                while w != 0 && out.count() < 10 {
                    out.add(at + w.trailing_zeros());
                    w &= w - 1;
                }
                let ones = w.count_ones();
                let pos: u32 = (0..6).map(|j| (w & POS[j]).count_ones() << j).sum();
                out.packed += ((ones as u64) << 40) + ones as u64 * at as u64 + pos as u64;
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().unwrap_or_else(|| "/data".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/out".into()));
    mmap::one_arena();
    let plan = plan(mmap::map(&data.join("queries.txt")));
    let docs = mmap::map(&data.join("docs.txt"));
    let threads = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(12);
    // Six chunks per thread. Queries are evaluated per chunk and the index dropped,
    // so smaller chunks mean smaller per-thread buffers but more evaluations (board,
    // with shared summaries: four 0.240 s / 62 MB, six 0.244 s / 47 MB, eight
    // 0.267 s / 41 MB). About 3.5 MB and 3,500 documents each at full scale, well
    // inside the 16-bit offsets.
    let chunks = split_lines(docs, (threads * 6).max(docs.len() >> 23));

    // The BOOL queries, numbered in input order.
    let bools: Vec<(u32, &[(Op, u32)])> = (plan.queries.iter())
        .filter_map(|q| match q {
            Query::Bool(first, rest) => Some((*first, &rest[..])),
            Query::Phrase(_) => None,
        })
        .collect();

    // The single pass over the corpus. Chunks are handed out dynamically. As soon as a
    // chunk is indexed, every BOOL query is evaluated on it while its index is still
    // in cache, its result added to the query's shared summary, and the index dropped.
    // Each thread starts at a different query, so threads rarely meet on one summary.
    let shared: Vec<Shared> = bools.iter().map(|_| Shared::default()).collect();
    let next = AtomicUsize::new(0);
    let partial: Vec<Vec<(u32, u32)>> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (plan, chunks, next, bools, shared) = (&plan, &chunks, &next, &bools, &shared);
                s.spawn(move || {
                    let start = t * bools.len() / threads;
                    let mut matches = Vec::new();
                    let mut acc = Acc::default();
                    let mut ids = Vec::with_capacity(1024);
                    let mut scratch = Scratch::default();
                    // The chunk's postings, `doc offset << 16 | BOOL index` (there are at
                    // most 65,536 BOOL terms), split by term range; and each term's count.
                    let mut postings = Postings {
                        groups: vec![Vec::new(); GROUPS],
                        seen: vec![(u16::MAX, 0); plan.n_bool + 1],
                        shift: (plan.n_bool.max(1) - 1)
                            .checked_ilog2()
                            .map_or(0, |b| b + 1)
                            .saturating_sub(GROUPS.ilog2()),
                    };
                    while let Some(&chunk) = chunks.get(next.fetch_add(1, Relaxed)) {
                        let mut keys: Vec<(u32, u32, u32)> = Vec::new();
                        let mut n_docs = 0;
                        let mut base = None;
                        for_each_doc(chunk, |doc, text| {
                            let offset = doc - *base.get_or_insert(doc);
                            n_docs += 1;
                            ids.clear();
                            keys.clear();
                            tokenize(text, &plan.table, &mut scratch, &mut ids);
                            n_docs += std::hint::black_box(ids.len()) & 0; // TIMING PROBE
                            if std::hint::black_box(true) { return; } // TIMING PROBE
                            // Each token's BOOL posting (once per document) and whether a
                            // phrase is keyed on it, from its slot.
                            for (i, &t) in ids.iter().enumerate().filter(|(_, t)| **t != NONE) {
                                let e = plan.table.slots[t as usize];
                                let b = (e & T_NO_BOOL) as u32;
                                if b != T_NO_BOOL as u32 {
                                    postings.add(offset, b);
                                }
                                if e & T_KEYED != 0 {
                                    let k = &plan.key_first[t as usize..];
                                    keys.push((i as u32, k[0], k[1]));
                                }
                            }
                            match_phrases(plan, &ids, &keys, doc, &mut matches);
                        });
                        assert!(n_docs < 1 << 16, "chunk too large for 16-bit offsets");
                        let ci = postings.index(base.unwrap_or(0), n_docs);
                        for k in 0..bools.len() {
                            let q = (k + start) % bools.len();
                            let (first, rest) = bools[q];
                            let mut result = Summary::default();
                            if rest.is_empty() {
                                emit(ci.set(first), ci.base, &mut result);
                            } else {
                                acc.start(ci.set(first));
                                for &(op, t) in rest {
                                    acc.apply(op, ci.set(t));
                                }
                                acc.emit(ci.base, &mut result);
                            }
                            shared[q].add(&result);
                        }
                    }
                    matches
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    std::fs::write(out.join("results.txt"), b"").unwrap(); // TIMING PROBE
    std::process::exit(0);
    // A phrase can match one document at several positions; count the document once.
    let mut matches: Vec<(u32, u32)> = partial.into_iter().flatten().collect();
    matches.sort_unstable();
    matches.dedup();
    let phrase_docs = |p: usize| {
        let lo = matches.partition_point(|&(q, _)| (q as usize) < p);
        let hi = matches.partition_point(|&(q, _)| (q as usize) <= p);
        matches[lo..hi].iter().map(|&(_, d)| d)
    };

    // Merge the threads' summaries and format, in blocks handed out dynamically;
    // blocks are written back in order. `nth_bool[i]` numbers query i among the BOOLs.
    let nth_bool: Vec<usize> = (plan.queries.iter())
        .scan(0, |k, q| {
            *k += matches!(q, Query::Bool(..)) as usize;
            Some(*k - 1)
        })
        .collect();
    const BLOCK: usize = 1024;
    let n_blocks = plan.queries.len().div_ceil(BLOCK);
    let next = AtomicUsize::new(0);
    let mut blocks: Vec<(usize, Vec<u8>)> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (plan, next, shared, phrase_docs, nth_bool) =
                    (&plan, &next, &shared, &phrase_docs, &nth_bool);
                s.spawn(move || {
                    let mut done = Vec::new();
                    loop {
                        let k = next.fetch_add(1, Relaxed);
                        if k >= n_blocks {
                            break;
                        }
                        let mut buf = Vec::with_capacity(BLOCK * 40);
                        for i in k * BLOCK..((k + 1) * BLOCK).min(plan.queries.len()) {
                            let mut result = Summary::default();
                            match plan.queries[i] {
                                Query::Phrase(p) => phrase_docs(p).for_each(|d| result.add(d)),
                                Query::Bool(..) => result = shared[nth_bool[i]].get(),
                            }
                            result.write(&mut buf);
                        }
                        done.push((k, buf));
                    }
                    done
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    blocks.sort_unstable_by_key(|&(k, _)| k);

    let mut file = File::create(out.join("results.txt")).expect("create results.txt");
    for (_, buf) in &blocks {
        file.write_all(buf).expect("write results.txt");
    }
}

/// Term ranges the postings are split into while they are collected.
const GROUPS: usize = 64;

/// One thread's postings for the chunk it is reading, kept by term range: scattering
/// them into per-term lists then touches one range's lists at a time, a few KB that
/// stay in cache, instead of all ~50,000 lists at once.
struct Postings {
    groups: Vec<Vec<u32>>,
    /// Per BOOL term: the offset of the last document of the chunk that gave it a
    /// posting (u16::MAX before the first), and its count in the chunk, side by side
    /// in 4 bytes so a token touches one small cache line. The spare last entry is
    /// never written.
    seen: Vec<(u16, u16)>,
    shift: u32,
}

impl Postings {
    /// Records that the document at `offset` in the chunk contains BOOL term `b`.
    #[inline]
    fn add(&mut self, offset: u32, b: u32) {
        debug_assert!((b as usize) < self.seen.len() - 1);
        // SAFETY: `b` is a BOOL index from the term table, below n_bool, and `seen`
        // has n_bool + 1 entries.
        let seen = unsafe { self.seen.get_unchecked_mut(b as usize) };
        if seen.0 != offset as u16 {
            *seen = (offset as u16, seen.1 + 1);
            self.groups[b as usize >> self.shift].push(offset << 16 | b);
        }
    }

    /// Turns the chunk's postings into its index and empties the buffers. Documents
    /// were visited in order, so every list comes out ascending.
    fn index(&mut self, base: u32, n_docs: usize) -> ChunkIndex {
        let n_bool = self.seen.len() - 1;
        let words = n_docs.div_ceil(64);
        // Dense terms (in more than 1/64 of the documents) become bitmaps and get an
        // empty list range.
        let mut offsets = vec![0u32; n_bool + 1];
        let mut dense = Vec::new();
        let mut is_dense = vec![0u64; n_bool.div_ceil(64)];
        let mut slot = vec![u32::MAX; n_bool];
        let mut total = 0;
        for (b, (last, c)) in self.seen[..n_bool].iter_mut().enumerate() {
            offsets[b] = total;
            if *c as usize * 64 > n_docs {
                slot[b] = (dense.len() * words) as u32;
                dense.push((b as u32, slot[b]));
                is_dense[b / 64] |= 1 << (b % 64);
            } else {
                total += *c as u32;
            }
            (*last, *c) = (u16::MAX, 0);
        }
        offsets[n_bool] = total;
        let mut cursor = offsets.clone();
        let mut docs = vec![0u16; total as usize];
        let mut bits = vec![0u64; dense.len() * words];
        for group in &mut self.groups {
            for &p in group.iter() {
                let (d, b) = ((p >> 16) as usize, (p & 0xffff) as usize);
                match slot[b] {
                    u32::MAX => {
                        docs[cursor[b] as usize] = d as u16;
                        cursor[b] += 1;
                    }
                    w => bits[w as usize + d / 64] |= 1 << (d % 64),
                }
            }
            group.clear();
        }
        ChunkIndex {
            base,
            words,
            offsets,
            docs,
            dense,
            is_dense,
            bits,
        }
    }
}

/// Parses queries.txt and assigns term ids.
fn plan(text: &'static [u8]) -> Plan {
    // Query words go straight into the token table, keyed exactly as the tokenizer
    // will key corpus tokens (terms are already lowercase).
    let mut terms = Terms::default();
    let mut id = |w: &[u8]| {
        assert!(w.len() <= MAX_TERM, "query term longer than 32 bytes");
        terms.insert(key_at(
            text,
            w.as_ptr() as usize - text.as_ptr() as usize,
            w.len(),
        ))
    };
    let mut raw_bool: Vec<(u32, Vec<(Op, u32)>)> = Vec::new();
    let mut queries = Vec::new();
    let mut phrases: Vec<Vec<u32>> = Vec::new();
    for line in text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix(b"BOOL ") {
            let mut words = rest.split(|&b| b == b' ');
            let first = id(words.next().unwrap());
            let mut ops = Vec::new();
            while let (Some(op), Some(w)) = (words.next(), words.next()) {
                let op = match op {
                    b"AND" => Op::And,
                    b"OR" => Op::Or,
                    b"NOT" => Op::Not,
                    _ => panic!("bad operator in {}", String::from_utf8_lossy(line)),
                };
                ops.push((op, id(w)));
            }
            raw_bool.push((first, ops));
            queries.push(Query::Bool(0, Vec::new()));
        } else {
            let inner = line
                .strip_prefix(b"PHRASE \"")
                .and_then(|r| r.strip_suffix(b"\""))
                .expect("bad query");
            phrases.push(inner.split(|&b| b == b' ').map(&mut id).collect());
            queries.push(Query::Phrase(phrases.len() - 1));
        }
    }

    // BOOL terms get dense indices so each chunk's offset table only covers them.
    let n_terms = terms.count as usize;
    let mut bool_index = vec![NONE; n_terms];
    let mut n_bool = 0;
    let mut dense = |t: u32| {
        if bool_index[t as usize] == NONE {
            bool_index[t as usize] = n_bool;
            n_bool += 1;
        }
        bool_index[t as usize]
    };
    let raw_terms: Vec<u32> = raw_bool
        .iter()
        .flat_map(|(f, ops)| std::iter::once(*f).chain(ops.iter().map(|&(_, t)| t)))
        .collect();
    let mut raw_bool = raw_bool.into_iter();
    for q in queries.iter_mut() {
        if let Query::Bool(..) = q {
            let (first, ops) = raw_bool.next().unwrap();
            *q = Query::Bool(
                dense(first),
                ops.into_iter().map(|(op, t)| (op, dense(t))).collect(),
            );
        }
    }

    // Query frequency of every term, then each phrase's key: its least used term.
    let mut uses = vec![0u32; n_terms];
    for q in &queries {
        if let Query::Phrase(p) = q {
            phrases[*p].iter().for_each(|&t| uses[t as usize] += 1);
        }
    }
    for t in raw_terms {
        uses[t as usize] += 1;
    }
    let keys: Vec<(u32, u32, u32)> = phrases
        .iter()
        .enumerate()
        .map(|(p, terms)| {
            let at = (0..terms.len())
                .min_by_key(|&j| uses[terms[j] as usize])
                .unwrap();
            (terms[at], p as u32, at as u32)
        })
        .collect();
    // The corpus pass reads a compact copy of the table, and from here on a term is
    // known by its slot: phrases and phrase keys are renumbered to match.
    let cap = terms.slots.len();
    let mut slot_of = vec![0u32; n_terms];
    for (i, s) in terms.slots.iter().enumerate().filter(|(_, s)| s.hash != 0) {
        slot_of[s.id as usize] = i as u32;
    }
    let mut key_first = vec![0u32; cap + 1];
    keys.iter()
        .for_each(|&(t, ..)| key_first[slot_of[t as usize] as usize + 1] += 1);
    for t in 0..cap {
        key_first[t + 1] += key_first[t];
    }
    let mut cursor = key_first.clone();
    let mut keyed = vec![(0, 0); keys.len()];
    for &(t, p, at) in &keys {
        let t = slot_of[t as usize] as usize;
        keyed[cursor[t] as usize] = (p, at);
        cursor[t] += 1;
    }
    let slots = (terms.slots.iter().enumerate())
        .map(|(i, s)| match s.hash {
            0 => 0,
            h => {
                let keyed = if key_first[i] != key_first[i + 1] { T_KEYED } else { 0 };
                Table::tag(h) | (bool_index[s.id as usize] as u64).min(T_NO_BOOL) | keyed
            }
        })
        .collect();
    let table = Table {
        mask: terms.mask,
        slots,
    };
    for terms in &mut phrases {
        terms.iter_mut().for_each(|t| *t = slot_of[*t as usize]);
    }
    Plan {
        table,
        n_bool: {
            assert!(n_bool <= 1 << 16, "BOOL term indices are 16-bit");
            n_bool as usize
        },
        queries,
        phrases,
        keyed,
        key_first,
    }
}

/// Records (phrase, doc) for every phrase found in the document's token ids, checking
/// each phrase only where its key term occurs.
fn match_phrases(
    plan: &Plan,
    ids: &[u32],
    keys: &[(u32, u32, u32)],
    doc: u32,
    matches: &mut Vec<(u32, u32)>,
) {
    for &(i, lo, hi) in keys {
        let i = i as usize;
        for &(p, at) in &plan.keyed[lo as usize..hi as usize] {
            let terms = &plan.phrases[p as usize];
            if let Some(start) = i.checked_sub(at as usize) {
                if ids[start..].starts_with(terms) {
                    matches.push((p, doc));
                }
            }
        }
    }
}

/// Calls `f(doc_id, text)` for every `id TAB text LF` line.
fn for_each_doc(chunk: &[u8], mut f: impl FnMut(u32, &[u8])) {
    let mut p = 0;
    while p < chunk.len() {
        let line = &chunk[p..];
        let tab = line
            .iter()
            .position(|&b| b == b'\t')
            .expect("TAB after the id");
        let eol = tab + find_newline(&line[tab..]);
        let doc = line[..tab]
            .iter()
            .fold(0u32, |acc, &b| acc * 10 + (b - b'0') as u32);
        assert!(doc < 1 << 20, "document ids are packed in 20 bits");
        f(doc, &line[tab + 1..eol]);
        p += eol + 1;
    }
}

/// Position of the first LF in `s` (or `s.len()`), 32 bytes at a time.
#[inline]
fn find_newline(s: &[u8]) -> usize {
    let mut p = 0;
    while let Some(block) = s.get(p..p + 32) {
        let m = newline_mask(block.try_into().unwrap());
        if m != 0 {
            return p + m.trailing_zeros() as usize;
        }
        p += 32;
    }
    p + s[p..]
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(s.len() - p)
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
fn newline_mask(block: &[u8; 32]) -> u32 {
    use std::arch::x86_64::*;
    // SAFETY: AVX2 is enabled at compile time, and the load reads the 32-byte array.
    unsafe {
        let v = _mm256_loadu_si256(block.as_ptr() as *const __m256i);
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8))) as u32
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline]
fn newline_mask(block: &[u8; 32]) -> u32 {
    block
        .iter()
        .enumerate()
        .fold(0, |m, (i, &b)| m | ((b == b'\n') as u32) << i)
}

/// Bytes 0xFF then 0x00: the word at `KEEP[32 - len + 8 * w..]` keeps the bytes of
/// word w that lie inside a token of `len` bytes.
static KEEP: [u8; 64] = {
    let mut keep = [0; 64];
    let mut i = 0;
    while i < 32 {
        keep[i] = 0xff;
        i += 1;
    }
    keep
};

/// A token, zero-padded to 32 bytes and read as four words, so hashing and comparing
/// are a few word operations instead of a byte loop and a `memcmp` through a pointer.
type Key = [u64; 4];

/// A query term while the plan is built: its hash and its id in order of appearance.
#[derive(Clone, Copy, Default)]
struct Slot {
    hash: u64,
    id: u32,
}

/// The table the corpus pass reads: the plan's table copied slot for slot, one word
/// per slot, `tag | info`. The tag is the hash's top 46 bits (bit 63 forced on, so a
/// filled slot is never 0); `info` is the term's dense BOOL index (or T_NO_BOOL) with
/// T_KEYED set if phrases are keyed on it. Eight slots share a cache line, so the
/// ~75,000 terms of a full query set take 1 MB, which fits the box's L2.
///
/// A corpus token is mistaken for a query term only if it shares the 45 free tag bits
/// and its home slot (the hash's low 17 bits, independent of the tag) falls in the
/// short run of slots probed: about 2^-61 per pair, or ~2 * 10^-8 per run for the
/// spec's 600,000 distinct tokens and 75,000 query terms.
struct Table {
    mask: usize,
    slots: Vec<u64>,
}

const INFO_BITS: u32 = 18;
const T_KEYED: u64 = 1 << 17;
const T_NO_BOOL: u64 = T_KEYED - 1;

impl Table {
    #[inline]
    fn tag(h: u64) -> u64 {
        (h | 1 << 63) >> INFO_BITS << INFO_BITS
    }

    /// The slot of the term with hash `h`, or NONE if it is not a query term. Most
    /// corpus tokens are query terms found at their first slot, so that test comes
    /// first.
    #[inline]
    fn find(&self, h: u64) -> u32 {
        let (tag, mut i) = (Table::tag(h), h as usize & self.mask);
        loop {
            // SAFETY: `i` is masked by `mask`, and the table has `mask + 1` slots.
            let e = unsafe { *self.slots.get_unchecked(i) };
            if e >> INFO_BITS << INFO_BITS == tag {
                return i as u32;
            }
            if e == 0 {
                return NONE;
            }
            i = (i + 1) & self.mask;
        }
    }
}

/// Open-addressing table from query terms to their ids while the plan is built.
/// Terms are identified by a strong 64-bit hash of their bytes, so a lookup needs no
/// key comparison. Most corpus tokens are query terms, so there is no filter in front
/// of the corpus pass's copy: on the box a 128 KB bit filter saved nothing.
struct Terms {
    mask: usize,
    slots: Vec<Slot>,
    count: u32,
}

impl Default for Terms {
    fn default() -> Self {
        Terms::with_capacity(1 << 16)
    }
}

impl Terms {
    fn with_capacity(cap: usize) -> Terms {
        Terms {
            mask: cap - 1,
            slots: vec![Slot::default(); cap],
            count: 0,
        }
    }

    /// The id of `key`, adding it if it is new. Grows at 60% full.
    fn insert(&mut self, key: Key) -> u32 {
        let h = hash(&key);
        let i = self.find(h);
        if i != NONE {
            return self.slots[i as usize].id;
        }
        if (self.count as usize + 1) * 5 > self.slots.len() * 3 {
            let old = std::mem::replace(self, Terms::with_capacity(self.slots.len() * 2));
            self.count = old.count;
            for s in old.slots.into_iter().filter(|s| s.hash != 0) {
                self.place(s);
            }
        }
        self.place(Slot {
            hash: h,
            id: self.count,
        });
        self.count += 1;
        self.count - 1
    }

    fn place(&mut self, slot: Slot) {
        let mut i = slot.hash as usize & self.mask;
        while self.slots[i].hash != 0 {
            i = (i + 1) & self.mask;
        }
        self.slots[i] = slot;
    }

    /// The slot index of the term with hash `h`, or NONE if it is not a query term.
    fn find(&self, h: u64) -> u32 {
        let mut i = h as usize & self.mask;
        loop {
            let x = self.slots[i].hash;
            if x == h {
                return i as u32;
            }
            if x == 0 {
                return NONE;
            }
            i = (i + 1) & self.mask;
        }
    }
}

/// A strong 64-bit hash of a token's key: a multiply-xor of its words, then a full
/// avalanche (MurmurHash3's finaliser). Never 0, which marks an empty slot.
#[inline]
fn hash(k: &Key) -> u64 {
    let mut h = (k[0].wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ k[1].rotate_left(23).wrapping_mul(0xc2b2_ae3d_27d4_eb4f))
        ^ (k[2] ^ k[3].rotate_left(41)).wrapping_mul(0x1656_67b1_9e37_79f9);
    h = (h ^ h >> 33).wrapping_mul(0xff51_afd7_ed55_8ccd);
    h = (h ^ h >> 33).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    (h ^ h >> 33) | 1
}

/// Fills `slots` with the term-table slot (or NONE) of every token of `text`, in order.
///
/// A token is a maximal run of `[A-Za-z0-9]`. Each 64-byte block is classified into a
/// bitmask; XOR-ing it with itself shifted by one bit sets a bit at every token
/// boundary, starts and ends alternating, so tokens are walked boundary by boundary
/// with trailing-zero counts and no per-byte branch.
///
/// The lookups for a whole document go in stages: hash and slot prefetch for every
/// token, then the probes, so the misses of one stage overlap instead of each token
/// waiting for its own.
fn tokenize(text: &[u8], terms: &Table, scratch: &mut Scratch, slots: &mut Vec<u32>) {
    // Tokens are separated, so there are at most len / 2 + 1 of them: the buffer is
    // sized once and written by index, with no capacity check per token.
    if scratch.hashes.len() < text.len() / 2 + 1 {
        scratch.hashes.resize(text.len() / 2 + 1, 0);
    }
    let (hashes, mut n) = (&mut scratch.hashes[..], 0);
    let mut token = |start: usize, end: usize| {
        let mut h = 0; // 0: cannot be a query term
        if end - start <= MAX_TERM {
            h = hash(&key_at(text, start, end - start));
            prefetch(&terms.slots[h as usize & terms.mask]);
        }
        // SAFETY: tokens are separated, so a text of `len` bytes has at most
        // len / 2 + 1 of them, and `hashes` was sized to that above.
        unsafe { *hashes.get_unchecked_mut(n) = h };
        n += 1;
    };
    // Each block's mask is used as soon as it is made. `carry` is the last bit of the
    // previous word; `start` is where the open token began, or usize::MAX between
    // tokens.
    let blocks = text.chunks_exact(64);
    let tail = blocks.remainder();
    let tail = (!tail.is_empty()).then(|| {
        (tail.iter())
            .enumerate()
            .fold(0, |m, (i, &b)| m | ((CLASS[b as usize] != 0) as u64) << i)
    });
    let masks = blocks.map(|b| block_mask(b.try_into().unwrap())).chain(tail);
    let (mut carry, mut start) = (0, usize::MAX);
    for (w, m) in masks.enumerate() {
        let mut edges = m ^ (m << 1 | carry);
        carry = m >> 63;
        while edges != 0 {
            let p = w * 64 + edges.trailing_zeros() as usize;
            edges &= edges - 1;
            if start == usize::MAX {
                start = p;
            } else {
                token(start, p);
                start = usize::MAX;
            }
        }
    }
    if start != usize::MAX {
        token(start, text.len());
    }
    slots.clear();
    slots.extend(scratch.hashes[..n].iter().map(|&h| match h {
        0 => NONE,
        h => terms.find(h),
    }));
}

/// Per-thread buffers for `tokenize`.
#[derive(Default)]
struct Scratch {
    hashes: Vec<u64>,
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

/// The lowercased, zero-padded key of the token `text[start..start + len]`. OR-ing
/// 0x20 lowercases ASCII letters and leaves digits alone; the length mask clears the
/// separator bytes that follow the token.
#[inline(always)]
fn key_at(text: &[u8], start: usize, len: usize) -> Key {
    // Almost always 32 bytes follow the start: four plain loads, no branches.
    let mut padded = [0u8; 32];
    let window = match text[start..].first_chunk::<32>() {
        Some(w) => w,
        None => {
            padded[..text.len() - start].copy_from_slice(&text[start..]);
            &padded
        }
    };
    std::array::from_fn(|w| {
        let word = u64::from_le_bytes(window[8 * w..8 * w + 8].try_into().unwrap());
        let keep = u64::from_le_bytes(KEEP[32 - len + 8 * w..][..8].try_into().unwrap());
        (word | 0x2020_2020_2020_2020) & keep
    })
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
fn block_mask(block: &[u8; 64]) -> u64 {
    use std::arch::x86_64::*;
    // SAFETY: AVX2 is enabled at compile time (target-cpu=x86-64-v3), and both loads
    // read 32 bytes inside the 64-byte block.
    unsafe {
        let half = |p: *const u8| {
            let v = _mm256_loadu_si256(p as *const __m256i);
            let lower = _mm256_or_si256(v, _mm256_set1_epi8(0x20));
            // Signed compares: bytes >= 0x80 are negative and fail both ranges.
            let alpha = _mm256_and_si256(
                _mm256_cmpgt_epi8(lower, _mm256_set1_epi8(b'a' as i8 - 1)),
                _mm256_cmpgt_epi8(_mm256_set1_epi8(b'z' as i8 + 1), lower),
            );
            let digit = _mm256_and_si256(
                _mm256_cmpgt_epi8(v, _mm256_set1_epi8(b'0' as i8 - 1)),
                _mm256_cmpgt_epi8(_mm256_set1_epi8(b'9' as i8 + 1), v),
            );
            _mm256_movemask_epi8(_mm256_or_si256(alpha, digit)) as u32 as u64
        };
        half(block.as_ptr()) | half(block.as_ptr().add(32)) << 32
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline]
fn block_mask(block: &[u8; 64]) -> u64 {
    block
        .iter()
        .enumerate()
        .fold(0, |m, (i, &b)| m | ((CLASS[b as usize] != 0) as u64) << i)
}

/// `out = a op b` on ascending, duplicate-free lists.
fn apply(op: Op, a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    out.clear();
    match op {
        Op::And => {
            let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
            if small.len() * 16 < large.len() {
                // Gallop the short list through the long one.
                let mut lo = 0;
                for &x in small {
                    lo += large[lo..].partition_point(|&y| y < x);
                    if lo == large.len() {
                        break;
                    }
                    if large[lo] == x {
                        out.push(x);
                    }
                }
            } else {
                let (mut i, mut j) = (0, 0);
                while i < a.len() && j < b.len() {
                    let (x, y) = (a[i], b[j]);
                    if x == y {
                        out.push(x);
                    }
                    i += (x <= y) as usize;
                    j += (y <= x) as usize;
                }
            }
        }
        Op::Or => {
            out.reserve(a.len() + b.len());
            let (mut i, mut j) = (0, 0);
            while i < a.len() && j < b.len() {
                let (x, y) = (a[i], b[j]);
                out.push(x.min(y));
                i += (x <= y) as usize;
                j += (y <= x) as usize;
            }
            out.extend_from_slice(&a[i..]);
            out.extend_from_slice(&b[j..]);
        }
        Op::Not => {
            if b.len() > a.len() * 16 {
                let mut lo = 0;
                for &x in a {
                    lo += b[lo..].partition_point(|&y| y < x);
                    if lo == b.len() || b[lo] != x {
                        out.push(x);
                    }
                }
            } else {
                let mut j = 0;
                for &x in a {
                    while j < b.len() && b[j] < x {
                        j += 1;
                    }
                    if j == b.len() || b[j] != x {
                        out.push(x);
                    }
                }
            }
        }
    }
}

/// One BOOL query's summary, shared by all threads, which add one chunk's result at
/// a time (chunks are disjoint). The packed count and sum take one atomic addition.
/// The ten smallest ids so far are kept sorted (u32::MAX where unfilled) under a
/// lock, and `tenth` mirrors the last of them: a chunk whose smallest match is not
/// below it cannot change them, so the lock is skipped. `tenth` only decreases, so a
/// stale read can only take the lock needlessly, never miss an update.
struct Shared {
    packed: AtomicU64,
    tenth: AtomicU32,
    first: Mutex<[u32; 10]>,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            packed: AtomicU64::new(0),
            tenth: AtomicU32::new(u32::MAX),
            first: Mutex::new([u32::MAX; 10]),
        }
    }
}

impl Shared {
    fn add(&self, s: &Summary) {
        let n = s.count().min(10) as usize;
        if n == 0 {
            return;
        }
        self.packed.fetch_add(s.packed, Relaxed);
        if s.first[0] < self.tenth.load(Relaxed) {
            let mut first = self.first.lock().unwrap();
            let (a, b) = (*first, &s.first[..n]);
            let (mut i, mut j) = (0, 0);
            for f in first.iter_mut() {
                if j == b.len() || a[i] < b[j] {
                    (*f, i) = (a[i], i + 1);
                } else {
                    (*f, j) = (b[j], j + 1);
                }
            }
            self.tenth.store(first[9], Relaxed);
        }
    }

    /// The summary, once every thread has finished.
    fn get(&self) -> Summary {
        Summary {
            first: *self.first.lock().unwrap(),
            packed: self.packed.load(Relaxed),
        }
    }
}

/// `count sum id_1 .. id_10` of the matching documents, fed in ascending order. The
/// count and the sum share one word, `count << 40 | sum`, so adding a document is one
/// addition and a summary is 48 bytes (each thread keeps one per BOOL query). Ids
/// are below 2^20 (asserted as documents are read), so every sum is below 2^39.
#[derive(Default)]
struct Summary {
    first: [u32; 10],
    packed: u64,
}

impl Summary {
    #[inline]
    fn count(&self) -> u32 {
        (self.packed >> 40) as u32
    }

    #[inline]
    fn add(&mut self, doc: u32) {
        if self.count() < 10 {
            self.first[self.count() as usize] = doc;
        }
        self.packed += (1 << 40) + doc as u64;
    }

    fn write(&self, buf: &mut Vec<u8>) {
        push_num(buf, self.count() as u64);
        buf.push(b' ');
        push_num(buf, self.packed & ((1 << 40) - 1));
        for &d in &self.first[..(self.count() as usize).min(10)] {
            buf.push(b' ');
            push_num(buf, d as u64);
        }
        buf.push(b'\n');
    }
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

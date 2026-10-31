//! Track B: Index. Answer 50,000 BOOL and PHRASE queries over a 250,000-document corpus.
//!
//! The queries are known before the corpus is read, so only their terms are indexed:
//! 1. Parse the queries and give every distinct term an id.
//! 2. One pass over the corpus, in chunks of consecutive documents. Each document is
//!    tokenized into term ids (tokens no query uses become NONE). Phrases are matched
//!    right there: a phrase is looked up by its first two terms at every adjacent
//!    pair, then checked in full, so no positions are ever stored. Each document's
//!    distinct BOOL terms are recorded, and a counting sort turns them into the
//!    chunk's own posting lists, already sorted because documents arrive in order.
//!    A chunk holds about 10,000 documents, so lists store 16-bit offsets from the
//!    chunk's first document: half the memory of u32 ids.
//! 3. Chunks cover disjoint, ascending document ranges, so a BOOL query's answer is
//!    the per-chunk answers concatenated in chunk order. Queries run in parallel,
//!    each strictly left to right on sorted lists, chunk by chunk.

mod mmap;

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
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
    terms: Terms,
    n_bool: usize,
    queries: Vec<Query>,
    phrases: Vec<Vec<u32>>,
    /// Each phrase is checked only where its key term occurs: the phrase's term used
    /// by the fewest queries, a proxy for the rarest in the corpus (almost every
    /// phrase is built around a rare anchor). A term's keyed phrases, with the key's
    /// position inside each, are the `keyed` range in its info record.
    keyed: Vec<(u32, u32)>,
}

/// Posting sets of the BOOL terms for one chunk of documents, as offsets from `base`.
/// A sparse term is an ascending list, `docs[offsets[b]..offsets[b + 1]]`. A term in
/// more than 1/16 of the chunk's documents is a bitmap instead (never larger than
/// its list would be), listed in `dense` by term with its first word in `bits`.
struct ChunkIndex {
    base: u32,
    words: usize,
    offsets: Vec<u32>,
    docs: Vec<u16>,
    dense: Vec<(u32, u32)>,
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
        if !list.is_empty() {
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
fn has(bits: &[u64], d: u16) -> bool {
    bits[d as usize / 64] >> (d % 64) & 1 == 1
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
    match s {
        Set::List(l) => l.iter().for_each(|&d| out.add(base + d as u32)),
        Set::Bits(b) => {
            for (k, &w) in b.iter().enumerate() {
                let mut w = w;
                while w != 0 {
                    out.add(base + (k * 64) as u32 + w.trailing_zeros());
                    w &= w - 1;
                }
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().unwrap_or_else(|| "/data".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/out".into()));
    let plan = plan(mmap::map(&data.join("queries.txt")));
    let docs = mmap::map(&data.join("docs.txt"));
    let threads = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(12);
    // Four chunks per thread, so the last round is not left half empty; at most
    // ~8 MB each (~8,000 documents), well inside the 16-bit offsets.
    let chunks = split_lines(docs, (threads * 4).max(docs.len() >> 23));

    // The single pass over the corpus. Chunks are handed out dynamically.
    let next = AtomicUsize::new(0);
    let mut indexed: Vec<(usize, ChunkIndex, Vec<(u32, u32)>)> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (plan, chunks, next) = (&plan, &chunks, &next);
                s.spawn(move || {
                    let mut done = Vec::new();
                    let mut last = vec![NONE; plan.n_bool];
                    let mut ids = Vec::with_capacity(1024);
                    let mut scratch = Scratch::default();
                    // Each document's distinct BOOL terms, back to back (as 16-bit indices:
                    // there are at most 65,536 BOOL terms), reused chunk after chunk.
                    let mut terms: Vec<u16> = Vec::new();
                    while let Some(&chunk) = chunks.get(next.fetch_add(1, Relaxed)) {
                        terms.clear();
                        let mut keys: Vec<(u32, u32, u32)> = Vec::new();
                        let mut per_doc: Vec<u32> = Vec::new();
                        let mut matches = Vec::new();
                        let mut base = None;
                        for_each_doc(chunk, |doc, text| {
                            base.get_or_insert(doc);
                            ids.clear();
                            keys.clear();
                            let before = terms.len();
                            tokenize(text, &plan.terms, &mut scratch, &mut ids);
                            std::hint::black_box(&ids);
                            if std::env::var_os("SD_FULL").is_none() {
                                return; // TIMING PROBE
                            }
                            // Each token's BOOL posting (once per document) and whether a
                            // phrase is keyed on it, from its slot; then the slot index
                            // becomes the term id the phrase check compares.
                            for (i, t) in ids.iter_mut().enumerate().filter(|(_, t)| **t != NONE) {
                                let s = &plan.terms.slots[*t as usize];
                                let (b, key_lo, key_hi) = (s.bool_index, s.key_lo, s.key_hi);
                                *t = s.id;
                                if b != NONE && last[b as usize] != doc {
                                    last[b as usize] = doc;
                                    terms.push(b as u16);
                                }
                                if key_lo != key_hi {
                                    keys.push((i as u32, key_lo, key_hi));
                                }
                            }
                            per_doc.push((terms.len() - before) as u32);
                            match_phrases(plan, &ids, &keys, doc, &mut matches);
                        });
                        let base = base.unwrap_or(0);
                        assert!(
                            per_doc.len() <= 1 << 16,
                            "chunk too large for 16-bit offsets"
                        );
                        done.push((
                            base as usize,
                            counting_sort(base, &terms, &per_doc, plan.n_bool),
                            matches,
                        ));
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
    std::hint::black_box(&indexed);
    std::fs::write(out.join("results.txt"), b"").unwrap(); // TIMING PROBE
    std::process::exit(0);
    // Restore document order: chunks are keyed by their first document id.
    indexed.sort_unstable_by_key(|&(first, ..)| first);
    let mut matches: Vec<(u32, u32)> = indexed
        .iter_mut()
        .flat_map(|(_, _, m)| std::mem::take(m))
        .collect();
    let index: Vec<ChunkIndex> = indexed.into_iter().map(|(_, ci, _)| ci).collect();
    // A phrase can match one document at several positions; count the document once.
    matches.sort_unstable();
    matches.dedup();
    let phrase_docs = |p: usize| {
        let lo = matches.partition_point(|&(q, _)| (q as usize) < p);
        let hi = matches.partition_point(|&(q, _)| (q as usize) <= p);
        matches[lo..hi].iter().map(|&(_, d)| d)
    };

    // Evaluate in blocks handed out dynamically; blocks are written back in order.
    const BLOCK: usize = 256;
    let n_blocks = plan.queries.len().div_ceil(BLOCK);
    let next = AtomicUsize::new(0);
    let mut blocks: Vec<(usize, Vec<u8>)> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (plan, next, index, phrase_docs) = (&plan, &next, &index, &phrase_docs);
                s.spawn(move || {
                    let mut done = Vec::new();
                    let mut acc = Acc::default();
                    loop {
                        let k = next.fetch_add(1, Relaxed);
                        if k >= n_blocks {
                            break;
                        }
                        let mut buf = Vec::with_capacity(BLOCK * 40);
                        for q in &plan.queries[k * BLOCK..((k + 1) * BLOCK).min(plan.queries.len())]
                        {
                            let mut result = Summary::default();
                            match q {
                                Query::Phrase(p) => phrase_docs(*p).for_each(|d| result.add(d)),
                                Query::Bool(first, rest) => {
                                    for ci in index {
                                        if rest.is_empty() {
                                            emit(ci.set(*first), ci.base, &mut result);
                                            continue;
                                        }
                                        acc.start(ci.set(*first));
                                        for &(op, t) in rest {
                                            acc.apply(op, ci.set(t));
                                        }
                                        acc.emit(ci.base, &mut result);
                                    }
                                }
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

/// Groups a chunk's per-document BOOL terms by term. Documents are visited in order,
/// so every list comes out ascending.
fn counting_sort(base: u32, terms: &[u16], per_doc: &[u32], n_bool: usize) -> ChunkIndex {
    let mut offsets = vec![0u32; n_bool + 1];
    for &b in terms {
        offsets[b as usize + 1] += 1;
    }
    for i in 0..n_bool {
        offsets[i + 1] += offsets[i];
    }
    // Dense terms become bitmaps; their list ranges are emptied.
    let n_docs = per_doc.len();
    let words = n_docs.div_ceil(64);
    let mut dense = Vec::new();
    let mut slot = vec![u32::MAX; 0];
    let mut sizes = vec![0u32; n_bool];
    for b in 0..n_bool {
        sizes[b] = offsets[b + 1] - offsets[b];
        if sizes[b] as usize * 16 > n_docs {
            dense.push((b as u32, (dense.len() * words) as u32));
            sizes[b] = 0;
        }
    }
    slot.resize(n_bool, u32::MAX);
    for &(b, w) in &dense {
        slot[b as usize] = w;
    }
    offsets[0] = 0;
    for b in 0..n_bool {
        offsets[b + 1] = offsets[b] + sizes[b];
    }
    let mut cursor = offsets.clone();
    let mut docs = vec![0u16; offsets[n_bool] as usize];
    let mut bits = vec![0u64; dense.len() * words];
    let mut at = 0;
    for (d, &n) in per_doc.iter().enumerate() {
        for &b in &terms[at..at + n as usize] {
            match slot[b as usize] {
                u32::MAX => {
                    docs[cursor[b as usize] as usize] = d as u16;
                    cursor[b as usize] += 1;
                }
                w => bits[w as usize + d / 64] |= 1 << (d % 64),
            }
        }
        at += n as usize;
    }
    ChunkIndex {
        base,
        words,
        offsets,
        docs,
        dense,
        bits,
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
    let mut key_first = vec![0u32; n_terms + 1];
    keys.iter()
        .for_each(|&(t, ..)| key_first[t as usize + 1] += 1);
    for t in 0..n_terms {
        key_first[t + 1] += key_first[t];
    }
    let mut cursor = key_first.clone();
    let mut keyed = vec![(0, 0); keys.len()];
    for &(t, p, at) in &keys {
        keyed[cursor[t as usize] as usize] = (p, at);
        cursor[t as usize] += 1;
    }
    for s in terms.slots.iter_mut().filter(|s| s.hash != 0) {
        let t = s.id as usize;
        (s.bool_index, s.key_lo, s.key_hi) = (bool_index[t], key_first[t], key_first[t + 1]);
    }
    Plan {
        terms,
        n_bool: {
            assert!(n_bool <= 1 << 16, "BOOL term indices are 16-bit");
            n_bool as usize
        },
        queries,
        phrases,
        keyed,
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

/// Everything a corpus token needs, in one 32-byte slot: the term's hash, its id, its
/// dense BOOL index (or NONE) and the range of phrases keyed on it.
#[derive(Clone, Copy, Default)]
#[repr(C, align(32))]
struct Slot {
    hash: u64,
    id: u32,
    bool_index: u32,
    key_lo: u32,
    key_hi: u32,
}

/// Open-addressing table from query terms to their slots, built once, read by every
/// thread. Terms are identified by a strong 64-bit hash of their bytes, so a lookup
/// is one cache miss with no key comparison. (A corpus token would have to share all
/// 64 bits with a query term to be mistaken for it: about 4 * 10^-9 per run for the
/// spec's 600,000 distinct tokens and 110,000 query terms.) In front of it, a 128 KB
/// bit filter (two bits per term) that stays in L2: most corpus tokens are not query
/// terms, and the filter turns their lookup into a cached test.
struct Terms {
    mask: usize,
    slots: Vec<Slot>,
    count: u32,
    filter: Vec<u64>,
}

impl Default for Terms {
    fn default() -> Self {
        Terms::with_capacity(1 << 16)
    }
}

const FILTER_BITS: u32 = 20;

#[inline]
fn filter_bits(h: u64) -> (usize, usize) {
    (
        (h >> (64 - FILTER_BITS)) as usize,
        (h >> (40 - FILTER_BITS)) as usize & ((1 << FILTER_BITS) - 1),
    )
}

impl Terms {
    fn with_capacity(cap: usize) -> Terms {
        Terms {
            mask: cap - 1,
            slots: vec![Slot::default(); cap],
            count: 0,
            filter: vec![0; 1 << (FILTER_BITS - 6)],
        }
    }

    /// The id of `key`, adding it if it is new. Grows at half full.
    fn insert(&mut self, key: Key) -> u32 {
        let h = hash(&key);
        if let Some(i) = self.find(h) {
            return self.slots[i].id;
        }
        if (self.count as usize + 1) * 2 > self.slots.len() {
            let old = std::mem::replace(self, Terms::with_capacity(self.slots.len() * 2));
            self.count = old.count;
            for s in old.slots.into_iter().filter(|s| s.hash != 0) {
                self.place(s);
            }
        }
        self.place(Slot {
            hash: h,
            id: self.count,
            bool_index: NONE,
            ..Slot::default()
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
        let (a, b) = filter_bits(slot.hash);
        self.filter[a / 64] |= 1 << (a % 64);
        self.filter[b / 64] |= 1 << (b % 64);
    }

    #[inline]
    fn filtered_out(&self, h: u64) -> bool {
        let (a, b) = filter_bits(h);
        self.filter[a / 64] >> (a % 64) & self.filter[b / 64] >> (b % 64) & 1 == 0
    }

    /// The slot index of the term with hash `h`, if it is a query term.
    #[inline]
    fn find(&self, h: u64) -> Option<usize> {
        let mut i = h as usize & self.mask;
        loop {
            match self.slots[i].hash {
                0 => return None,
                x if x == h => return Some(i),
                _ => i = (i + 1) & self.mask,
            }
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
/// A token is a maximal run of `[A-Za-z0-9]`. First every byte is classified into a
/// bitmask (64 bytes per word), then tokens are walked with trailing-zero counts, so
/// the only unpredictable branch is once per token rather than once per byte.
///
/// The term table does not fit in L2, so the lookups for a whole document go in
/// stages: hash and slot prefetch for every token, then the probes. The misses of
/// one stage overlap instead of each token waiting for its own.
fn tokenize(text: &[u8], terms: &Terms, scratch: &mut Scratch, slots: &mut Vec<u32>) {
    token_mask(text, &mut scratch.mask);
    scratch.hashes.clear();
    let mut i = 0;
    while let Some(start) = next_bit(&scratch.mask, i, 0) {
        let end = next_bit(&scratch.mask, start, !0).unwrap_or(text.len());
        let mut h = 0; // 0: cannot be a query term
        if end - start <= MAX_TERM {
            h = hash(&key_at(text, start, end - start));
            if terms.filtered_out(h) {
                h = 0;
            } else {
                prefetch(&terms.slots[h as usize & terms.mask]);
            }
        }
        scratch.hashes.push(h);
        i = end;
    }
    slots.clear();
    slots.extend(scratch.hashes.iter().map(|&h| match h {
        0 => NONE,
        h => terms.find(h).map_or(NONE, |i| i as u32),
    }));
}

/// Per-thread buffers for `tokenize`.
#[derive(Default)]
struct Scratch {
    mask: Vec<u64>,
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

/// The first position at or after `i` whose mask bit is set (`flip == 0`) or clear
/// (`flip == !0`). Bits past the text are clear.
#[inline]
fn next_bit(mask: &[u64], i: usize, flip: u64) -> Option<usize> {
    let mut w = i / 64;
    let mut m = (*mask.get(w)? ^ flip) & (!0u64 << (i % 64));
    while m == 0 {
        w += 1;
        m = *mask.get(w)? ^ flip;
    }
    Some(w * 64 + m.trailing_zeros() as usize)
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

/// One bit per byte of `text`, set for the token bytes `[A-Za-z0-9]`.
fn token_mask(text: &[u8], mask: &mut Vec<u64>) {
    mask.clear();
    let mut blocks = text.chunks_exact(64);
    for block in &mut blocks {
        mask.push(block_mask(block.try_into().unwrap()));
    }
    let tail = blocks.remainder();
    if !tail.is_empty() {
        mask.push(
            tail.iter()
                .enumerate()
                .fold(0, |m, (i, &b)| m | ((CLASS[b as usize] != 0) as u64) << i),
        );
    }
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

/// `count sum id_1 .. id_10` of the matching documents, fed in ascending order.
#[derive(Default)]
struct Summary {
    count: u64,
    sum: u64,
    first: [u32; 10],
}

impl Summary {
    #[inline]
    fn add(&mut self, doc: u32) {
        if self.count < 10 {
            self.first[self.count as usize] = doc;
        }
        self.count += 1;
        self.sum += doc as u64;
    }

    fn write(&self, buf: &mut Vec<u8>) {
        push_num(buf, self.count);
        buf.push(b' ');
        push_num(buf, self.sum);
        for &d in &self.first[..(self.count as usize).min(10)] {
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

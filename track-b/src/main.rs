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

use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::thread;

/// FxHash: one multiply per word. Keys are query words and packed term pairs.
#[derive(Default)]
struct Fx(u64);

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..c.len()].copy_from_slice(c);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

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
    /// Term id -> dense index among BOOL terms, or NONE.
    bool_index: Vec<u32>,
    n_bool: usize,
    queries: Vec<Query>,
    phrases: Vec<Vec<u32>>,
    /// (first term, second term) -> the phrases that start with that pair.
    starts: FxMap<u64, Vec<u32>>,
    starts_phrase: Vec<bool>,
}

/// Posting lists of the BOOL terms for one chunk of documents: term b's documents
/// are `base + docs[offsets[b]..offsets[b + 1]]`, ascending.
struct ChunkIndex {
    base: u32,
    offsets: Vec<u32>,
    docs: Vec<u16>,
}

impl ChunkIndex {
    #[inline]
    fn list(&self, b: u32) -> &[u16] {
        &self.docs[self.offsets[b as usize] as usize..self.offsets[b as usize + 1] as usize]
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().unwrap_or_else(|| "/data".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/out".into()));
    let plan = plan(mmap::map(&data.join("queries.txt")));
    let docs = mmap::map(&data.join("docs.txt"));
    let threads = thread::available_parallelism().map_or(1, |n| n.get()).min(12);
    // About 8 MB per chunk: ~8,000 documents, well inside the 16-bit offsets.
    let chunks = split_lines(docs, (threads * 2).max(docs.len() >> 23));

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
                    while let Some(&chunk) = chunks.get(next.fetch_add(1, Relaxed)) {
                        // Each document's distinct BOOL terms, back to back, and how many each has.
                        let mut terms: Vec<u32> = Vec::with_capacity(chunk.len() / 12);
                        let mut per_doc: Vec<u32> = Vec::new();
                        let mut matches = Vec::new();
                        let mut base = None;
                        for_each_doc(chunk, |doc, text| {
                            base.get_or_insert(doc);
                            ids.clear();
                            tokenize(text, &plan.terms, |t| ids.push(t));
                            let before = terms.len();
                            for &t in &ids {
                                let b = if t == NONE { NONE } else { plan.bool_index[t as usize] };
                                if b != NONE && last[b as usize] != doc {
                                    last[b as usize] = doc;
                                    terms.push(b);
                                }
                            }
                            per_doc.push((terms.len() - before) as u32);
                            match_phrases(plan, &ids, doc, &mut matches);
                        });
                        let base = base.unwrap_or(0);
                        assert!(per_doc.len() <= 1 << 16, "chunk too large for 16-bit offsets");
                        done.push((base as usize, counting_sort(base, &terms, &per_doc, plan.n_bool), matches));
                    }
                    done
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    // Restore document order: chunks are keyed by their first document id.
    indexed.sort_unstable_by_key(|&(first, ..)| first);
    let mut matches: Vec<(u32, u32)> = indexed.iter_mut().flat_map(|(_, _, m)| std::mem::take(m)).collect();
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
                    let (mut a, mut b) = (Vec::new(), Vec::new());
                    loop {
                        let k = next.fetch_add(1, Relaxed);
                        if k >= n_blocks {
                            break;
                        }
                        let mut buf = Vec::with_capacity(BLOCK * 40);
                        for q in &plan.queries[k * BLOCK..((k + 1) * BLOCK).min(plan.queries.len())] {
                            let mut result = Summary::default();
                            match q {
                                Query::Phrase(p) => phrase_docs(*p).for_each(|d| result.add(d)),
                                Query::Bool(first, rest) => {
                                    for ci in index {
                                        if rest.is_empty() {
                                            ci.list(*first).iter().for_each(|&d| result.add(ci.base + d as u32));
                                            continue;
                                        }
                                        a.clear();
                                        a.extend_from_slice(ci.list(*first));
                                        for &(op, t) in rest {
                                            apply(op, &a, ci.list(t), &mut b);
                                            std::mem::swap(&mut a, &mut b);
                                        }
                                        a.iter().for_each(|&d| result.add(ci.base + d as u32));
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
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    blocks.sort_unstable_by_key(|&(k, _)| k);

    let mut file = File::create(out.join("results.txt")).expect("create results.txt");
    for (_, buf) in &blocks {
        file.write_all(buf).expect("write results.txt");
    }
}

/// Groups a chunk's per-document BOOL terms by term. Documents are visited in order,
/// so every list comes out ascending.
fn counting_sort(base: u32, terms: &[u32], per_doc: &[u32], n_bool: usize) -> ChunkIndex {
    let mut offsets = vec![0u32; n_bool + 1];
    for &b in terms {
        offsets[b as usize + 1] += 1;
    }
    for i in 0..n_bool {
        offsets[i + 1] += offsets[i];
    }
    let mut cursor = offsets.clone();
    let mut docs = vec![0u16; terms.len()];
    let mut at = 0;
    for (d, &n) in per_doc.iter().enumerate() {
        for &b in &terms[at..at + n as usize] {
            docs[cursor[b as usize] as usize] = d as u16;
            cursor[b as usize] += 1;
        }
        at += n as usize;
    }
    ChunkIndex { base, offsets, docs }
}

/// Parses queries.txt and assigns term ids.
fn plan(text: &'static [u8]) -> Plan {
    let mut ids: FxMap<&'static [u8], u32> = FxMap::default();
    let mut id = |w: &'static [u8]| {
        let next = ids.len() as u32;
        *ids.entry(w).or_insert(next)
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
            let inner = line.strip_prefix(b"PHRASE \"").and_then(|r| r.strip_suffix(b"\"")).expect("bad query");
            phrases.push(inner.split(|&b| b == b' ').map(&mut id).collect());
            queries.push(Query::Phrase(phrases.len() - 1));
        }
    }

    // BOOL terms get dense indices so each chunk's offset table only covers them.
    let mut bool_index = vec![NONE; ids.len()];
    let mut n_bool = 0;
    let mut dense = |t: u32| {
        if bool_index[t as usize] == NONE {
            bool_index[t as usize] = n_bool;
            n_bool += 1;
        }
        bool_index[t as usize]
    };
    let mut raw_bool = raw_bool.into_iter();
    for q in queries.iter_mut() {
        if let Query::Bool(..) = q {
            let (first, ops) = raw_bool.next().unwrap();
            *q = Query::Bool(dense(first), ops.into_iter().map(|(op, t)| (op, dense(t))).collect());
        }
    }

    let mut starts: FxMap<u64, Vec<u32>> = FxMap::default();
    let mut starts_phrase = vec![false; ids.len()];
    for (p, terms) in phrases.iter().enumerate() {
        starts.entry(pair(terms[0], terms[1])).or_default().push(p as u32);
        starts_phrase[terms[0] as usize] = true;
    }
    Plan { terms: Terms::new(&ids), bool_index, n_bool: n_bool as usize, queries, phrases, starts, starts_phrase }
}

fn pair(a: u32, b: u32) -> u64 {
    (a as u64) << 32 | b as u64
}

/// Records (phrase, doc) for every phrase found in the document's token ids.
fn match_phrases(plan: &Plan, ids: &[u32], doc: u32, matches: &mut Vec<(u32, u32)>) {
    for i in 0..ids.len().saturating_sub(1) {
        let (a, b) = (ids[i], ids[i + 1]);
        if a == NONE || b == NONE || !plan.starts_phrase[a as usize] {
            continue;
        }
        if let Some(candidates) = plan.starts.get(&pair(a, b)) {
            for &p in candidates {
                if ids[i..].starts_with(&plan.phrases[p as usize]) {
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
        let tab = line.iter().position(|&b| b == b'\t').expect("TAB after the id");
        let eol = line[tab..].iter().position(|&b| b == b'\n').map_or(line.len(), |e| tab + e);
        let doc = line[..tab].iter().fold(0u32, |acc, &b| acc * 10 + (b - b'0') as u32);
        f(doc, &line[tab + 1..eol]);
        p += eol + 1;
    }
}

/// A token, zero-padded to 32 bytes and read as four words, so hashing and comparing
/// are a few word operations instead of a byte loop and a `memcmp` through a pointer.
type Key = [u64; 4];

/// Open-addressing table from query terms to ids, built once, read by every thread.
/// Each slot packs a 32-bit hash tag with `id + 1`, so a miss is decided from the
/// slot array alone without touching any key.
struct Terms {
    mask: usize,
    slots: Vec<u64>,
    keys: Vec<Key>,
}

impl Terms {
    fn new(ids: &FxMap<&'static [u8], u32>) -> Terms {
        let cap = (ids.len() * 2).next_power_of_two().max(16);
        let mut t = Terms { mask: cap - 1, slots: vec![0; cap], keys: vec![[0; 4]; ids.len()] };
        for (&word, &id) in ids {
            let mut bytes = [0u8; MAX_TERM];
            bytes[..word.len()].copy_from_slice(word);
            let key = to_key(&bytes);
            let h = hash(&key);
            let mut i = h as usize & t.mask;
            while t.slots[i] != 0 {
                i = (i + 1) & t.mask;
            }
            t.slots[i] = (h >> 32) << 32 | (id as u64 + 1);
            t.keys[id as usize] = key;
        }
        t
    }

    #[inline]
    fn get(&self, key: &Key) -> u32 {
        let h = hash(key);
        let mut i = h as usize & self.mask;
        loop {
            let s = self.slots[i];
            if s == 0 {
                return NONE;
            }
            let id = (s as u32).wrapping_sub(1);
            if s >> 32 == h >> 32 && self.keys[id as usize] == *key {
                return id;
            }
            i = (i + 1) & self.mask;
        }
    }
}

#[inline]
fn to_key(bytes: &[u8; MAX_TERM]) -> Key {
    std::array::from_fn(|w| u64::from_le_bytes(bytes[w * 8..w * 8 + 8].try_into().unwrap()))
}

#[inline]
fn hash(k: &Key) -> u64 {
    let h = (k[0].wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ k[1].rotate_left(23).wrapping_mul(0xc2b2_ae3d_27d4_eb4f))
        ^ (k[2] ^ k[3].rotate_left(41)).wrapping_mul(0x1656_67b1_9e37_79f9);
    h ^ h >> 29
}

/// Calls `f` with the term id (or NONE) of every token, in order.
#[inline]
fn tokenize(text: &[u8], terms: &Terms, mut f: impl FnMut(u32)) {
    let mut i = 0;
    while i < text.len() {
        if CLASS[text[i] as usize] == 0 {
            i += 1;
            continue;
        }
        let mut buf = [0u8; MAX_TERM];
        let mut len = 0;
        while i < text.len() {
            let c = CLASS[text[i] as usize];
            if c == 0 {
                break;
            }
            if len < MAX_TERM {
                buf[len] = c;
            }
            len += 1;
            i += 1;
        }
        f(if len <= MAX_TERM { terms.get(&to_key(&buf)) } else { NONE });
    }
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
            end = data[guess..].iter().position(|&b| b == b'\n').map_or(data.len(), |p| guess + p + 1);
        }
        chunks.push(&data[start..end]);
        start = end;
    }
    chunks
}

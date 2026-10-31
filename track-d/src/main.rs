//! Track D: Traverse. Exact shortest-path distances for 2,000 (s, t) queries on a
//! 5M-edge directed graph with non-negative u32 weights.
//!
//! 1. Parse the edge list twice in parallel, straight from the map: the first pass
//!    counts out-degrees, the second writes each edge into place at an atomic cursor.
//!    No intermediate edge list is ever materialised.
//! 2. Layout: node ids are randomly relabelled, so every settled node is a cache miss.
//!    Each node gets one 32-byte row holding up to four edges, aligned so it is never
//!    split across cache lines: one miss per node, found by address arithmetic with
//!    no offset table to read first. 97% of nodes have at most four out-edges; the
//!    rest keep three inline and spill the others to an overflow list.
//! 3. Answer queries in parallel with Dijkstra that stops when t is settled. Distances
//!    live in a small per-query hash table rather than an n-sized array per thread:
//!    queries are local (median ~512 settled nodes), so this keeps memory near the
//!    graph's own size and the working set in cache.
//! 4. Cheap exact shortcuts: s == t is 0; a source with no out-edges or a target with
//!    no in-edges is unreachable.

mod mmap;

use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread;

/// An edge in a row is one word, `(head + 1) << 32 | weight`, so an all-zero row is
/// empty and rows can come from zeroed memory.
const EMPTY: u64 = 0;
/// In a row's last slot, `SPILL << 32 | bucket << 16 | offset`: the node's remaining
/// edges are in its bucket's overflow list, as a length followed by the edges.
const SPILL: u64 = u32::MAX as u64;
/// The build sorts edges into buckets of 2^11 consecutive nodes.
const BUCKET_BITS: u32 = 11;
const LOCAL: u64 = (1 << BUCKET_BITS) - 1;

/// Up to four edges of one node, in one aligned half cache line.
#[derive(Clone, Copy)]
#[repr(C, align(32))]
struct Row([u64; 4]);

/// One direction of the graph: each node's edges in its own row, plus per-bucket
/// overflow lists for nodes with more than four.
struct Adjacency {
    rows: Vec<Row>,
    spill: Vec<Vec<u64>>,
}

impl Adjacency {
    /// Builds the rows from edges grouped by bucket: `edges[starts[b]..starts[b + 1]]`
    /// are bucket b's, each `local tail << 53 | head << 32 | weight`. Every bucket's
    /// rows are a disjoint slice written by one worker, in cache, with no atomics.
    /// Buckets finish roughly in order; the edges below the lowest unfinished one
    /// are given back as they go, so the rows grow as the edges shrink.
    fn build(edges: &[AtomicU64], starts: &[usize], n: usize, threads: usize) -> Adjacency {
        let mut rows: Vec<Row> = zeroed(n);
        let buckets = n.div_ceil(1 << BUCKET_BITS);
        let work = Mutex::new((
            rows.chunks_mut(1 << BUCKET_BITS).enumerate(),
            vec![false; buckets], // finished buckets
            0,                    // all buckets below this are finished
        ));
        let mut spill: Vec<(usize, Vec<u64>)> = thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let work = &work;
                    s.spawn(move || {
                        let mut done = Vec::new();
                        loop {
                            // Take the lock only to pick a bucket, not while filling it.
                            let Some((b, rows)) = work.lock().unwrap().0.next() else {
                                break;
                            };
                            done.push((b, fill_bucket(b, rows, &edges[starts[b]..starts[b + 1]])));
                            let (from, to) = {
                                let (_, finished, low) = &mut *work.lock().unwrap();
                                let from = *low;
                                finished[b] = true;
                                while *low < buckets && finished[*low] {
                                    *low += 1;
                                }
                                (starts[from], starts[*low])
                            };
                            mmap::release(edges, from, to);
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
        spill.sort_unstable_by_key(|&(b, _)| b);
        Adjacency {
            rows,
            spill: spill.into_iter().map(|(_, list)| list).collect(),
        }
    }

    fn has_edges(&self, u: u32) -> bool {
        self.rows[u as usize].0[0] != EMPTY
    }

    /// Calls `f(other end, weight)` for every edge of `u`.
    #[inline]
    fn for_each_edge(&self, u: u32, mut f: impl FnMut(u32, u32)) {
        for &e in &self.rows[u as usize].0 {
            match e >> 32 {
                _ if e == EMPTY => return,
                SPILL => {
                    let (list, at) = (
                        &self.spill[(e as u32 >> 16) as usize],
                        (e & 0xffff) as usize,
                    );
                    let edges = &list[at + 1..at + 1 + list[at] as usize];
                    edges
                        .iter()
                        .for_each(|&e| f((e >> 32) as u32 - 1, e as u32));
                    return;
                }
                v => f(v as u32 - 1, e as u32),
            }
        }
    }

    /// Starts loading `u`'s row early; the search will likely settle `u` soon.
    #[inline]
    fn prefetch(&self, u: u32) {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: a prefetch is only a hint and never faults; `u` is in bounds anyway.
        unsafe {
            use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            _mm_prefetch::<_MM_HINT_T0>(self.rows.as_ptr().add(u as usize) as *const i8);
        }
        let _ = u;
    }
}

/// Writes the rows of bucket `b` from its edges and returns its overflow list.
fn fill_bucket(b: usize, rows: &mut [Row], edges: &[AtomicU64]) -> Vec<u64> {
    let mut degree = vec![0u32; rows.len()];
    edges
        .iter()
        .for_each(|e| degree[(e.load(Relaxed) >> 53) as usize] += 1);
    let mut list = Vec::new();
    let mut at = vec![0u32; rows.len()]; // where a spilled node's list starts
    for (i, &d) in degree.iter().enumerate().filter(|&(_, &d)| d > 4) {
        assert!(list.len() < 1 << 16, "overflow list too long");
        rows[i].0[3] = SPILL << 32 | (b as u64) << 16 | list.len() as u64;
        at[i] = list.len() as u32;
        list.push(d as u64 - 3);
        list.resize(list.len() + d as usize - 3, 0);
    }
    let mut used = vec![0u32; rows.len()];
    for e in edges.iter().map(|e| e.load(Relaxed)) {
        let i = (e >> 53) as usize;
        let slot = used[i] as usize;
        used[i] += 1;
        let word = ((e >> 32 & 0x1f_ffff) + 1) << 32 | (e & 0xffff_ffff);
        match degree[i] > 4 && slot >= 3 {
            true => list[at[i] as usize + 1 + slot - 3] = word,
            false => rows[i].0[slot] = word,
        }
    }
    list
}

/// `n` values from a zeroed allocation: no pass writes them first, and each page is
/// faulted in by whichever build worker writes it first.
fn zeroed<T: Zeroable>(n: usize) -> Vec<T> {
    let layout = std::alloc::Layout::array::<T>(n).expect("allocation size");
    if layout.size() == 0 {
        return Vec::new();
    }
    // SAFETY: `Zeroable` types are valid as all-zero bytes; the allocation has the
    // layout of `n` values of T and is handed to the Vec that will free it.
    unsafe {
        let ptr = std::alloc::alloc_zeroed(layout) as *mut T;
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Vec::from_raw_parts(ptr, n, n)
    }
}

/// Types whose all-zero bit pattern is a valid value.
trait Zeroable {}
impl Zeroable for Row {}

struct Graph {
    out: Adjacency,
    into: Adjacency,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().unwrap_or_else(|| "/data".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/out".into()));
    let threads = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(12);
    let graph = load_graph(mmap::map(&data.join("graph.txt")), threads);
    let queries: Vec<(u32, u32)> = (mmap::map(&data.join("queries.txt")).split(|&b| b == b'\n'))
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (s, p) = number(l, 0);
            (s as u32, number(l, p + 1).0 as u32)
        })
        .collect();

    // Queries vary a lot in cost, so threads take them one at a time. u64::MAX means
    // unreachable; no real distance gets near it ((n - 1) * (2^32 - 1) < 2^63).
    let answers: Vec<AtomicU64> = (0..queries.len()).map(|_| AtomicU64::new(0)).collect();
    let next = AtomicUsize::new(0);
    thread::scope(|s| {
        for _ in 0..threads {
            let (graph, queries, answers, next) = (&graph, &queries, &answers, &next);
            s.spawn(move || {
                let mut search = Search::default();
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    let Some(&(s, t)) = queries.get(i) else { break };
                    answers[i].store(search.distance(graph, s, t).unwrap_or(u64::MAX), Relaxed);
                }
            });
        }
    });

    let mut text = String::with_capacity(queries.len() * 8);
    for a in &answers {
        match a.load(Relaxed) {
            u64::MAX => text.push_str("-1\n"),
            d => writeln!(text, "{d}").unwrap(),
        }
    }
    std::fs::write(out.join("distances.txt"), text).expect("write distances.txt");
}

fn load_graph(text: &'static [u8], threads: usize) -> Graph {
    let (n, p) = number(text, 0);
    let header_end = text[p..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(text.len(), |i| p + i + 1);
    let n = n as usize;
    assert!(n < 1 << 21, "node ids are packed in 21 bits");
    let buckets = n.div_ceil(1 << BUCKET_BITS);
    let chunks = split_lines(&text[header_end..], threads * 4);

    // Pass 1: each chunk counts its edges per bucket, by tail (out) and by head (in).
    let counts: Vec<Mutex<[Vec<usize>; 2]>> = chunks
        .iter()
        .map(|_| Mutex::new([vec![0; buckets], vec![0; buckets]]))
        .collect();
    for_each_chunk(&chunks, threads, |k, chunk| {
        let [out, into] = &mut *counts[k].lock().unwrap();
        for_each_line(chunk, |u, v, _| {
            out[u as usize >> BUCKET_BITS] += 1;
            into[v as usize >> BUCKET_BITS] += 1;
        });
    });

    // Prefix sums, bucket-major: each chunk's counts become its write cursors, so
    // chunk k's edges of bucket b land right after chunk k-1's.
    let mut starts = [vec![0; buckets + 1], vec![0; buckets + 1]];
    for (dir, starts) in starts.iter_mut().enumerate() {
        let mut total = 0;
        for b in 0..buckets {
            starts[b] = total;
            for c in &counts {
                let c = &mut c.lock().unwrap()[dir][b];
                (*c, total) = (total, total + *c);
            }
        }
        starts[buckets] = total;
    }

    // Pass 2: each edge goes to its bucket in both directions. A chunk writes only
    // its own ranges, as about a thousand sequential streams instead of random words.
    let m = starts[0][buckets];
    let edges = [mmap::zeroed_atomics(m), mmap::zeroed_atomics(m)];
    for_each_chunk(&chunks, threads, |k, chunk| {
        let [out, into] = &mut *counts[k].lock().unwrap();
        for_each_line(chunk, |u, v, w| {
            let (u, v, w) = (u as u64, v as u64, w as u64);
            let (bo, bi) = (u as usize >> BUCKET_BITS, v as usize >> BUCKET_BITS);
            edges[0][out[bo]].store((u & LOCAL) << 53 | v << 32 | w, Relaxed);
            edges[1][into[bi]].store((v & LOCAL) << 53 | u << 32 | w, Relaxed);
            (out[bo], into[bi]) = (out[bo] + 1, into[bi] + 1);
        });
    });

    // Rows, one direction at a time, giving each edge array back as it is consumed.
    let [out_edges, into_edges] = edges;
    let out = Adjacency::build(out_edges, &starts[0], n, threads);
    let into = Adjacency::build(into_edges, &starts[1], n, threads);
    Graph { out, into }
}

/// Runs `f` over the chunks on `threads` scoped threads, handing chunks out dynamically.
fn for_each_chunk(
    chunks: &[&'static [u8]],
    threads: usize,
    f: impl Fn(usize, &'static [u8]) + Sync,
) {
    let next = AtomicUsize::new(0);
    thread::scope(|s| {
        for _ in 0..threads {
            let (next, f) = (&next, &f);
            s.spawn(move || {
                loop {
                    let k = next.fetch_add(1, Relaxed);
                    let Some(&c) = chunks.get(k) else { break };
                    f(k, c);
                }
            });
        }
    });
}

/// Calls `f(u, v, w)` for every `u v w` line.
#[inline]
fn for_each_line(chunk: &[u8], mut f: impl FnMut(u32, u32, u32)) {
    let mut p = 0;
    while p < chunk.len() {
        let (u, q) = number(chunk, p);
        let (v, q) = number(chunk, q + 1);
        let (w, q) = number(chunk, q + 1);
        f(u as u32, v as u32, w as u32);
        p = q + 1;
    }
}

/// One direction of a bidirectional search.
#[derive(Default)]
struct Side {
    dist: DistMap,
    heap: RadixHeap,
}

impl Side {
    fn start(&mut self, from: u32) {
        self.dist.clear();
        self.heap.clear();
        self.dist.improve(from, 0);
        self.heap.push(0, from);
    }

    /// Settles the next node and relaxes its edges in `adj`. Every relaxation that
    /// reaches a node the other side has seen offers a path to `best`.
    #[inline]
    fn step(&mut self, adj: &Adjacency, other: &DistMap, best: &mut u64) {
        let Some((d, u)) = self.heap.pop() else {
            return;
        };
        if d > self.dist.get(u) {
            return; // stale: u was settled at a shorter distance
        }
        let (dist, heap) = (&mut self.dist, &mut self.heap);
        adj.for_each_edge(u, |v, w| {
            let nd = d + w as u64;
            if dist.improve(v, nd) {
                adj.prefetch(v);
                heap.push(nd, v);
                let back = other.get(v);
                if back != u64::MAX {
                    *best = (*best).min(nd + back);
                }
            }
        });
    }
}

/// Per-thread search state, reused across queries.
#[derive(Default)]
struct Search {
    fwd: Side,
    bwd: Side,
}

impl Search {
    /// Bidirectional Dijkstra: forward from s on the out-edges, backward from t on the
    /// in-edges, always growing the side with the smaller frontier. (Growing the side
    /// with the smaller key would let one ~2^32 edge out of s make the other side
    /// flood the whole graph.) Once the two smallest keys sum to at least the best
    /// meeting distance, no shorter path can exist. Stale heap keys only ever
    /// understate a side, which delays the stop but never makes it early.
    fn distance(&mut self, g: &Graph, s: u32, t: u32) -> Option<u64> {
        if s == t {
            return Some(0);
        }
        if !g.out.has_edges(s) || !g.into.has_edges(t) {
            return None;
        }
        self.fwd.start(s);
        self.bwd.start(t);
        let mut best = u64::MAX;
        loop {
            let (Some(kf), Some(kb)) = (self.fwd.heap.peek(), self.bwd.heap.peek()) else {
                break;
            };
            if kf.saturating_add(kb) >= best {
                break;
            }
            if self.fwd.heap.len <= self.bwd.heap.len {
                self.fwd.step(&g.out, &self.bwd.dist, &mut best);
            } else {
                self.bwd.step(&g.into, &self.fwd.dist, &mut best);
            }
        }
        (best != u64::MAX).then_some(best)
    }
}

/// Monotone priority queue for Dijkstra: keys pushed are never below the last key
/// popped. Bucket i holds keys whose highest bit differing from that last key is
/// bit i - 1 (bucket 0: equal to it), so a push is O(1) and every element moves to a
/// lower bucket at most 64 times over its life.
struct RadixHeap {
    last: u64,
    len: usize,
    buckets: [Vec<(u64, u32)>; 65],
}

impl Default for RadixHeap {
    fn default() -> Self {
        RadixHeap {
            last: 0,
            len: 0,
            buckets: std::array::from_fn(|_| Vec::new()),
        }
    }
}

impl RadixHeap {
    /// Empties the heap, keeping its buffers warm for the next query.
    fn clear(&mut self) {
        self.last = 0;
        self.len = 0;
        self.buckets.iter_mut().for_each(Vec::clear);
    }

    #[inline]
    fn bucket(&self, key: u64) -> usize {
        64 - (key ^ self.last).leading_zeros() as usize
    }

    #[inline]
    fn push(&mut self, key: u64, node: u32) {
        let b = self.bucket(key);
        self.buckets[b].push((key, node));
        self.len += 1;
    }

    /// The smallest key, without removing it.
    #[inline]
    fn peek(&mut self) -> Option<u64> {
        if self.buckets[0].is_empty() {
            // Move the lowest non-empty bucket down around its smallest key.
            let i = self.buckets.iter().position(|b| !b.is_empty())?;
            let moved = std::mem::take(&mut self.buckets[i]);
            self.last = moved.iter().map(|&(k, _)| k).min().unwrap();
            for &(k, v) in &moved {
                let b = self.bucket(k);
                self.buckets[b].push((k, v));
            }
            self.buckets[i] = moved;
            self.buckets[i].clear();
        }
        Some(self.last)
    }

    #[inline]
    fn pop(&mut self) -> Option<(u64, u32)> {
        self.peek()?;
        self.len -= 1;
        self.buckets[0].pop()
    }
}

/// A distance-map entry is one word, `node << 43 | distance`, so a probe touches one
/// cache line. Node ids fit 21 bits (asserted at load); distances in these graphs
/// stay far below 2^43 (asserted on every insert).
const DIST_BITS: u32 = 43;
const DIST_MASK: u64 = (1 << DIST_BITS) - 1;
const NO_ENTRY: u64 = u64::MAX;

/// Open-addressing map from node to tentative distance. Missing means infinity.
struct DistMap {
    slots: Vec<u64>,
    /// Slots filled since the last clear, so clearing costs what the query used.
    used: Vec<u32>,
}

impl Default for DistMap {
    fn default() -> Self {
        DistMap {
            slots: vec![NO_ENTRY; 1024],
            used: Vec::new(),
        }
    }
}

impl DistMap {
    /// Empties the map, keeping its capacity warm for the next large query.
    fn clear(&mut self) {
        for &i in &self.used {
            self.slots[i as usize] = NO_ENTRY;
        }
        self.used.clear();
    }

    /// The slot holding `k`, or the empty slot where it would go.
    #[inline]
    fn slot(&self, k: u32) -> usize {
        let mask = self.slots.len() - 1;
        let mut i = (k.wrapping_mul(0x9e37_79b9) >> 7) as usize & mask;
        while self.slots[i] != NO_ENTRY && (self.slots[i] >> DIST_BITS) as u32 != k {
            i = (i + 1) & mask;
        }
        i
    }

    #[inline]
    fn get(&self, k: u32) -> u64 {
        match self.slots[self.slot(k)] {
            NO_ENTRY => u64::MAX,
            e => e & DIST_MASK,
        }
    }

    /// Lowers k's distance to `d` if that is an improvement; returns whether it was.
    #[inline]
    fn improve(&mut self, k: u32, d: u64) -> bool {
        assert!(d < DIST_MASK, "distance beyond 2^43");
        let i = self.slot(k);
        let e = self.slots[i];
        if e != NO_ENTRY {
            if d >= e & DIST_MASK {
                return false;
            }
        } else {
            self.used.push(i as u32);
        }
        self.slots[i] = (k as u64) << DIST_BITS | d;
        if self.used.len() * 2 > self.slots.len() {
            self.grow();
        }
        true
    }

    fn grow(&mut self) {
        let cap = self.slots.len() * 2;
        let old = std::mem::replace(&mut self.slots, vec![NO_ENTRY; cap]);
        for j in std::mem::take(&mut self.used) {
            let e = old[j as usize];
            let i = self.slot((e >> DIST_BITS) as u32);
            self.slots[i] = e;
            self.used.push(i as u32);
        }
    }
}

/// A decimal number at `p`; returns it and the position of the byte after it (a
/// space or a newline). Up to seven digits, which covers every node id and nearly
/// every weight, are found and converted with word arithmetic: locate the delimiter
/// in one 8-byte load, left-align the digits, then combine pairs, quads and octets by
/// multiplication. Longer numbers take the byte loop.
#[inline]
fn number(c: &[u8], mut p: usize) -> (u64, usize) {
    const ONES: u64 = 0x0101_0101_0101_0101;
    if let Some(w) = c.get(p..p + 8) {
        let w = u64::from_le_bytes(w.try_into().unwrap());
        let zero = |x: u64| x.wrapping_sub(ONES) & !x & (ONES << 7);
        let n = (zero(w ^ ONES * b' ' as u64) | zero(w ^ ONES * b'\n' as u64)).trailing_zeros()
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

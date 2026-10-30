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

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread;

/// A row slot holds `head + 1`, so zeroed memory is a row of empty slots (0) and the
/// rows need no fill pass. Node ids are below n < 2^32 - 2.
const EMPTY_SLOT: u32 = 0;
/// In a row's last slot: the node's remaining edges are overflow list number `weight`.
const SPILL: u32 = u32::MAX;
/// An empty key in a distance map.
const EMPTY: u32 = u32::MAX;

/// Up to four (head, weight) edges of one node, in one aligned half cache line.
#[derive(Clone, Copy)]
#[repr(C, align(32))]
struct Row([(u32, u32); 4]);

/// One direction of the graph: each node's edges in its own row, plus overflow lists
/// for nodes with more than four.
struct Adjacency {
    rows: Vec<Row>,
    /// Overflow list k is `spill_edges[spill_first[k]..spill_first[k + 1]]`.
    spill_first: Vec<u32>,
    spill_edges: Vec<(u32, u32)>,
}

impl Adjacency {
    /// Empty rows sized for the given degrees; a node with more than four edges gets
    /// an overflow list and a SPILL marker in its last slot.
    fn with_degrees(degree: &[AtomicU32]) -> Adjacency {
        let mut rows = zeroed_rows(degree.len());
        let mut spill_first = vec![0u32];
        for (u, d) in degree.iter().enumerate() {
            let d = d.load(Relaxed);
            if d > 4 {
                rows[u].0[3] = (SPILL, spill_first.len() as u32 - 1);
                spill_first.push(spill_first.last().unwrap() + d - 3);
            }
        }
        let spill_edges = vec![(0, 0); *spill_first.last().unwrap() as usize];
        Adjacency {
            rows,
            spill_first,
            spill_edges,
        }
    }

    fn has_edges(&self, u: u32) -> bool {
        self.rows[u as usize].0[0].0 != EMPTY_SLOT
    }

    /// Calls `f(other end, weight)` for every edge of `u`.
    #[inline]
    fn for_each_edge(&self, u: u32, mut f: impl FnMut(u32, u32)) {
        for &(v, w) in &self.rows[u as usize].0 {
            match v {
                EMPTY_SLOT => return,
                SPILL => {
                    let (lo, hi) = (
                        self.spill_first[w as usize],
                        self.spill_first[w as usize + 1],
                    );
                    for &(v, w) in &self.spill_edges[lo as usize..hi as usize] {
                        f(v, w);
                    }
                    return;
                }
                _ => f(v - 1, w),
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

/// Raw write access to an `Adjacency` for the placement pass, whose threads write
/// disjoint slots.
struct Placer {
    rows: *mut (u32, u32),
    spill: *mut (u32, u32),
}

// SAFETY: only used for the disjoint writes described in `place`.
unsafe impl Sync for Placer {}

impl Placer {
    fn new(adj: &mut Adjacency) -> Placer {
        Placer {
            rows: adj.rows.as_mut_ptr().cast(),
            spill: adj.spill_edges.as_mut_ptr(),
        }
    }

    /// Puts edge `e` of node `u` into slot `slot` (counted down from u's degree).
    ///
    /// # Safety
    /// Every (u, slot) must be written once, by one thread, with slot below u's degree
    /// as counted for `adj`, and `adj` must not be read until placement ends.
    unsafe fn place(&self, adj: &Adjacency, u: u32, slot: usize, e: (u32, u32)) {
        // Slot 3 of a spilled node was written before placement began and is never
        // placed into (its slots 0..3 are inline and the rest go to its list).
        let last = adj.rows[u as usize].0[3];
        unsafe {
            if last.0 == SPILL && slot >= 3 {
                self.spill
                    .add(adj.spill_first[last.1 as usize] as usize + slot - 3)
                    .write(e);
            } else {
                self.rows.add(u as usize * 4 + slot).write((e.0 + 1, e.1));
            }
        }
    }
}

/// `n` empty rows from a zeroed allocation: no pass writes 32 bytes per node first.
fn zeroed_rows(n: usize) -> Vec<Row> {
    if n == 0 {
        return Vec::new();
    }
    let layout = std::alloc::Layout::array::<Row>(n).expect("row count");
    // SAFETY: the layout is non-zero-sized; an all-zero Row is valid (four empty
    // slots); the allocation is owned by the Vec with exactly this layout.
    unsafe {
        let ptr = std::alloc::alloc_zeroed(layout) as *mut Row;
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Vec::from_raw_parts(ptr, n, n)
    }
}

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
    let queries: Vec<(u32, u32)> = lines(mmap::map(&data.join("queries.txt")))
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

    let mut buf = Vec::with_capacity(queries.len() * 8);
    for a in &answers {
        match a.load(Relaxed) {
            u64::MAX => buf.extend_from_slice(b"-1"),
            d => push_num(&mut buf, d),
        }
        buf.push(b'\n');
    }
    File::create(out.join("distances.txt"))
        .and_then(|mut f| f.write_all(&buf))
        .expect("write distances.txt");
}

fn load_graph(text: &'static [u8], threads: usize) -> Graph {
    let (n, p) = number(text, 0);
    let header_end = text[p..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(text.len(), |i| p + i + 1);
    let n = n as usize;
    let chunks = split_lines(&text[header_end..], threads * 4);

    // Pass 1: out- and in-degrees.
    let out_degree: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    let in_degree: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    for_each_chunk(&chunks, threads, |chunk| {
        for_each_batch(chunk, |batch| {
            for &(u, v, _) in batch {
                prefetch(&out_degree[u as usize]);
                prefetch(&in_degree[v as usize]);
            }
            for &(u, v, _) in batch {
                out_degree[u as usize].fetch_add(1, Relaxed);
                in_degree[v as usize].fetch_add(1, Relaxed);
            }
        });
    });
    let mut out = Adjacency::with_degrees(&out_degree);
    let mut into = Adjacency::with_degrees(&in_degree);

    // Pass 2: place every edge in both directions. Counting each degree down hands
    // out slots d-1..0; the order inside one node's edges varies between runs, which
    // cannot change a distance.
    let (out_dst, into_dst) = (Placer::new(&mut out), Placer::new(&mut into));
    let (out_ref, into_ref) = (&out, &into);
    for_each_chunk(&chunks, threads, |chunk| {
        for_each_batch(chunk, |batch| {
            for &(u, v, _) in batch {
                prefetch(&out_degree[u as usize]);
                prefetch(&in_degree[v as usize]);
                prefetch(&out_ref.rows[u as usize]);
                prefetch(&into_ref.rows[v as usize]);
            }
            for &(u, v, w) in batch {
                let out_slot = out_degree[u as usize].fetch_sub(1, Relaxed) as usize - 1;
                let into_slot = in_degree[v as usize].fetch_sub(1, Relaxed) as usize - 1;
                // SAFETY: pass 1 counted exactly these edges, so each countdown gives
                // every edge its own slot below the node's degree, written by this
                // thread alone.
                unsafe {
                    out_dst.place(out_ref, u, out_slot, (v, w));
                    into_dst.place(into_ref, v, into_slot, (u, w));
                }
            }
        });
    });
    Graph { out, into }
}

/// Runs `f` over the chunks on `threads` scoped threads, handing chunks out dynamically.
fn for_each_chunk(chunks: &[&'static [u8]], threads: usize, f: impl Fn(&'static [u8]) + Sync) {
    let next = AtomicUsize::new(0);
    thread::scope(|s| {
        for _ in 0..threads {
            let (next, f) = (&next, &f);
            s.spawn(move || {
                while let Some(&c) = chunks.get(next.fetch_add(1, Relaxed)) {
                    f(c);
                }
            });
        }
    });
}

/// Lines per batch in the build passes.
const BATCH: usize = 16;

/// Calls `f` with the `(u, v, w)` of consecutive lines, up to BATCH at a time, so the
/// caller can prefetch everything a batch touches before touching any of it: edges
/// arrive in random order, and the misses of one batch then overlap.
#[inline]
fn for_each_batch(chunk: &[u8], mut f: impl FnMut(&[(u32, u32, u32)])) {
    let mut batch = [(0u32, 0u32, 0u32); BATCH];
    let (mut n, mut p) = (0, 0);
    while p < chunk.len() {
        let (u, q) = number(chunk, p);
        let (v, q) = number(chunk, q + 1);
        let (w, q) = number(chunk, q + 1);
        batch[n] = (u as u32, v as u32, w as u32);
        n += 1;
        if n == BATCH {
            f(&batch);
            n = 0;
        }
        p = q + 1;
    }
    f(&batch[..n]);
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

/// Open-addressing map from node to tentative distance. Missing means infinity.
struct DistMap {
    keys: Vec<u32>,
    vals: Vec<u64>,
    /// Slots filled since the last clear, so clearing costs what the query used.
    used: Vec<u32>,
}

impl Default for DistMap {
    fn default() -> Self {
        DistMap {
            keys: vec![EMPTY; 1024],
            vals: vec![0; 1024],
            used: Vec::new(),
        }
    }
}

impl DistMap {
    /// Empties the map, keeping its capacity warm for the next large query.
    fn clear(&mut self) {
        for &i in &self.used {
            self.keys[i as usize] = EMPTY;
        }
        self.used.clear();
    }

    #[inline]
    fn slot(&self, k: u32) -> usize {
        let mask = self.keys.len() - 1;
        let mut i = (k.wrapping_mul(0x9e37_79b9) >> 7) as usize & mask;
        while self.keys[i] != EMPTY && self.keys[i] != k {
            i = (i + 1) & mask;
        }
        i
    }

    #[inline]
    fn get(&self, k: u32) -> u64 {
        let i = self.slot(k);
        if self.keys[i] == k {
            self.vals[i]
        } else {
            u64::MAX
        }
    }

    /// Lowers k's distance to `d` if that is an improvement; returns whether it was.
    #[inline]
    fn improve(&mut self, k: u32, d: u64) -> bool {
        let i = self.slot(k);
        if self.keys[i] == k {
            if d < self.vals[i] {
                self.vals[i] = d;
                return true;
            }
            return false;
        }
        self.keys[i] = k;
        self.vals[i] = d;
        self.used.push(i as u32);
        if self.used.len() * 2 > self.keys.len() {
            self.grow();
        }
        true
    }

    fn grow(&mut self) {
        let cap = self.keys.len() * 2;
        let old = std::mem::replace(
            self,
            DistMap {
                keys: vec![EMPTY; cap],
                vals: vec![0; cap],
                used: Vec::new(),
            },
        );
        for &j in &old.used {
            let (k, v) = (old.keys[j as usize], old.vals[j as usize]);
            let i = self.slot(k);
            self.keys[i] = k;
            self.vals[i] = v;
            self.used.push(i as u32);
        }
    }
}

/// A decimal number at `p`; returns it and the position of the byte after it (a
/// space or a newline). Up to seven digits, which covers every node id and nearly
/// every weight, are found and converted with word arithmetic: locate the
/// delimiter in one 8-byte load, left-align the digits, then combine pairs, quads
/// and octets by multiplication. Longer numbers take the byte loop.
#[inline]
fn number(c: &[u8], p: usize) -> (u64, usize) {
    const ONES: u64 = 0x0101_0101_0101_0101;
    let w = match c.get(p..p + 8) {
        Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
        None => 0x2020_2020_2020_2020, // near the end: no delimiter here, take the loop
    };
    let zero = |x: u64| x.wrapping_sub(ONES) & !x & (ONES << 7);
    let n = (zero(w ^ 0x2020_2020_2020_2020) | zero(w ^ 0x0a0a_0a0a_0a0a_0a0a)).trailing_zeros()
        as usize
        / 8;
    if (1..8).contains(&n) {
        let digits = (w << (8 * (8 - n))) & 0x0f0f_0f0f_0f0f_0f0f;
        let v = digits.wrapping_mul(10 * (1 << 8) + 1) >> 8 & 0x00ff_00ff_00ff_00ff;
        let v = v.wrapping_mul(100 * (1 << 16) + 1) >> 16 & 0x0000_ffff_0000_ffff;
        return (v.wrapping_mul(10000 * (1 << 32) + 1) >> 32, p + n);
    }
    let mut v = 0u64;
    let mut q = p;
    while let Some(&b) = c.get(q).filter(|b| b.is_ascii_digit()) {
        v = v * 10 + (b - b'0') as u64;
        q += 1;
    }
    (v, q)
}

fn lines(text: &[u8]) -> impl Iterator<Item = &[u8]> {
    text.split(|&b| b == b'\n').filter(|l| !l.is_empty())
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

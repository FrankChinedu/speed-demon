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
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread;

/// An edge is one word, `(head + 1) << 32 | weight`, so an all-zero row is empty and
/// rows can come from zeroed memory with no fill pass. Node ids are below n < 2^32 - 2.
const EMPTY: u64 = 0;
/// In a row's last slot, `SPILL << 32 | k`: the node's remaining edges are list k.
const SPILL: u64 = u32::MAX as u64;

/// Up to four edges of one node, in one aligned half cache line. Atomic so the build
/// threads can place edges without unsafe code; relaxed loads and stores compile to
/// plain moves.
#[repr(C, align(32))]
struct Row([AtomicU64; 4]);

/// One direction of the graph: each node's edges in its own row, plus overflow lists
/// for nodes with more than four.
struct Adjacency {
    rows: Vec<Row>,
    /// Overflow list k is `spill_edges[spill_first[k]..spill_first[k + 1]]`.
    spill_first: Vec<u32>,
    spill_edges: Vec<AtomicU64>,
}

impl Adjacency {
    /// Empty rows sized for the given degrees; a node with more than four edges gets
    /// an overflow list and a SPILL marker in its last slot.
    fn with_degrees(degree: &[AtomicU32], threads: usize) -> Adjacency {
        let mut spill_first = vec![0u32];
        let rows: Vec<Row> = zeroed(degree.len(), threads);
        for (row, d) in rows.iter().zip(degree.iter().map(|d| d.load(Relaxed))) {
            if d > 4 {
                row.0[3].store(SPILL << 32 | (spill_first.len() as u64 - 1), Relaxed);
                spill_first.push(spill_first.last().unwrap() + d - 3);
            }
        }
        let spill_edges = (0..*spill_first.last().unwrap())
            .map(|_| AtomicU64::new(0))
            .collect();
        Adjacency {
            rows,
            spill_first,
            spill_edges,
        }
    }

    /// Puts edge `e` of node `u` into slot `slot`, counted down from u's degree. Slot
    /// 3 of a spilled node holds its marker: its slots 0..3 are inline, the rest go to
    /// its overflow list.
    fn place(&self, u: u32, slot: usize, e: u64) {
        let last = self.rows[u as usize].0[3].load(Relaxed);
        match last >> 32 {
            SPILL if slot >= 3 => {
                &self.spill_edges[self.spill_first[last as u32 as usize] as usize + slot - 3]
            }
            _ => &self.rows[u as usize].0[slot],
        }
        .store(e, Relaxed);
    }

    fn has_edges(&self, u: u32) -> bool {
        self.rows[u as usize].0[0].load(Relaxed) != EMPTY
    }

    /// Calls `f(other end, weight)` for every edge of `u`.
    #[inline]
    fn for_each_edge(&self, u: u32, mut f: impl FnMut(u32, u32)) {
        for e in &self.rows[u as usize].0 {
            let e = e.load(Relaxed);
            match e >> 32 {
                _ if e == EMPTY => return,
                SPILL => {
                    let k = e as u32 as usize;
                    let list = &self.spill_edges
                        [self.spill_first[k] as usize..self.spill_first[k + 1] as usize];
                    list.iter()
                        .map(|e| e.load(Relaxed))
                        .for_each(|e| f((e >> 32) as u32 - 1, e as u32));
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

/// `n` zeroed values from a zeroed allocation, with every page touched first by all
/// threads in parallel, each over its own contiguous range. Without that, a single
/// thread fills the memory, or the pages fault one by one inside the random-write
/// build passes, both of which measured slower.
fn zeroed<T: Zeroable + Sync>(n: usize, threads: usize) -> Vec<T> {
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
        let v = Vec::from_raw_parts(ptr, n, n);
        let step = (4096 / std::mem::size_of::<T>()).max(1);
        thread::scope(|s| {
            for part in v.chunks(n.div_ceil(threads)) {
                s.spawn(move || part.iter().step_by(step).for_each(T::touch));
            }
        });
        v
    }
}

/// Types whose all-zero bit pattern is a valid value, and that can be written
/// through a shared reference (to fault their page in).
trait Zeroable {
    fn touch(&self);
}

impl Zeroable for AtomicU32 {
    fn touch(&self) {
        self.store(0, Relaxed);
    }
}

impl Zeroable for Row {
    fn touch(&self) {
        self.0[0].store(EMPTY, Relaxed);
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
    let chunks = split_lines(&text[header_end..], threads * 4);

    // Pass 1: out- and in-degrees.
    let (out_degree, in_degree): (Vec<AtomicU32>, Vec<AtomicU32>) =
        (zeroed(n, threads), zeroed(n, threads));
    for_each_chunk(&chunks, threads, |chunk| {
        for_each_line(chunk, |u, v, _| {
            out_degree[u as usize].fetch_add(1, Relaxed);
            in_degree[v as usize].fetch_add(1, Relaxed);
        });
    });
    let (out, into) = (
        Adjacency::with_degrees(&out_degree, threads),
        Adjacency::with_degrees(&in_degree, threads),
    );

    // Pass 2: place every edge in both directions. Counting each degree down hands
    // out slots d-1..0; the order inside one node's edges varies between runs, which
    // cannot change a distance.
    for_each_chunk(&chunks, threads, |chunk| {
        for_each_line(chunk, |u, v, w| {
            let out_slot = out_degree[u as usize].fetch_sub(1, Relaxed) as usize - 1;
            let into_slot = in_degree[v as usize].fetch_sub(1, Relaxed) as usize - 1;
            out.place(u, out_slot, (v as u64 + 1) << 32 | w as u64);
            into.place(v, into_slot, (u as u64 + 1) << 32 | w as u64);
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

/// An empty key in a distance map.
const NO_KEY: u32 = u32::MAX;

/// Open-addressing map from node to tentative distance. Missing means infinity.
struct DistMap {
    keys: Vec<u32>,
    vals: Vec<u64>,
    /// Slots filled since the last clear, so clearing costs what the query used.
    used: Vec<u32>,
}

impl Default for DistMap {
    fn default() -> Self {
        DistMap::with_capacity(1024)
    }
}

impl DistMap {
    fn with_capacity(cap: usize) -> DistMap {
        DistMap {
            keys: vec![NO_KEY; cap],
            vals: vec![0; cap],
            used: Vec::new(),
        }
    }

    /// Empties the map, keeping its capacity warm for the next large query.
    fn clear(&mut self) {
        for &i in &self.used {
            self.keys[i as usize] = NO_KEY;
        }
        self.used.clear();
    }

    #[inline]
    fn slot(&self, k: u32) -> usize {
        let mask = self.keys.len() - 1;
        let mut i = (k.wrapping_mul(0x9e37_79b9) >> 7) as usize & mask;
        while self.keys[i] != NO_KEY && self.keys[i] != k {
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
        let old = std::mem::replace(self, DistMap::with_capacity(self.keys.len() * 2));
        for &j in &old.used {
            let (k, v) = (old.keys[j as usize], old.vals[j as usize]);
            let i = self.slot(k);
            self.keys[i] = k;
            self.vals[i] = v;
            self.used.push(i as u32);
        }
    }
}

/// A decimal number at `p`; returns it and the position of the byte after it.
#[inline]
fn number(c: &[u8], mut p: usize) -> (u64, usize) {
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

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

/// Marks an unused slot in a row. Node ids are below n < 2^32 - 2.
const EMPTY: u32 = u32::MAX;
/// In a row's last slot: the node's remaining edges are overflow list number `weight`.
const SPILL: u32 = u32::MAX - 1;

/// Up to four (head, weight) edges of one node, in one aligned half cache line.
#[derive(Clone, Copy)]
#[repr(C, align(32))]
struct Row([(u32, u32); 4]);

struct Graph {
    rows: Vec<Row>,
    /// Overflow list k is `spill_edges[spill_first[k]..spill_first[k + 1]]`.
    spill_first: Vec<u32>,
    spill_edges: Vec<(u32, u32)>,
    has_in: Vec<u64>,
}

impl Graph {
    /// Calls `f(head, weight)` for every out-edge of `u`.
    #[inline]
    fn for_each_edge(&self, u: u32, mut f: impl FnMut(u32, u32)) {
        for &(v, w) in &self.rows[u as usize].0 {
            match v {
                EMPTY => return,
                SPILL => {
                    let (lo, hi) = (self.spill_first[w as usize], self.spill_first[w as usize + 1]);
                    for &(v, w) in &self.spill_edges[lo as usize..hi as usize] {
                        f(v, w);
                    }
                    return;
                }
                _ => f(v, w),
            }
        }
    }

    /// Starts loading `u`'s row early; Dijkstra will likely settle `u` soon.
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

fn main() {
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().unwrap_or_else(|| "/data".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/out".into()));
    let threads = thread::available_parallelism().map_or(1, |n| n.get()).min(12);
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
    let header_end = text[p..].iter().position(|&b| b == b'\n').map_or(text.len(), |i| p + i + 1);
    let n = n as usize;
    let chunks = split_lines(&text[header_end..], threads * 4);

    // Pass 1: out-degrees, and which nodes have any in-edge.
    let degree: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    let has_in: Vec<AtomicU64> = (0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
    for_each_chunk(&chunks, threads, |chunk| {
        for_each_line(chunk, |u, v, _| {
            degree[u as usize].fetch_add(1, Relaxed);
            has_in[v as usize / 64].fetch_or(1 << (v % 64), Relaxed);
        });
    });

    // Rows start empty; a node with more than four edges gets an overflow list and
    // a SPILL marker in its last slot.
    let mut rows = vec![Row([(EMPTY, 0); 4]); n];
    let mut spill_first = vec![0u32];
    for (u, d) in degree.iter().enumerate() {
        let d = d.load(Relaxed);
        if d > 4 {
            rows[u].0[3] = (SPILL, spill_first.len() as u32 - 1);
            spill_first.push(spill_first.last().unwrap() + d - 3);
        }
    }

    // Pass 2: place every edge. Counting each degree down hands out slots d-1..0. The
    // order inside one node's edges varies between runs, which cannot change a distance.
    let mut spill_edges = vec![(0u32, 0u32); *spill_first.last().unwrap() as usize];
    let (row_dst, spill_dst) = (Scatter(rows.as_mut_ptr().cast()), Scatter(spill_edges.as_mut_ptr()));
    let spill_list = |u: u32| {
        // SAFETY: slot 3 was written above, before any thread started, and pass 2 never
        // writes slot 3 of a node that has a spill list (its slots go 0..3 and the list).
        let (v, k) = unsafe { row_dst.get(u as usize * 4 + 3) };
        (v == SPILL).then_some(k)
    };
    for_each_chunk(&chunks, threads, |chunk| {
        for_each_line(chunk, |u, v, w| {
            let slot = degree[u as usize].fetch_sub(1, Relaxed) as usize - 1;
            // SAFETY: pass 1 counted exactly these edges, so the countdown gives each
            // edge of u its own slot: slots 0..min(d, 4) of u's row when d <= 4, else
            // 0..3 inline and the rest of u's own overflow range, all in bounds.
            unsafe {
                match spill_list(u) {
                    Some(k) if slot >= 3 => spill_dst.put(spill_first[k as usize] as usize + slot - 3, (v, w)),
                    _ => row_dst.put(u as usize * 4 + slot, (v, w)),
                }
            }
        });
    });

    let has_in = has_in.into_iter().map(AtomicU64::into_inner).collect();
    Graph { rows, spill_first, spill_edges, has_in }
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

/// Per-thread search state, reused across queries.
#[derive(Default)]
struct Search {
    dist: DistMap,
    heap: RadixHeap,
}

impl Search {
    fn distance(&mut self, g: &Graph, s: u32, t: u32) -> Option<u64> {
        if s == t {
            return Some(0);
        }
        if g.rows[s as usize].0[0].0 == EMPTY || g.has_in[t as usize / 64] >> (t % 64) & 1 == 0 {
            return None;
        }
        self.dist.clear();
        self.heap.clear();
        self.dist.improve(s, 0);
        self.heap.push(0, s);
        while let Some((d, u)) = self.heap.pop() {
            if d > self.dist.get(u) {
                continue; // stale: u was settled at a shorter distance
            }
            if u == t {
                return Some(d);
            }
            let (dist, heap) = (&mut self.dist, &mut self.heap);
            g.for_each_edge(u, |v, w| {
                let nd = d + w as u64;
                if dist.improve(v, nd) {
                    g.prefetch(v);
                    heap.push(nd, v);
                }
            });
        }
        None
    }
}

/// Monotone priority queue for Dijkstra: keys pushed are never below the last key
/// popped. Bucket i holds keys whose highest bit differing from that last key is
/// bit i - 1 (bucket 0: equal to it), so a push is O(1) and every element moves to a
/// lower bucket at most 64 times over its life.
struct RadixHeap {
    last: u64,
    buckets: [Vec<(u64, u32)>; 65],
}

impl Default for RadixHeap {
    fn default() -> Self {
        RadixHeap { last: 0, buckets: std::array::from_fn(|_| Vec::new()) }
    }
}

impl RadixHeap {
    /// Empties the heap, keeping its buffers warm for the next query.
    fn clear(&mut self) {
        self.last = 0;
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
    }

    #[inline]
    fn pop(&mut self) -> Option<(u64, u32)> {
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
        DistMap { keys: vec![EMPTY; 1024], vals: vec![0; 1024], used: Vec::new() }
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
        if self.keys[i] == k { self.vals[i] } else { u64::MAX }
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
        let old = std::mem::replace(self, DistMap { keys: vec![EMPTY; cap], vals: vec![0; cap], used: Vec::new() });
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
            end = data[guess..].iter().position(|&b| b == b'\n').map_or(data.len(), |p| guess + p + 1);
        }
        chunks.push(&data[start..end]);
        start = end;
    }
    chunks
}

/// Raw pointer to an edge array shared by the pass-2 threads, which write disjoint slots.
struct Scatter(*mut (u32, u32));

// SAFETY: only used for the disjoint writes described at the call sites.
unsafe impl Sync for Scatter {}

impl Scatter {
    /// # Safety
    /// `i` must be in bounds and written by exactly one thread.
    unsafe fn put(&self, i: usize, v: (u32, u32)) {
        unsafe { self.0.add(i).write(v) }
    }

    /// # Safety
    /// `i` must be in bounds and not written concurrently.
    unsafe fn get(&self, i: usize) -> (u32, u32) {
        unsafe { self.0.add(i).read() }
    }
}

# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, zero dependencies. **Build.** The edge list is parsed twice in parallel straight from the map, with SWAR number parsing. The first pass counts each chunk's edges per bucket of 2,048 nodes, in both directions. Prefix sums turn those counts into bucket-major write cursors, and the second pass writes every edge into its bucket's range in both directions as about a thousand sequential streams. Then each bucket's rows are filled by one worker, in cache, with no atomics.

**Layout.** Each node owns one 32-byte, cache-aligned row of up to four edges, `(head + 1) << 32 | weight`, in each direction. Out-degree is at most 4 for 97% of nodes, and the rest spill to a per-bucket overflow list. With randomly relabelled ids this makes each settled node a single cache miss, found by address arithmetic with no offset table to read first.

**Queries.** Queries run in parallel, one at a time per thread. Each is a bidirectional Dijkstra that grows the side with the smaller frontier and stops once the two smallest keys sum to at least the best meeting distance. It uses a radix heap (keys are monotone) and per-side distance maps of one-word entries, `node << 43 | distance`, so a probe touches one cache line; the maps stay warm across queries and are cleared by their used-slot lists. A pushed node's row is prefetched. `s == t`, sources with no out-edges and targets with no in-edges are answered in O(1).

The step-by-step history, with every board measurement, the timing probes and the failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `mmap` is declared directly against the C library that std already links.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- Queries are two thirds of the run (a box probe exits after the build at 64 ms of 194 ms). A goal-directed search (ALT landmarks) would cut settled nodes, if its preprocessing can fit in the budget.
- Peak memory is the two row tables plus the build's edge arrays. Returning edge memory progressively did not lower the box's peak (huge pages stay charged), so a smaller row layout is the remaining lever.

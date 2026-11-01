# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, zero dependencies. **Build.** The edge list is parsed twice in parallel straight from the map, with SWAR number parsing. The first pass counts each chunk's edges per bucket of 2,048 nodes, in both directions. Prefix sums turn those counts into bucket-major write cursors. The second pass writes every out-edge into its bucket's range as about a thousand sequential streams, and each bucket's rows are filled by one worker, in cache, with no atomics. The in-edges are then read back from the out rows into the same 40 MB buffer (each piece of rows reserves its range per bucket with one atomic add), so only one edge array is ever alive and the text is parsed only twice.

**Layout.** Each node owns one 16-byte, aligned row of up to four edges of one u32 each, `(head + 1) << 11 | weight`, in each direction. Node ids fit 21 bits, and main-body weights are travel times of 1 to a few thousand (99.9% below 2,047). Out-degree is at most 4 for 97% of nodes; a node with more edges, or with any weight of 2,047 or more, keeps all its edges as full words in a per-bucket overflow list. Both row tables together are 56 MB, half of the earlier 32-byte rows. With randomly relabelled ids this makes each settled node a single cache miss, found by address arithmetic with no offset table to read first.

**Queries.** Queries run in parallel, one at a time per thread. Each is a bidirectional Dijkstra that grows the side with the smaller frontier and stops once the two smallest keys sum to at least the best meeting distance. It uses a radix heap (keys are monotone) and per-side distance maps of one-word entries, `node << 43 | distance`, so a probe touches one cache line; the maps stay warm across queries and are cleared by their used-slot lists. A pushed node's row is prefetched. `s == t`, sources with no out-edges and targets with no in-edges are answered in O(1).

The step-by-step history, with every board measurement, the timing probes and the failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `mmap` is declared directly against the C library that std already links.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- Queries are about two thirds of the run. An x86 profile puts 22% of the whole run in the distance-map probes (each relaxation probes its own side's map and the other side's). A goal-directed search (ALT landmarks) would cut settled nodes, and so probes, if its preprocessing fits the budget.
- Peak memory is set during the build (box probe: 127 MB), ~23 MB above the row tables and the one edge array. Finding where that comes from is the next memory step.

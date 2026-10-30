# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, zero dependencies. The edge list is parsed twice in parallel straight from the map. The first pass counts out-degrees and in-edge presence. The second places each edge at an atomic countdown cursor.

Each node owns one 32-byte, cache-aligned row of up to four edges. Out-degree is at most 4 for 97% of nodes, and the rest spill to an overflow list. With randomly relabelled ids this makes each settled node a single cache miss, with no offset table.

Queries run in parallel: Dijkstra stops when the target is settled, uses a radix heap (keys are monotone), and keeps distances in a per-query open-addressing map that stays warm across queries and is cleared by its used-slot list. `s == t`, sources with no out-edges and targets with no in-edges are answered in O(1).

The step-by-step history, with measurements and failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `mmap` is declared directly against the C library that std already links.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- Bidirectional Dijkstra, which roughly halves settled nodes on road-like graphs, weighed against the memory of a reverse graph.
- Reorder node ids for locality (BFS order), so a search's working set shares cache lines.

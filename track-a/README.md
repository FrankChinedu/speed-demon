# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, zero dependencies. The log is memory-mapped and cut into one chunk per thread at line boundaries. Each thread parses lines into compact `(key, latency)` records. The key packs a thread-local endpoint id, the hour and the 5xx flag. Only the date and hour digits of a timestamp are read, and the date is cached between lines.

The endpoint tables are merged into one list sorted bytewise, so the dense group id `hour * endpoints + rank` is already the output order. A parallel counting sort scatters every latency into its group's slice, with per-thread cursors so no writes conflict. For each group, three nested `select_nth_unstable` calls find the nearest-rank p99, p95 and p50 without a full sort. Threads format disjoint group ranges, and the main thread writes them in order.

The step-by-step history, with measurements and failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `mmap` is declared directly against the C library that std already links.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- Cut peak memory (about 45 MB) by sorting each thread's records in place and selecting percentiles by a k-way merge across threads, which avoids the scatter array.
- SIMD (AVX2) search for the endpoint's terminating space.

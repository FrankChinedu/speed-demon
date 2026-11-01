# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, zero dependencies. The log is read with `pread` in 128 KB chunks, handed out to the threads on demand, into one reused buffer per thread (so each chunk is still in L2 when it is parsed). A chunk owns the lines that start in it and reads a little past its end to finish its last line. Each thread parses lines into 4-byte records `endpoint << 21 | latency`, appended to one block list per hour; the rare larger latencies and the 5xx lines go to small side lists of the hour. Only the date and hour digits of a timestamp are read, and the date is cached between lines.

Endpoints get global ids from a shared registry the first time a thread meets them; after that a thread-local table answers by a 64-bit hash of the name alone (a folded-multiply hash: the FxHash step used before collided on near-duplicate strings). The names are sorted bytewise once, which is the output order. Then one task per hour, in cache: a counting sort of the hour's records by endpoint rank, three nested `select_nth_unstable` calls per group for the nearest-rank p99, p95 and p50 without a full sort, and the hour's lines formatted. Finished hours are written in hour order.

The step-by-step history, with measurements and failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `prctl` (to opt out of transparent huge pages) is declared directly against the C library that std already links; `pread` is std's `FileExt::read_exact_at`.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- Cut peak memory (about 45 MB) by sorting each thread's records in place and selecting percentiles by a k-way merge across threads, which avoids the scatter array.
- SIMD (AVX2) search for the endpoint's terminating space.

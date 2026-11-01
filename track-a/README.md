# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, zero dependencies, one set of 12 threads for both phases (a barrier between them).

**Parse.** The log is read with `pread` in 128 KB chunks, handed out on demand, into one reused buffer per thread, so each chunk is still in L2 when it is parsed. A chunk owns the lines that start in it and reads 256 bytes past its end to finish its last line. Only the date (cached between lines) and the two hour digits of a timestamp are read. The method is skipped branch-free (its space is always in the word at byte 28). One AVX2 compare of the 64 bytes after the method finds the space after any endpoint shorter than 64 bytes (95% of lines), and the endpoint is hashed in a fixed run of vector steps (one XXH3 accumulate step and two folded multiplies), so no branch depends on its length. The latency is converted eight digits at a time with three multiplies.

**Endpoint ids.** One lock-free table shared by all threads: 4,096 one-word slots (32 KB, 53 hash bits above the id), read by every core and written once per name with a compare-exchange. A thread-local table with a locked registry behind it cost ~8 ms on the 12-thread box, all of it lock contention a 2-CPU machine never shows.

**Records.** Each line becomes a 4-byte record `endpoint << 21 | latency` (larger latencies and 5xx lines go to small side lists). Each hour has one mapping reserved up front for every thread's records; threads claim 512-record blocks from it with an atomic add, and the whole hour is returned with one `munmap` once summarized. Freeing each thread's own blocks instead took ~860 unmaps, each a TLB shootdown on every core, ~3.5 ms on the box.

**Summary.** One task per hour: a counting sort of the hour's records by endpoint rank (names sorted bytewise once, which is the output order), then nearest-rank p50, p95 and p99 per group by three selections, each on the part above the last (groups of 32 or fewer are sorted once instead), and the hour's lines formatted. Hours are deposited under a short lock and written in order by one thread at a time, outside the lock.

**Build.** Fat LTO, one codegen unit, `target-cpu=x86-64-v3`, and profile-guided optimisation: `pgo.profdata` is the profile of an instrumented build of this code run on full seeds 2 and 3 (never the board's seed 1). It only steers code layout; it cannot change any output.

The step-by-step history, with every board run, the timing probes, the x86 A/B results and the failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `prctl`, `mmap` and `munmap` are declared directly against the C library that std already links; `pread` is std's `FileExt::read_exact_at`.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) and `profile-use=pgo.profdata` for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- The parse is ~11 ms of compute over an I/O floor of ~8.6 ms (a box probe that only `pread`s the file). A block-wide structural mask (every line has exactly four spaces and one LF) would replace the per-line 64-byte window, if it measured faster on the box.
- Phase 2 is ~7 ms; merging the count pass into the parse failed before because of the per-line table update, but a per-hour count kept in the thread's block headers might not.

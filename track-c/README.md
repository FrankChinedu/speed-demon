# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, zero dependencies, 12 threads.

**Start-up.** The workers start at once. While the main thread reads `regions.csv` (2.5 ms, single-threaded), each worker faults in its share of the mapped `records.csv`, so the parse later finds those pages mapped. The region table maps a region id to its country in one byte per id (1 MB), widened to two only if a 255th country appears; the code's base-26 key indexes a 26^3 table, so there is no hashing.

**Splitting.** Section 2.2 of the spec, which solutions may rely on, guarantees that a line inside a quoted field never starts with a digit, while every record starts with a digit or a BOM. So each split point is the first newline after a guess that is followed by a digit or a BOM, found by looking at a few bytes, with no pass over the whole file first. Chunks are handed out on demand.

**Parse.** Each thread parses records straight from the map. Text fields are scanned 32 bytes per step with AVX2 (most fields end in the first step). A BOM at a record start is recognised by its first byte, since every other byte is ASCII (spec 2.2); a 3-byte `memcmp` call per record had cost 8%. Ids, region ids and amounts are converted with word arithmetic, amounts in integer cents. Name and note lengths are the raw lengths minus the `""` escapes, so no field is ever copied.

**Distinct names.** Names need no unescaping: raw field bytes identify a value exactly (an unquoted field cannot contain `"`, and unescaping is one-to-one). One lock-free table shared by all threads gives each name a dense id, keyed by a folded-multiply 64-bit hash of its bytes (the FxHash step used before collided on near-duplicate names: seed 6 has `Dacom Mina` and `Dacom Maca`). Threads take fresh ids 256 at a time, since nearly all 100,000 names are first met in the run's first milliseconds. Lookups go in batches of 32 (hash and prefetch every slot, then resolve), so their cache misses overlap. Each (country, id) pair is one bit in a shared bitset; the bits are written in batches: each thread queues its pairs per shard of countries and applies 256 at a time under the shard's lock, with no locked instruction per record.

**Build.** Fat LTO, one codegen unit, `target-cpu=x86-64-v3`, and profile-guided optimisation: `pgo.profdata` is the profile of an instrumented build of this code run on full seeds 2 and 3 (never the board's seed 1). It only steers code layout; it cannot change any output.

The step-by-step history, with every board run, the timing probes, the x86 A/B results and the failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `mmap` is declared directly against the C library that std already links.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) and `profile-use=pgo.profdata` for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- On the box the parse and join take ~28 ms (a probe without interning) and interning ~13 ms. The interning is bound by the 2 MB shared name table; a smaller one would need a guard for seeds with more distinct names than the spec's planning pool.
- Batching the region join with the name lookups measured 2.4% faster on the 2-CPU runner but not on the box; it may pay once the parse is cheaper.

# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, zero dependencies. **Splitting.** Section 2.2 of the spec, which solutions may rely on, guarantees that a line inside a quoted field never starts with a digit, while every record starts with a digit or a BOM. So each split point is the first newline after a guess that is followed by a digit or a BOM, found by looking at a few bytes, with no pass over the whole file first.

**Parse.** Each thread parses records straight from the map. Text fields are scanned 32 bytes per step with AVX2 on the scoring box (most fields end in the first step), and eight bytes per step by word arithmetic elsewhere: BOM skip, quoted fields with `""` escapes, CRLF or LF, ids and amounts by multiply-combine digit conversion, amounts in integer cents. It joins through a dense `region_id -> country` array and keeps running sums per country.

**Distinct names.** Names are identified by a folded-multiply 64-bit hash of their raw bytes (the FxHash step used before collided on near-duplicate names: seed 6 has `Dacom Mina` and `Dacom Maca`). They need no unescaping: raw field bytes identify a value exactly, because an unquoted field cannot contain `"` and unescaping is one-to-one (the two empty forms are special-cased to one value). One lock-free table, shared by all threads, gives each name a dense id. A slot is one word, a 40-bit hash tag above the id, so the table is 2 MB. Each country sets that id's bit in a shared bitset. Lookups go in batches of 32: hash and prefetch every slot, then resolve ids and prefetch the bitset words, then set the bits, so the cache misses of a batch overlap. Names are identified by 58 hash bits; for the spec's pool of 100,000 names the chance of a collision is about 2 * 10^-8 per run.

The step-by-step history, with every board measurement, the timing probes and the failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `mmap` is declared directly against the C library that std already links.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- Interning and the shared bitset are about 40% of the run (a box probe without them: 36 ms against 57 ms). Owner-partitioned bitset rows, which remove the locked RMW, saved 2 ms but cost memory. A cheaper route to distinct counts would be the next big step.
- A structural (simdcsv-style) parser was tried and measured no faster on the box: field scanning is not the bottleneck.

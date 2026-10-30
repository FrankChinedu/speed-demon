# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, zero dependencies. The file is split for threads by quote parity. Under the spec's grammar a byte is inside quotes exactly when an odd number of `"` bytes precede it, so a parallel count gives the exact quote state at each split point. Each chunk starts just after a terminator that is outside quotes. This uses only section 2.1 of the spec, not the generator's habits.

Each thread parses records with a byte-level state machine on the map: BOM skip, quoted fields with `""` escapes, CRLF or LF, and amounts in integer cents. It joins through a dense `region_id -> country` array and keeps running sums.

Distinct names need no unescaping. Raw field bytes identify a value exactly, because an unquoted field cannot contain `"` and unescaping is one-to-one. One lock-free table, shared by all threads, gives each raw name a dense id, and each country sets that id's bit in a zero-initialised bitset.

The step-by-step history, with measurements and failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `mmap` is declared directly against the C library that std already links.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- SIMD quote and comma search inside fields (AVX2 `movemask`). Field scanning is now the largest cost.
- Vectorise the quote-parity count used for splitting.

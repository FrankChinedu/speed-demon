# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Rust, zero dependencies. The queries are read first, and only their terms get ids, in an open-addressing table of 32-byte padded keys with hash tags. The corpus is memory-mapped and processed once, in chunks of about 10,000 consecutive documents.

Each document is tokenized into term ids. Phrases are matched right there: a phrase is found by its first two terms at each adjacent pair and then checked in full, so no positions are stored. Each document's distinct BOOL terms go through a per-chunk counting sort into posting lists of 16-bit offsets from the chunk's first document.

Because chunks are ascending and disjoint, a BOOL query runs strictly left to right per chunk, with galloping AND/NOT when list sizes differ a lot and merge otherwise, and the chunk results simply concatenate.

The step-by-step history, with measurements and failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `mmap` is declared directly against the C library that std already links.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- Bitmaps for the 369 terms with df > N/32, which hold half the postings. That saves memory, and AND/OR on them becomes word operations.
- A faster tokenizer: classify 32 bytes at a time with AVX2 instead of a byte table.

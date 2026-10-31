# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Rust, zero dependencies. The queries are read first, and only their terms are indexed. Query terms go into one open-addressing table of 32-byte slots, keyed by a strong 64-bit hash of the token, so a corpus token's lookup is one cache miss with no byte comparison. Each slot holds the term's id, its BOOL index and the phrases keyed on it.

**Corpus pass.** The corpus is memory-mapped and read once, in chunks of consecutive documents handed out dynamically (four per thread). Each document is classified 64 bytes at a time with AVX2 into a token bitmask, tokens are walked with trailing-zero counts, and their slot lookups are hashed and prefetched for the whole document before they are probed. Phrases are matched right there: each phrase is checked only where its least-used term occurs, so no positions are ever stored. Each document's distinct BOOL terms become postings, collected into 64 buffers by term range with their counts. At the end of the chunk they are scattered one term range at a time into 16-bit document offsets (terms in more than 1/16 of the chunk's documents become bitmaps), so the scatter stays in cache. Timing probes on the box showed the earlier all-terms-at-once scatter cost a third of the run.

**Queries.** Chunks are ascending and disjoint, so a BOOL query runs strictly left to right per chunk on lists or bitmaps and the chunk results concatenate. Queries run in parallel in blocks that are written back in order.

The step-by-step history, with every board measurement, the timing probes and the failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `mmap` is declared directly against the C library that std already links.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- Tokenizing is now about 60% of the run, at roughly 36 million tokens. A cheaper hash for tokens of 8 bytes or less (most of them) would cut it.
- The per-chunk index stores an offset table over all 53,000 BOOL terms. A compact table of only the terms present would save memory.

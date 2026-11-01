# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Rust, zero dependencies. The queries are read first, and only their terms are indexed. Every distinct query term goes into an open-addressing table keyed by a strong 64-bit hash of the token. The corpus pass reads a compact copy of it, one word per slot: the hash's top 46 bits as a tag, plus the term's BOOL index and a flag for phrases keyed on it. That is 1 MB for a full query set, so it fits the box's L2, and a term's slot is its id from then on. A corpus token is mistaken for a query term only if it shares the tag and its home slot falls in the short probe run, about 2 * 10^-8 per run for the spec's vocabulary.

**Corpus pass.** The corpus is memory-mapped and read once, in chunks of consecutive documents handed out dynamically (four per thread). Each 64-byte block is classified with AVX2 into a token bitmask, and the mask XOR itself shifted by one bit marks every token boundary, so tokens are visited with trailing-zero counts and no per-byte branch. A document's token hashes are computed and their slots prefetched first, then probed. Phrases are matched right there: each phrase is checked only where its least-used term occurs, so no positions are stored. Each document's distinct BOOL terms become postings, collected by term range and scattered at the end of the chunk into 16-bit document offsets; terms in more than 1/64 of the chunk's documents become bitmaps.

**Queries, inside the pass.** As soon as a chunk is indexed, its thread evaluates every BOOL query on it, strictly left to right on lists and bitmaps, while the index is still in cache, and then drops the index. Each thread keeps a running summary per BOOL query (count, sum, ten smallest ids); it meets its chunks in ascending order, so documents arrive in order. Once a summary has its ten smallest ids, the rest is counted in bulk: a bitmap word adds its popcount and the sum of its set bit positions. At the end the threads' summaries are merged per query (counts and sums add, the ten smallest ids merge) and formatted in parallel blocks, written in order.

The step-by-step history, with every board measurement, the timing probes, the x86 A/B timings and the failed experiments, is in [PERF.md](PERF.md).

## Dependencies

None. Standard library only. `mmap` is declared directly against the C library that std already links.

## Build

`Dockerfile`: `rust:1-slim-trixie` (rustc 1.99.1) builds a release binary with fat LTO, one codegen unit and `panic = "abort"`. `.cargo/config.toml` sets `target-cpu=x86-64-v3` (AVX2) for x86-64 only. The runtime image is `debian:13-slim`. Locally: `cargo run --release -- <data_dir> <out_dir>`.

## With another 24 hours

- Tokenizing is still about half the run (a box probe: ~148 ms of ~284 before the last steps). The next steps are a cheaper hash for tokens of 8 bytes or less and fewer passes over each document's token arrays.
- The plan phase (parsing 50,000 queries and filling the term table) is serial, ~20 ms on the box. Parsing and hashing can be split across threads, with slots claimed by compare-exchange.
- Peak memory (~83 MB) is mostly per-thread: each thread's posting buffers and its 1.7 MB of query summaries.

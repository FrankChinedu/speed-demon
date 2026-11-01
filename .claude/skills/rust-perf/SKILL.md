---
name: rust-perf
description: Speed Demon performance playbook for the Rust tracks in repo/track-*. Use when writing, auditing, profiling or optimising any track, deciding what to try next, or preparing a board push.
---

# Speed Demon performance playbook

K = (T_ref/T)^0.50 x min(M_ref/M, 50)^0.20 x min(S_ref/S, 1)^0.15 x (1/D)^0.15.
Freeze is Nov 2 15:00 UTC. Board runs are the scarce resource: one per team per 15 minutes, and each push runs all four tracks.

## 1. Start every session with the gap

```
python3 tools/board.py
```

The `x... to <leader>` line under each track splits the gap to the leader per axis (T, M, S) and gives the leader's numbers as targets. Work on the axis with the biggest factor. As of Oct 31 05:00 UTC, T was the biggest factor everywhere (x1.25-1.5) and M mattered on B and D (x1.15-1.19). S was worth at most x1.06.

Read the track's `PERF.md` before changing anything. It lists the failed experiments, so you don't repeat one.

## 2. The loop

1. Make one change. Run `python3 tools/sd.py run <t> --scale tiny`, then `--scale full`, and `SD_SEED=2 ... --scale full`. Every case must PASS.
2. `python3 tools/gate.py check` gates HEAD the same way the pre-push hook does: it builds the committed tree and checks several seeds.
3. Stack changes: one experiment per track per commit, so a single board slot measures up to four ideas.
4. After the board scores the push, `python3 tools/board.py` appends the row to `tools/runs.tsv`. Copy it into the track's PERF.md, failed experiments included.
5. Timing probe: a commit whose subject starts with `TIMING PROBE (fails the hash on purpose)` exits early to time a prefix of the pipeline. The gate skips these. Revert it in the next commit.

Local numbers (Apple M1, arm64) do not predict the box (i5-13500, x86-64, AVX2, 12 threads, 2.5 GHz). Use local runs for correctness and big wins only. Only board runs settle anything under about 15%.

## 3. Where the time goes, and what to try

The leaders' whole runs are faster than our parse-only probes (A: 0.019 s against 0.026 s; C: 0.026 s against 0.036 s). On those tracks the parse itself must get about 2x faster. Tuning the later stages cannot close that.

**SIMD byte classification (the main lever for A, B and C).** The build sets `target-cpu=x86-64-v3`, so `cfg(target_feature = "avx2")` holds on the box. Load 32 bytes, `_mm256_cmpeq_epi8` against each structural byte (`\n`, space, `"`, `,`), `_mm256_movemask_epi8` to get 32-bit masks, then walk the set bits with `trailing_zeros` and `m & (m - 1)`. One pass over 64-byte blocks gives every delimiter position with no per-byte branch. For C, quote parity over a block is a prefix XOR of the quote mask: carry-less multiply by all-ones (`_mm_clmulepi64_si128`, also in v3), or a shift-XOR ladder.
- Keep a scalar or SWAR path for `not(target_feature = "avx2")`. It is what runs locally, so it must stay correct.
- **The gate and `sd.py run` only exercise the scalar path** (native arm64). Keep the AVX2 path short and obviously equivalent. Try `DOCKER_DEFAULT_PLATFORM=linux/amd64 python3 tools/sd.py bench <t> --scale tiny` (Rosetta) to test it for correctness before spending a board slot. The timing from that run means nothing.
- Fixed-offset fields beat scanning. A's timestamp is always 27 bytes, so the method starts at byte 28. Use what the spec guarantees.

**Page faults and the mapping.** The input is about 240 MB, about 60k 4 KB pages. Faults on the mapping are serial per page but parallel across threads. Let each thread fault its own chunk rather than `MAP_POPULATE` on one thread. `madvise(MADV_WILLNEED)` per chunk is a cheap experiment.

**Memory (M is cgroup `memory.peak`).** Heap, `/tmp` and every page you touch count. The pre-warmed `/data` page cache does not.
- THP is already off (`mmap::small_pages`). With THP on, sparse tables pull in whole 2 MB pages.
- Peak is what matters. Free or reuse big buffers before the next phase allocates (A frees each hour's records once sorted).
- Smaller records: pack into `u32` and push rare large values to side lists (A's 21-bit latency), and use 16-bit offsets within chunks (B).
- D: holding warm per-thread search buffers beat returning them to the OS (step 4 vs 5 in its PERF.md). Shrink the buffers themselves instead.

**Things this repo already measured. Don't redo them blind.**
- On x86 the batched prefetch passes paid 14% in C. Prefetch is cheap to try anywhere a hash or graph lookup misses cache.
- Compact, cache-resident maps beat dense n-sized arrays (D step 2).
- Per-thread pair lists plus a merge were faster in C but cost too much M. Check K, not T alone.
- A per-side seen bitset in D was slower on the box.
- The bitmask parser in A was no faster than word-at-a-time and 38 lines longer.

**Output.** Format into one preallocated buffer per independent unit (A: per hour) and write in order with as few syscalls as possible. Don't drop large structures at the end: `std::process::exit(0)` after the final write skips the frees.

## 4. Hard rules

- **No crates.** D costs about 10% per dependency. Use std only: `std::thread::scope`, `std::sync::atomic`, and `std::arch` behind `cfg`. Declare FFI (`mmap`, `madvise`, `prctl`) yourself; std already links libc.
- **No async.** At most 12 worker threads.
- **Deterministic output.** Merge parallel results in a fixed order, and sort explicitly, never by hash-map iteration order. All five board runs must hash identically.
- **`unsafe`** only for FFI, `std::arch`, and `get_unchecked` in a measured hot loop whose bounds hold by construction. Each block gets a `// SAFETY:` comment; judges read the code.
- **Use the unseen seeds.** Code that fits seed 1's shape (counts, max lengths, ranges) fails on seed 2 or 3. Rely only on what the spec in `harness/specs/` promises.

## 5. S, last

S is scc's code-line count after rustfmt. Comments and blank lines are free, so never delete an explanation to save S. A 10% cut in lines is worth only about 1.6% K. Spend lines wherever they buy T. Do a trim pass on the final day only: inline single-use helpers and shorten expressions rustfmt splits over several lines.

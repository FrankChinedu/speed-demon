# Track D: Traverse: performance log

Measured locally with `tools/sd.py bench` (Docker on an Apple M1 Pro, 8 vCPUs, arm64, same sandbox flags as the board, data pre-warmed in a Docker volume), mean of the middle three of five runs. K uses the scoring-box reference numbers and the real S from `harness/static_axes.py`. Board run IDs are filled in from the live board once each step is pushed.

| Step | Change | Run ID | T (s) | M (MB) | K |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring box) | | 72.69 | 893 | 1.0 |
| 1 | Rust rewrite: parallel two-pass parse into CSR (count, then place at atomic cursors), Dijkstra per query with early exit, a per-query open-addressing distance map instead of an n-sized array per thread, O(1) answers for s == t, no out-edges and no in-edges | local | 1.606 | 134.9 | 7.2 |
| 2 | Failed: a dense per-thread distance array instead of the hash map. Slower (random access across 14 MB per thread against a compact, cache-resident map) and far more memory. Reverted | local | slower | - | - |
| 3 | Node ids are random, so every settled node is a cache miss, and CSR needs two (offset, then edges). Out-degree is at most 4 for 97% of nodes, so each node got one 32-byte aligned row of four edges with an overflow list for the rest: one miss and no offset table. Prefetch the row when a node is pushed (x86 only). Binary heap replaced by a radix heap (keys are monotone) | local | 1.096 | 156.9 | 8.2 |
| 4 | Failed: pinned glibc's mmap threshold so freed search buffers go back to the OS. Memory 151 to 94 MB, but every large query paid fresh page faults: 1.38 s, K 8.06. Reverted | local | 1.379 | 94.4 | 8.06 |
| 5 | The opposite: keep each thread's map and heap buffers warm, and clear the map by its list of used slots, so clearing costs what the last query used and not the table size | local | 0.791 | 151.7 | 9.7 |
| board | Same code as the step above, measured on the scoring box (commit aba13de, 12 threads, 2.5 GHz) | run-37ba0cad2a89 | 0.557 | 184.2 | 11.18 |

## Notes

- 2,000 queries, median Dijkstra rank 512, mean about 12,300. The largest octaves dominate total work.
- Memory is mostly the 56 MB of rows plus each thread's warm search buffers. On the box's 12 threads expect more than measured here on 8.

## Board runs

Every step measured on the scoring box by the live board, in order. Rows marked FAILED EXPERIMENT were measured and reverted.

| Commit | Change | Run ID | T (s) | M (MB) | S | K |
|---|---|---|---|---|---|---|
| | The reference, unchanged | | 72.69 | 893 | | 1.0 |
| aba13de | first Rust version | run-37ba0cad2a89 | 0.5570 | 184.2 | 407 | 11.18 |
| 13f3047 | bidirectional Dijkstra | run-5ab0bd0752b3 | 0.2827 | 171.0 | 462 | 15.63 |
| 19cf611 | FAILED EXPERIMENT: THP opt-out slows random row access | run-f7db74289338 | 0.3408 | 168.4 | 473 | 14.23 |
| 0f8bf51 | huge pages back | run-7f05af1ca5aa | 0.2707 | 167.6 | 462 | 16.04 |
| 8b02279 | (unchanged code, rerun) | run-ffe286c96772 | 0.2584 | 167.8 | 462 | 16.41 |
| 42a77bd | FAILED EXPERIMENT: word-arithmetic number parse + batched prefetch in the build: same time, +62 lines; reverted | run-8ac29186f904 | 0.2559 | 169.1 | 524 | 16.16 |
| dcc4444 | atomic rows instead of raw-pointer placement (no unsafe in the graph build), writeln! output: S 462 -> 425, time within noise | run-f64b920e8307 | 0.2756 | 168.7 | 425 | 16.07 |
| a16a1f9 | TIMING PROBE (fails the hash on purpose): exits after the graph build; T times everything before it | run-706f064acd88 | 0.1344 | 130.7 | - | - |
| 067d220 | FAILED EXPERIMENT: rows and degree counters from zeroed memory: the pages then fault lazily inside the random-write pass (2 MB huge pages zeroed on first touch): slower; reverted | run-40799334ffdf | 0.2876 | 170.6 | 437 | 15.64 |
| 02def11 | TIMING PROBE 2 (fails the hash on purpose): exits after the degree-counting pass | run-f08ccea48886 | 0.0437 | 31.5 | - | - |
| 667c1b0 | FAILED EXPERIMENT: zeroed rows pre-touched by all threads in parallel: no gain; reverted | run-eaeed085a509 | 0.2780 | 168.2 | 455 | 15.85 |
| 2ad69fb | build without atomics: bucketed counting sort, then each bucket's rows written by one worker: 0.25 s natively vs 0.36, but no gain on the box | run-cbfa7eec07d1 | 0.2715 | 165.3 | 495 | 15.89 |
| e4d2ed5 | word-arithmetic number parsing in the build passes (the bottleneck once atomics were gone) | run-06a89ee55ffb | 0.2463 | 166.4 | 509 | 16.59 |
| b122872 | build workers no longer hold the bucket lock while filling (the bucketed row build had been serialised); one-word distance-map entries | run-c3d8ba8148e2 | 0.1933 | 159.9 | 507 | 18.89 |
| 0ced1ba | TIMING PROBE 3 (fails the hash on purpose): exits after the bucketed build | run-c391e0c632a7 | 0.1318 | 158.0 | - | - |
| fe1c3bf | FAILED EXPERIMENT: per-side seen bitset to skip cross-side probes: slower; reverted | run-7c68dafc7493 | 0.2089 | 160.1 | 520 | 18.10 |
| 3f4cfda | TIMING PROBE (fails the hash on purpose): exits after the graph build, so queries take ~130 ms | run-e071af406290 | 0.0640 | 159.3 | - | - |
| 0f789db | FAILED EXPERIMENT: progressive madvise of edge arrays at 4 KB granularity: THP pages split but stay charged (233 MB) | run-98a13889884e | 0.2119 | 233.5 | 552 | 16.52 |
| 30de835 | FAILED EXPERIMENT: same with 2 MB-aligned release: memory still not returned on the box; reverted | run-fee778b3c1f5 | 0.1991 | 236.0 | 552 | 17.00 |

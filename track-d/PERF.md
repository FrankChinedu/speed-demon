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
- Memory (Nov 1): 16-byte rows of u32 edges make both row tables 56 MB, and only one 40 MB edge array is alive during the build. A box probe exiting after the build measures the peak there (127 MB), ~23 MB above the arrays; a probe exiting after the counting parse measures 8 MB, so that excess comes during the build. Thread stacks (256 KB) and pinned glibc thresholds did not change it.

## x86 A/B on the GitHub runner

Board noise on D is about ±8% (unchanged code measured 0.196 to 0.241 s), so after the first misjudged revert, candidates were timed by `dev/tools/ab.py` on an x86-64 runner: full seed-1 hash checked, 15 alternating runs each, bootstrap 95% CI.

| Candidate | Change | Speedup (95% CI) | Kept |
|---|---|---|---|
| 7362b59 | In-edges read back from the out rows into the same buffer instead of a second parse of the text | 1.073 (1.064-1.081) over the second parse | yes (it had been reverted on two noisy board samples, and was restored) |

An x86 profile of the full run puts 22% in the distance-map probe loop (every relaxation probes its own side's map and the other side's, both up to 1 MB and random), 7% in heap pushes and 6% in row loads. Cutting those probes needs dense per-node arrays (~14 MB per side per thread) or a goal-directed search.

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
| 01fdee3 | noise check: docs-only commit, code unchanged from 4dd375f | run-176c2326db53 | 0.2038 | 159.5 | 507 | 18.41 |
| 5042f52 | 16-byte rows of four u32 edges ((head + 1) << 11 | weight), one direction built at a time (text parsed again): M 159.7 -> 127.1, T within noise | run-7619465cb5d5 | 0.2152 | 127.1 | 509 | 18.73 |
| 41f7f9f | code unchanged from 9ff61a0 (slow box run, see A) | run-708452eb3f0e | 0.2276 | 127.0 | 509 | 18.22 |
| 86e958d | glibc mmap threshold pinned at 128 KB and one-word heap entries: M unchanged (128.3), so freed buffers in the arenas were not the cause | run-65b120e87a0a | 0.2035 | 128.3 | 521 | 19.16 |
| 87155c8 | code unchanged from fe2348f | run-10aabc67718c | 0.2137 | 125.4 | 521 | 18.78 |
| 7f10e38 | TIMING PROBE (fails the hash on purpose): exits after the build: 84 ms (64 before the second parse), M 127.3, so the build sets the peak, ~24 MB above its arrays (B shows the same ~24 MB: 12 thread stacks on huge pages?) | run-5d533060e3ff | 0.0841 | 127.3 | - | - |
| 7a22f66 | 256 KB worker stacks: M unchanged (125.3); reverted | run-d981a2d08b17 | 0.2086 | 125.3 | 522 | 19.01 |
| bfafaeb | in-edges read back from the out rows instead of a second parse: 0.2199, M 129.0; same run as B's slow sample, needs a resample | run-23b89a4be2b5 | 0.2199 | 129.0 | 542 | 18.31 |
| 1e03453 | TIMING PROBE (fails the hash on purpose): exits after the counting parse: 14.4 ms, M 8.0, so the ~23 MB above the arrays comes during the build, not from the process or its threads | run-79c9c91fe995 | 0.0144 | 8.0 | - | - |
| 8ce6ac2 | in-edges from the out rows, second sample: 0.2200 again, no faster than the second parse; reverted | run-3ead8c2a70bd | 0.2200 | 127.7 | 541 | 18.34 |
| 165fb12 | back to the second parse (same code as 0559a7e): 0.2258, so D now samples ~0.22 either way | run-24e72fc43c94 | 0.2258 | 126.9 | 521 | 18.24 |
| 529b16e | second parse (code unchanged since 0559a7e): 0.2411, board noise on D; x86 A/B puts in-edges-from-rows (7362b59) at 1.073x over this | run-fb24a58b1209 | 0.2411 | 124.6 | 521 | 17.71 |
| ffbc9b1 | in-edges read back from the out rows, restored (x86 A/B 1.073x over the second parse): 0.2129 | run-b057b94f104b | 0.2129 | 129.1 | 541 | 18.61 |
| 09b5340 | code unchanged from 6753993 | run-483d2267abd5 | 0.2254 | 127.7 | 541 | 18.13 |
| f6cc5dc | code unchanged from 6753993 (in-edges from the out rows): 0.1959, best so far | run-913b74adc131 | 0.1959 | 127.0 | 541 | 19.46 |
| ae1dbdb | code unchanged from 6753993: 0.1965 (second sample at ~0.196) | run-28d381916cb6 | 0.1965 | 124.8 | 541 | 19.50 |

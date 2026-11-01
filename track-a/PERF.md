# Track A: Ingest: performance log

Before the first push, steps were measured locally with `tools/sd.py bench` (Docker on an Apple M1 Pro, 8 vCPUs, arm64, same sandbox flags as the board, data pre-warmed in a Docker volume), mean of the middle three of five runs. K uses the scoring-box reference numbers and the real S from `harness/static_axes.py`. From the first push on, every step is a live-board run on the scoring box (table below).

## Before the board (local Docker, Apple M1, 8 vCPUs)

| Step | Change | Run ID | T (s) | M (MB) | K |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring box) | | 32.05 | 228 | 1.0 |
| 1 | Rust rewrite: mmap, one chunk per thread, compact (key, latency) records, sorted endpoint table so the group id is already the output order, parallel counting sort, three nested `select_nth_unstable` per group | local | 0.117 | 44.5 | 17.2 |

## Notes

- The first local measurement said 0.19 s and 285 MB. That was the macOS Docker bind mount charging the mapped input to the container. The board pre-warms `/data` on the host, so `sd.py bench` now copies the data into a Docker volume and warms it from a separate container first. The real figure is 44 MB.
- Timestamps are never fully parsed. Only the date (cached, since consecutive lines usually share it) and the two hour digits matter. The reference spends about 80% of its time in `strptime`.
- Percentiles use selection, not sorting. `select_nth_unstable` for p99, then p95 inside the left part, then p50 inside that.

- **What moved the box most** was removing work that only costs on 12 threads: one shared lock-free endpoint table instead of a locked registry (28.5 -> 19.9 ms of parse), one mapping per hour returned with one munmap instead of ~860 block frees (0.033 -> 0.028 s), and one thread pool for both phases. The 2-CPU x86 A/B runner (`dev/tools/ab.py`, 21 alternating runs, CI about 0.5%) judged the pure compute changes: the AVX2 endpoint window and vector hash (1.055x), one-word table slots (1.034x), no murmur finalizer (1.029x), upward selection (1.019x), PGO (1.027x).

## Board runs

Every step measured on the scoring box by the live board, in order. Rows marked FAILED EXPERIMENT were measured and reverted.

| Commit | Change | Run ID | T (s) | M (MB) | S | K |
|---|---|---|---|---|---|---|
| | The reference, unchanged | | 32.05 | 228 | | 1.0 |
| aba13de | first Rust version | run-2f8a43e19bdb | 0.0705 | 62.6 | 374 | 20.77 |
| 0d36516 | SWAR endpoint scan, own table, dynamic chunks, streamed output | run-1299d2d7b688 | 0.0592 | 62.5 | 468 | 21.92 |
| 13f3047 | THP opt-out | run-597c87e1cf05 | 0.0565 | 41.3 | 479 | 24.29 |
| 8b02279 | branch-free method skip, word-arithmetic latency | run-e1643fe393ea | 0.0540 | 41.4 | 488 | 24.77 |
| 92c4439 | prefix sums in parallel over group ranges (was one core over 132k groups x 12 threads) | run-b92e68934b04 | 0.0458 | 41.0 | 529 | 26.60 |
| a16a1f9 | TIMING PROBE (fails the hash on purpose): exits after parsing; T times everything before it | run-ee1e5b235496 | 0.0263 | 29.3 | - | - |
| 02def11 | TIMING PROBE 2 (fails the hash on purpose): exits after the key-rewrite pass | run-580f0e05762f | 0.0325 | 32.5 | - | - |
| e801d6f | FAILED EXPERIMENT: count per (endpoint, hour) while parsing to drop the key-rewrite pass: the per-line count update costs more inside the parse loop than the pass it removes; reverted | run-b5d87d3526e7 | 0.0574 | 42.4 | 535 | 23.59 |
| 2ad69fb | back to the separate key-rewrite pass | run-67e88afbcad4 | 0.0467 | 41.1 | 529 | 26.36 |
| 00310a5 | FAILED EXPERIMENT: delimiter bitmask parse (AVX2): no change on the box, +38 lines; reverted | run-d35c7a49c7af | 0.0458 | 41.3 | 567 | 26.31 |
| ba0a249 | TIMING PROBE 4 (fails the hash on purpose): only reads the whole input map in parallel | run-27e9071e51eb | 0.0099 | 11.3 | - | - |
| c5028ae | TIMING PROBE 5 (fails the hash on purpose): exits after the latency scatter | run-0945df59e033 | 0.0375 | 41.1 | - | - |
| 59fcb1b | per-hour 4-byte records, one in-cache task per hour, hash-identity endpoint ids | run-7e80b92f35c5 | 0.0396 | 22.3 | 475 | 32.88 |
| 68dec3f | TIMING PROBE (fails the hash on purpose): exits after the parse (256 KB mmap blocks) | run-8760faae8839 | 0.0326 | 17.8 | - | - |
| 198096d | TIMING PROBE (fails the hash on purpose): exits after the parse, 4 KB heap blocks: same, so block mmaps are not the cost | run-29867f0b098f | 0.0310 | 17.5 | - | - |
| 5a4630c | FAILED EXPERIMENT: thread-local ids plus one record mapping, 16 KB blocks released by madvise: slower | run-982e6bce4f52 | 0.0475 | 23.3 | 512 | 29.41 |
| 1614751 | FAILED EXPERIMENT: same without releasing blocks: still slower than 59fcb1b; both reverted | run-09da972c1dfc | 0.0432 | 32.9 | 512 | 28.77 |
| 70cc9cd | FAILED EXPERIMENT: MADV_POPULATE_READ per input chunk: no change; reverted | run-43815d55bb2f | 0.0405 | 22.3 | 488 | 32.37 |
| 3f4cfda | FAILED EXPERIMENT: branch-free AVX2 endpoint scan and folded hash: 2% faster, +71 lines, lower K; reverted | run-7fef89ad3f28 | 0.0388 | 22.2 | 546 | 32.56 |
| 18cf889 | TIMING PROBE (fails the hash on purpose): parse only, endpoint table removed: the table costs ~9 ms | run-589103ab56b3 | 0.0227 | 17.3 | - | - |
| 7a08c2d | FAILED EXPERIMENT: thread-local endpoint ids alone (no registry): slower again; reverted | run-5146a0a7a7e3 | 0.0435 | 23.0 | 456 | 31.37 |
| 81e0731 | noise check: unchanged 59fcb1b code | run-b1a450557639 | 0.0432 | 22.9 | 475 | 31.29 |
| 209c01f | FAILED EXPERIMENT: one-word 32 KB endpoint table: no change; reverted | run-37d9f7da7468 | 0.0425 | 22.0 | 476 | 31.79 |
| 9c83538 | FAILED EXPERIMENT: two halves of each chunk parsed in lockstep: no change; reverted | run-343c93875116 | 0.0404 | 22.5 | 491 | 32.31 |
| e041e16 | TIMING PROBE (fails the hash on purpose): lookups done, records independent of them: the lookup work itself is the ~10 ms | run-6bab828bfa12 | 0.0338 | 18.0 | - | - |
| 0ad8d98 | FAILED EXPERIMENT: cold insert path out of line: no gain (box noisy this run); reverted | run-0ab5b869a85f | 0.0437 | 23.9 | 479 | 30.80 |
| 01fdee3 | noise check: docs-only commit, code unchanged from 4dd375f | run-e3e705c92504 | 0.0442 | 23.6 | 475 | 30.75 |
| 14db9e8 | FAILED EXPERIMENT: bucketed endpoint table, eight one-word slots per bucket compared with selects (no probe loop): within noise, +15 lines; reverted | run-264d7a39c5c5 | 0.0426 | 21.6 | 490 | 31.71 |
| 211cbe9 | input read with pread into 128 KB per-thread buffers instead of mmap: time within noise, -26 lines, +1 MB | run-de8b3fa36b19 | 0.0425 | 23.8 | 449 | 31.58 |
| 8c6f070 | folded-multiply hash step (correctness: FxHash collided on near-duplicate names): no time change | run-ca8f39816221 | 0.0394 | 24.5 | 450 | 32.59 |
| 1abd716 | code unchanged from 8c6f070 | run-5c0459801eb9 | 0.0410 | 23.7 | 450 | 32.16 |
| babcf41 | probe reverted: pread version of 8c6f070 | run-ea14c4ddb000 | 0.0410 | 24.1 | 450 | 32.05 |
| 3dfa93b | TIMING PROBE (fails the hash on purpose): only preads the whole input in 128 KB chunks in parallel (the mmap read-only probe was 9.9 ms) | run-42cd2466289d | 0.0086 | 7.7 | - | - |
| 7dbbf8a | TIMING PROBE (fails the hash on purpose): exits after the parse (pread version): phase 2 is about 11 ms | run-7b5957e0df3e | 0.0299 | 20.3 | - | - |
| 4ad05b8 | TIMING PROBE (fails the hash on purpose): every hour formatted, nothing written: the output write costs about 1 ms | run-58bde850099f | 0.0400 | 21.7 | - | - |
| e542027 | code unchanged from 0abefaf (noise) | run-52b88e564fb6 | 0.0458 | 24.2 | 450 | 30.33 |
| 4c412e0 | percentiles selected upward (x86 A/B 1.019x) | run-ebc3e478610a | 0.0408 | 24.6 | 450 | 32.00 |
| 0abea93 | endpoint hash without the murmur3 finalizer (x86 A/B 1.029x) | run-fc6f5b4c2f6c | 0.0404 | 23.4 | 442 | 32.56 |
| 8cab084 | code unchanged from 0abea93 | run-eaa1a15098c2 | 0.0406 | 24.4 | 442 | 32.21 |
| fdec699 | code unchanged from 0abea93 | run-cfeccd7afa2e | 0.0378 | 23.9 | 442 | 33.50 |
| 27a96ae | glibc mmap threshold pinned at 64 KB: M 23.9 -> 21.3 MB but slower (x86 A/B 0.984x); reverted | run-b212166f107e | 0.0419 | 21.3 | 450 | 32.49 |
| 635fc59 | endpoint found by one AVX2 space mask, XXH3-style hash (x86 A/B 1.055x) | run-4c24db345b4c | 0.0387 | 24.1 | 500 | 32.50 |
| ff44fa3 | one-word endpoint slots, 32 KB table (x86 A/B 1.034x) | run-91324ea3d371 | 0.0370 | 23.5 | 502 | 33.37 |
| 851ead2 | groups of up to 32 latencies sorted once (x86 A/B 1.010x) | run-81c808a916c7 | 0.0391 | 23.6 | 506 | 32.38 |
| 83fb9ee | TIMING PROBE (fails the hash on purpose): exits after the parse, current code: 28.5 ms (29.9 at 7dbbf8a), so the x86 compute gains barely reach the box | run-8fd9a8774ea8 | 0.0285 | 19.8 | - | - |
| 64ac480 | one lock-free endpoint table shared by all threads instead of a registry lock per first sighting per thread: 0.0338 (contention the 2-CPU A/B runner cannot show) | run-3304286ef79e | 0.0338 | 23.0 | 505 | 35.00 |
| 257172c | code unchanged from 64ac480 (second sample) | run-d55574a0c150 | 0.0329 | 23.1 | 505 | 35.46 |
| 46626c1 | TIMING PROBE (fails the hash on purpose): exits after the parse with the shared endpoint table: 19.9 ms (28.5 before), so phase 2 is now ~13 ms | run-a5d1e237911e | 0.0199 | 19.7 | - | - |
| dd8490a | MEASUREMENT: record blocks leaked instead of freed in phase 2: 0.0296 vs ~0.033, so the frees cost ~3.5 ms (M +11.6 MB, not kept) | run-46fa46b9dfc3 | 0.0296 | 34.7 | 506 | 34.48 |
| 6ed2372 | each hour's records in one reserved mapping, released with one munmap (was ~860 block frees): 0.0282, M 22.2 | run-eba9e241931a | 0.0282 | 22.2 | 570 | 37.91 |
| 4a09495 | code unchanged from 6ed2372 (second sample) | run-b237bd439fa8 | 0.0277 | 21.9 | 570 | 38.40 |
| 022abb8 | one set of threads for both phases, barrier between them | run-c1be9703f5fe | 0.0274 | 21.9 | 570 | 38.55 |
| 8172e57 | code unchanged from 022abb8 (second sample) | run-c54ef44cdcd1 | 0.0273 | 22.1 | 570 | 38.60 |
| 6de5579 | code unchanged from 022abb8 (third sample) | run-b995a9c9a120 | 0.0277 | 22.4 | 570 | 38.20 |
| 1fa8e0d | 570 -> 516 code lines (x86 A/B 1.005x); T looks like a slow sample | run-1dd8f5492ac4 | 0.0293 | 21.6 | 516 | 37.95 |
| bc5e6ce | summary hours written outside the writer lock | run-453835a024df | 0.0273 | 22.9 | 526 | 38.76 |
| 7f10e38 | code of bc5e6ce (second sample) | run-aa2777b794f4 | 0.0267 | 22.4 | 526 | 39.34 |
| 8ce6ac2 | code unchanged from bfafaeb | run-1e14832326e7 | 0.0322 | 21.6 | 548 | 35.91 |
| 165fb12 | code unchanged from bfafaeb | run-9665347884c5 | 0.0289 | 21.1 | 548 | 38.06 |
| 529b16e | FAILED TUNING: 256 KB read chunks: 0.0293 and 0.0275 (with f6cc5dc) against ~0.027, M +1 MB; reverted | run-7aeb25c85f14 | 0.0293 | 23.5 | 526 | 37.22 |
| f6cc5dc | code of 529b16e (second sample) | run-2c4522dc84c2 | 0.0275 | 23.3 | 526 | 38.54 |
| ecdc8d2 | TUNING (7865cb4): record blocks of 512 instead of 1,024: M 21.7 (from ~22.5), T unchanged; kept | run-141cc11e0268 | 0.0271 | 21.7 | 526 | 39.34 |

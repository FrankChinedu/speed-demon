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

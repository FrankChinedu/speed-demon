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

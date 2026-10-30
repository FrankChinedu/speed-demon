# Track A: Ingest: performance log

Measured locally with `tools/sd.py bench` (Docker on an Apple M1 Pro, 8 vCPUs, arm64, same sandbox flags as the board, data pre-warmed in a Docker volume), mean of the middle three of five runs. K uses the scoring-box reference numbers and the real S from `harness/static_axes.py`. Board run IDs are filled in from the live board once each step is pushed.

| Step | Change | Run ID | T (s) | M (MB) | K |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring box) | | 32.05 | 228 | 1.0 |
| 1 | Rust rewrite: mmap, one chunk per thread, compact (key, latency) records, sorted endpoint table so the group id is already the output order, parallel counting sort, three nested `select_nth_unstable` per group | (pending push) | 0.117 | 44.5 | 17.2 |

## Notes

- The first local measurement said 0.19 s and 285 MB. That was the macOS Docker bind mount charging the mapped input to the container. The board pre-warms `/data` on the host, so `sd.py bench` now copies the data into a Docker volume and warms it from a separate container first. The real figure is 44 MB.
- Timestamps are never fully parsed. Only the date (cached, since consecutive lines usually share it) and the two hour digits matter. The reference spends about 80% of its time in `strptime`.
- Percentiles use selection, not sorting. `select_nth_unstable` for p99, then p95 inside the left part, then p50 inside that.

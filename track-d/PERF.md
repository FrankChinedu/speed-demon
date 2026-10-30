# Track D: Traverse: performance log

Before the first push, steps were measured locally with `tools/sd.py bench` (Docker on an Apple M1 Pro, 8 vCPUs, arm64, same sandbox flags as the board, data pre-warmed in a Docker volume), mean of the middle three of five runs. K uses the scoring-box reference numbers and the real S from `harness/static_axes.py`. From the first push on, every step is a live-board run on the scoring box (table below).

## Before the board (local Docker, Apple M1, 8 vCPUs)

| Step | Change | Run ID | T (s) | M (MB) | K |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring box) | | 72.69 | 893 | 1.0 |
| 1 | Rust rewrite: parallel two-pass parse into CSR (count, then place at atomic cursors), Dijkstra per query with early exit, a per-query open-addressing distance map instead of an n-sized array per thread, O(1) answers for s == t, no out-edges and no in-edges | local | 1.606 | 134.9 | 7.2 |
| 2 | Failed: a dense per-thread distance array instead of the hash map. Slower (random access across 14 MB per thread against a compact, cache-resident map) and far more memory. Reverted | local | slower | - | - |
| 3 | Node ids are random, so every settled node is a cache miss, and CSR needs two (offset, then edges). Out-degree is at most 4 for 97% of nodes, so each node got one 32-byte aligned row of four edges with an overflow list for the rest: one miss and no offset table. Prefetch the row when a node is pushed (x86 only). Binary heap replaced by a radix heap (keys are monotone) | local | 1.096 | 156.9 | 8.2 |
| 4 | Failed: pinned glibc's mmap threshold so freed search buffers go back to the OS. Memory 151 to 94 MB, but every large query paid fresh page faults: 1.38 s, K 8.06. Reverted | local | 1.379 | 94.4 | 8.06 |
| 5 | The opposite: keep each thread's map and heap buffers warm, and clear the map by its list of used slots, so clearing costs what the last query used and not the table size | local | 0.791 | 151.7 | 9.7 |

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

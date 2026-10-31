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

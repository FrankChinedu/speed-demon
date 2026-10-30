# Track C: Join: performance log

Before the first push, steps were measured locally with `tools/sd.py bench` (Docker on an Apple M1 Pro, 8 vCPUs, arm64, same sandbox flags as the board, data pre-warmed in a Docker volume), mean of the middle three of five runs. K uses the scoring-box reference numbers and the real S from `harness/static_axes.py`. From the first push on, every step is a live-board run on the scoring box (table below).

## Before the board (local Docker, Apple M1, 8 vCPUs)

| Step | Change | Run ID | T (s) | M (MB) | K |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring box) | | 14.44 | 367 | 1.0 |
| 1 | Rust rewrite: parallel split by quote parity (a byte is inside quotes exactly when an odd number of `"` precede it), byte-level record parser on the map, per-thread aggregates; distinct names via per-thread `HashMap<Cow<[u8]>>`, per-thread (country, name) pairs, then a single-threaded global merge | local | 0.501 | 83.0 | 5.9 |
| 2 | Profile: interning about 80 ms, field scanning about 65 ms, single-threaded merge about 55 ms (10 threads, native). Raw field bytes identify a name value exactly (unquoted fields cannot hold `"`, and unescaping is one-to-one), so there is no unescaping for identity. One lock-free table shared by all threads hands out dense ids, and each country sets bits in a zero-initialised bitset that is never touched past the ids in use. No per-thread maps, no pairs, no merge | local | 0.272 | 25.7 | 10.1 |

## Notes

- Verified on seeds 1, 2 and 3 at tiny and sample scale, and on seed 1 at full, against the reference's output.
- `name_bytes` and `note_bytes` are raw lengths minus the number of `""` escapes, so no field is ever copied.

## Board runs

Every step measured on the scoring box by the live board, in order. Rows marked FAILED EXPERIMENT were measured and reverted.

| Commit | Change | Run ID | T (s) | M (MB) | S | K |
|---|---|---|---|---|---|---|
| | The reference, unchanged | | 14.44 | 367 | | 1.0 |
| aba13de | first Rust version | run-a6909ba84955 | 0.1100 | 65.5 | 426 | 13.17 |
| 13f3047 | THP opt-out, bounded name tables, dynamic chunks, SWAR fields | run-0d66f1b8d721 | 0.1081 | 15.7 | 474 | 17.40 |
| 19cf611 | (unchanged code, rerun) | run-c8c506218df8 | 0.0998 | 16.1 | 474 | 18.02 |
| 0f8bf51 | FAILED EXPERIMENT: per-thread pairs, faster but more memory | run-809998ace672 | 0.0912 | 29.1 | 552 | 16.37 |
| 8b02279 | shared bitset back, masked name words, direct regions | run-22d82e8e4eb0 | 0.0800 | 17.0 | 558 | 19.42 |
| 5dde67a | FAILED EXPERIMENT: plain per-record interning without the batched prefetch passes: -50 lines but 14% slower (prefetch pays on x86); reverted | run-56db6cebd935 | 0.0909 | 16.8 | 508 | 18.53 |
| b6426c7 | batched prefetch interning restored | run-13e118fa1487 | 0.0794 | 17.3 | 558 | 19.42 |

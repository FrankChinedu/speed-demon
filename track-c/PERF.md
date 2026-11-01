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
| a16a1f9 | TIMING PROBE (fails the hash on purpose): exits after setup and the quote-parity split; T times everything before it | run-4432ab4b407d | 0.0142 | 8.7 | - | - |
| 067d220 | FxHash step as a plain function instead of a Hasher impl (S 558 -> 536) | run-00f1dee770bf | 0.0718 | 16.8 | 536 | 20.67 |
| 02def11 | TIMING PROBE 2 (fails the hash on purpose): exits after the parse loop | run-8f253c460e83 | 0.0785 | 16.9 | - | - |
| 667c1b0 | word-arithmetic parsing of ids, region ids and amounts: within noise | run-426b991ca132 | 0.0775 | 16.9 | 550 | 19.80 |
| b122872 | split points from the spec 2.2 guarantee instead of a whole-file quote count: -23 lines, time within noise | run-3dae84d9223d | 0.0714 | 16.2 | 527 | 20.95 |
| 4df3012 | names identified by a strong 64-bit hash in one-line slots: one cache miss per lookup, no name arena or byte compare (see the note in the code on exactness) | run-a9e03fd3bd64 | 0.0601 | 13.7 | 497 | 23.82 |
| 79f1da0 | TIMING PROBE 6 (fails the hash on purpose): parse without interning names | run-fb6f568a243d | 0.0358 | 8.4 | - | - |
| b777978 | TIMING PROBE 7 (fails the hash on purpose): interning without the bitset writes | run-da5cc602094b | 0.0651 | 12.3 | - | - |
| 81e0731 | FAILED EXPERIMENT: PREFETCHW for the shared bitset: no change; reverted | run-59e79fdb7e13 | 0.0594 | 14.0 | 507 | 23.78 |
| 30de835 | one-word name slots (40-bit tag above the id): 2 MB table instead of 4 MB | run-9ae715470d5f | 0.0573 | 11.3 | 493 | 25.36 |
| 5892567 | FAILED EXPERIMENT: structural-mask record parser: no faster, more memory; reverted | run-1702f527be4c | 0.0586 | 13.7 | 515 | 23.98 |
| 209c01f | TIMING PROBE (fails the hash on purpose): no name interning: interning and the shared bitset cost ~21 ms | run-caa95fc37fcf | 0.0361 | 8.3 | - | - |
| 241d75d | FAILED EXPERIMENT: owner-written bitset rows (no locked RMW), byte region table: 2 ms faster but more memory and lines, lower K; reverted | run-a6ac5a373a1e | 0.0552 | 12.9 | 542 | 24.83 |
| 0ad8d98 | FAILED EXPERIMENT: cold claim path out of line: no gain; reverted | run-3f1b11477bac | 0.0599 | 11.4 | 500 | 24.70 |
| 01fdee3 | noise check: docs-only commit, code unchanged from 4dd375f | run-34ab624a63be | 0.0594 | 11.0 | 493 | 25.06 |
| 8c6f070 | folded-multiply hash step (correctness: FxHash collided on near-duplicate names): no time change | run-cbd75f941041 | 0.0588 | 11.2 | 494 | 25.09 |
| 1abd716 | FAILED EXPERIMENT: region join batched with the name lookups (region slot prefetched): within noise, +19 lines; reverted | run-7bcd424bccf7 | 0.0578 | 11.9 | 513 | 24.86 |
| 3dfa93b | AVX2 field scan, 32 bytes per step | run-dfda535984a0 | 0.0550 | 11.3 | 506 | 25.79 |

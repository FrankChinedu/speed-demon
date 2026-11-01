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

- **What moved the box most:** the AVX2 field scan, one-byte BOM test (1.080x on the x86 A/B runner), the inlined field reader (1.039x), sharded batched bitset writes (1.073x), per-thread blocks of name ids (box only: the shared counter is contended by 12 threads), the one-byte region table (M 11.8 -> 10.6 MB) and PGO (1.049x). The x86 A/B runner (`dev/tools/ab.py`) has only 2 CPUs, so several of its wins (batched join, vector name hash) did not show on the box and were reverted.

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
| babcf41 | AVX2 field scan, second run (code of 3dfa93b) | run-39e8f2e48db0 | 0.0554 | 11.3 | 506 | 25.69 |
| 7dbbf8a | TIMING PROBE (fails the hash on purpose): names hashed, no table lookup, no bitset: lookup plus bitset cost about 19 ms | run-7bbabded3017 | 0.0361 | 7.5 | - | - |
| 4ad05b8 | TIMING PROBE (fails the hash on purpose): names looked up, no bitset prefetch or writes: the bitset costs about 1 ms, the name lookup about 18 ms | run-4f457ea453b8 | 0.0540 | 7.8 | - | - |
| e542027 | seen-bitset writes queued per thread by shard of countries, applied under one lock per 2048 pairs | run-c10022693196 | 0.0477 | 13.6 | 530 | 26.49 |
| 4c412e0 | text field reader forced inline (x86 A/B 1.039x); board noise: slower than the run before | run-d8f7e20fd693 | 0.0531 | 14.0 | 530 | 24.98 |
| 0abea93 | region join batched with the name lookups (x86 A/B 1.024x); box: no better than 4c412e0 | run-47cadd8d72a8 | 0.0532 | 14.0 | 551 | 24.80 |
| 8cab084 | check: sharded bitset alone (code of e542027) is also ~0.053 on the box, so e542027 at 0.0477 was a lucky run | run-33d21f3ee9b6 | 0.0526 | 14.4 | 530 | 24.94 |
| fdec699 | forced-inline field reader; bitset queues of 256 pairs instead of 2048: memory back to 11.8 MB | run-7c5f12f6cc30 | 0.0474 | 11.8 | 530 | 27.35 |
| 27a96ae | name ids reserved 256 at a time per thread; one-byte region table: M 10.6 MB | run-fdce8690114a | 0.0452 | 10.6 | 569 | 28.30 |
| 635fc59 | code unchanged from 27a96ae | run-c0c4da44881b | 0.0448 | 11.1 | 569 | 28.16 |
| ff44fa3 | BOM detected by its first byte, no memcmp call per record (x86 A/B 1.080x) | run-3f7cbc038582 | 0.0419 | 11.2 | 569 | 29.12 |
| 851ead2 | name hash without the murmur3 finaliser (x86 A/B 1.021x) | run-7c6b51a8a5cc | 0.0412 | 10.6 | 565 | 29.66 |
| 83fb9ee | code unchanged from 851ead2 (second sample) | run-568c9acacfe9 | 0.0411 | 10.6 | 565 | 29.72 |
| 64ac480 | code unchanged from 851ead2 (third sample) | run-95ae19619dee | 0.0413 | 10.9 | 565 | 29.49 |
| 257172c | FAILED EXPERIMENT: names under 32 bytes hashed by one AVX2 XXH3 step (x86 A/B 1.029x): no visible gain on the box, +38 lines, lower K; reverted | run-1adcc68356e0 | 0.0409 | 10.8 | 603 | 29.38 |
| 46626c1 | code of 851ead2 (XXH3 name hash reverted) | run-d0fdac40f096 | 0.0410 | 11.0 | 565 | 29.56 |
| dd8490a | FAILED EXPERIMENT: 32 chunks per thread instead of 8: no gain; reverted | run-e20a32f76633 | 0.0422 | 10.6 | 565 | 29.30 |
| 6ed2372 | code unchanged from 851ead2 | run-5b6a281a3a91 | 0.0413 | 11.1 | 565 | 29.36 |
| 4a09495 | TIMING PROBE (fails the hash on purpose): parse and join, no name interning: 27.9 ms, so interning and the bitset cost ~13.4 ms | run-c67933267f82 | 0.0279 | 7.5 | - | - |
| 022abb8 | code unchanged from 851ead2 | run-e3d43c225be2 | 0.0434 | 11.1 | 565 | 28.66 |
| 8172e57 | TIMING PROBE (fails the hash on purpose): exits after loading regions.csv: 3.1 ms of serial start-up | run-4af1957f0861 | 0.0031 | 7.9 | - | - |
| 6de5579 | FAILED EXPERIMENT: regions.csv loaded by all threads in two passes: no gain (two more thread spawns cost what it saved), +41 lines; reverted | run-216886e90743 | 0.0425 | 11.0 | 608 | 28.67 |
| 1fa8e0d | 565 -> 515 code lines (x86 A/B within noise) | run-458d2f1e49c4 | 0.0410 | 11.0 | 515 | 29.96 |
| bc5e6ce | code unchanged from 1fa8e0d | run-43f317ca7e9f | 0.0417 | 10.5 | 515 | 30.00 |
| 7f10e38 | FAILED EXPERIMENT (a5e47cc): huge pages left on with one malloc arena: no gain; reverted | run-7a42b5487298 | 0.0413 | 10.6 | 515 | 30.06 |
| 529b16e | FAILED TUNING: interning batches of 16: 0.0410 and 0.0409, no change; reverted | run-36b1c4a30c6c | 0.0410 | 11.2 | 515 | 29.85 |
| f6cc5dc | code of 529b16e (second sample) | run-ade2730ae19d | 0.0409 | 11.0 | 515 | 29.99 |
| ecdc8d2 | FAILED TUNING (7865cb4): bitset queues of 512: M 11.3 (from ~10.8), T unchanged; reverted | run-bdedccb4b1e8 | 0.0406 | 11.3 | 515 | 29.96 |

# Track B: Index: performance log

Before the first push, steps were measured locally with `tools/sd.py bench` (Docker on an Apple M1 Pro, 8 vCPUs, arm64, same sandbox flags as the board, data pre-warmed in a Docker volume), mean of the middle three of five runs. K uses the scoring-box reference numbers and the real S from `harness/static_axes.py`. From the first push on, every step is a live-board run on the scoring box (table below).

## Before the board (local Docker, Apple M1, 8 vCPUs)

| Step | Change | Run ID | T (s) | M (MB) | K |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring box) | | 56.35 | 2,200 | 1.0 |
| 1 | Rust rewrite: index only the query terms, two tokenizer passes (count, then write one exact-size global arena), phrases matched inline by a (first, second) term pair table so no positions are stored, BOOL left to right on sorted lists | local | 1.694 | 128.7 | 7.7 |
| 2 | Failed: replaced `HashMap<&[u8], u32>` with a custom open-addressing table (32-byte padded keys, 32-bit hash tags). No measurable change, because the lookups were not the bottleneck. Kept, because it is no slower and has simpler hashing | local | ~1.69 | ~129 | 7.7 |
| 3 | A sampling profile showed the two tokenizer passes cost about the same, so the second pass was pure overhead. Now one pass: each chunk of about 10k documents builds its own posting lists by counting sort, and BOOL queries run per chunk (chunks are ascending, disjoint doc ranges, so results concatenate) | local | 0.805 | 196.5 | 10.3 |
| 4 | Memory: the 8-byte (term, doc) pairs held per chunk in flight became per-document term lists, and lists store 16-bit offsets from the chunk's first document | local | 0.719 | 91.8 | 12.7 |

## Notes

- 21.1M (term, doc) postings for 53,444 BOOL terms. 369 terms with df > N/32 hold half of them.
- Box timing probes (Nov 1): the plan is 23 ms (18 of it parsing and filling the term table); tokenizing alone ends at 141 ms of a ~244 ms run, and evaluating the queries per chunk costs ~35-50 ms. Earlier, tokenizing alone was ~148 ms of a ~284 ms run (154 ms before the boundary walk, 168 ms with the 8 MB term table); postings, phrases, the per-chunk scatter and the queries were the other ~136 ms.
- Memory on the box carries ~24 MB more than the arrays explain (B's tokenize-only probe 31.6 MB against ~6 MB of data; D shows the same). It is not thread stacks (256 KB stacks changed nothing) and not malloc arenas (one arena saved 3.7 MB).
- Tokenization is byte-exact ASCII: `A-Z` lowercased, runs of `a-z0-9`, every other byte (including all UTF-8 bytes) separates.

## x86 A/B on the GitHub runner

From Nov 1 afternoon, board noise on B (the same code measured 0.279 and 0.323 s) was larger than most single steps, so each candidate was first timed against its base by `dev/tools/ab.py` (`.github/workflows/ab.yml`): both commits built on an x86-64 runner (2 vCPUs, AVX2), the full seed-1 output hash checked, then 15 alternating runs each, with a bootstrap 95% confidence interval of the speedup. Only winners went to the board. Failures are listed too.

| Candidate | Change | Speedup (95% CI) | Kept |
|---|---|---|---|
| 9327878 | Tokens walked boundary by boundary (mask XOR itself shifted marks every start and end) instead of two bit scans per token; lookup tests a hit first | 1.018 (1.012-1.025) | yes |
| 23f2428 | Branch-free first-occurrence test for postings (write every token's term, advance only for a new BOOL term) | 0.967 (0.955-0.981) | no |
| 68b9d50 | Each term's last document and chunk count side by side; chunk index buffers reused per thread | 1.020 (1.017-1.034) | the first half: reuse cost 5 MB on the box and timed 1.001 (0.992-1.009) without it |
| 0ee7820 | Corpus pass reads a compact 1 MB copy of the term table (one word per slot: 46-bit tag, BOOL index, keyed flag) | 1.017 (1.012-1.028) | yes |
| 63c2495 | Bitmaps for terms in more than 1/64 of a chunk (was 1/16): the index is dropped after its chunk's queries, so bitmaps cost no lasting memory, and most query time was short list merges | 1.023 (1.020-1.026) | yes |
| 1/128, 1/256 | The same threshold pushed further | 0.988 (0.970-0.999), 0.958 (0.934-0.976) | no |
| e7c72cb | Results summarised in bulk once their ten smallest ids are known: a bitmap word adds its popcount and the sum of its set bit positions (six masked popcounts) | 1.074 (1.071-1.077) | yes |
| 7a33ede | Postings entry shrunk to (u16, u16) and prefetched from the tokenizer | 0.983 (0.973-0.997) | no |
| a29740b | The (u16, u16) entry alone | 1.012 (1.001-1.015) | yes |
| 934fc6e | Each block's token mask walked as soon as it is made (no mask buffer); token hashes written by index into a buffer sized once per document | 1.066 (1.053-1.076) | yes |
| aa60984 | No slot prefetch in the tokenizer (the profile put 7% on the prefetch instruction) | 0.986 (0.974-1.004) | no: the time was the load it waits for |
| ec9a972 | Token hash by three folded 64x64 -> 128-bit multiplies instead of three multiplies and a MurmurHash3 finaliser | 1.002 (0.979-1.029) | no |
| 0297d50 | The plan in parallel (flat per-thread parse, lock-free set of distinct terms) | 0.913 (0.903-0.923) on the 2-vCPU runner | no: a box probe put it at 19.9 ms against the serial 23.0, not worth 44 lines |
| 8c7f960 | A per-thread direct-mapped cache of recent term lookups (2,048 entries, 32 KB) | 0.792 (0.781-0.799) | no |
| 0cde2f9 | Unchecked indexing where the index is in range by construction (masked table probe, per-document hash buffer, postings entry), each with its SAFETY argument | 1.015 (1.009-1.020) | yes |
| ccefbf8 | Skip the lookups of AND and NOT terms once a query's running result is an empty list | 0.999 (0.977-1.010) | no |
| ad4033a | The term lookup returns each token's BOOL index and keyed flag, so the postings loop does not reload the table entry | 1.007 (0.993-1.012) | no |
| 40f4c2b | First occurrences in a document by a 6.7 KB bitset of BOOL terms, chunk counts by a pass over the posting groups | 0.967 (0.962-0.975) | no |
| 23322c6 | Chunk index built and reset over the terms the chunk touched only (no per-chunk tables over all 53,000 terms) | 0.989 (0.975-1.015) on the runner's 30 chunks | yes: board 0.2429 s, 45.3 MB at 72 chunks |
| c2b0f70 | The plan keeps every BOOL query's and phrase's terms in one array each (no allocation per query); term table presized | 1.013 (1.002-1.034) | yes |

Tested on the board only, and reverted: six threads instead of twelve (0.342 s: SMT pays, the lookups are latency-bound); skipping a query in a chunk where a term every match needs is absent (0.2628 and 0.2491 s); reusing each thread's chunk lists and bitmaps (0.2475 s); cache-line-aligned shared summaries (0.2453 s); eight chunks per thread with the touched-terms index (0.2715 s: each chunk costs ~1 ms of query evaluation on the box, which the 2-vCPU runner cannot show).

Chunk count and the shared summaries could only be judged on the box (the 2-vCPU runner makes 30 chunks whatever the setting, and memory is the box's cgroup peak). Board runs, in order: per-thread summaries with 3, 5, 6, 8 and 12 chunks per thread gave M 91, 73, 67, 60 and 53 MB at T 0.239-0.274 s (eight best, K 21.38); packing count and sum into one word and sharing one summary per query across threads took eight per thread from 57.7 to 41.0 MB (K 22.16); with shared summaries, four, six and eight per thread gave 0.2400 s / 61.6 MB, 0.2435 s / 47.4 MB and 0.2667 s / 41.0 MB, so six (K 22.85).


An x86 profile (`.github/workflows/prof.yml`, perf by source line on the runner) guided the last three rows: ~10% of the run on the postings-entry miss, ~10% on the term-table probe, ~15% in `Vec` push and extend paths, ~5% in the hash finaliser.

## Board runs

Every step measured on the scoring box by the live board, in order. Rows marked FAILED EXPERIMENT were measured and reverted.

| Commit | Change | Run ID | T (s) | M (MB) | S | K |
|---|---|---|---|---|---|---|
| | The reference, unchanged | | 56.35 | 2200 | | 1.0 |
| aba13de | first Rust version | run-765bcea369cb | 0.7527 | 102.8 | 576 | 12.11 |
| 19cf611 | pair filter, bitmask tokenizer, THP opt-out | run-43435159d046 | 0.6185 | 103.6 | 666 | 13.05 |
| 0f8bf51 | (unchanged code, rerun) | run-71f33bbd0b52 | 0.5904 | 103.8 | 666 | 13.35 |
| 8b02279 | (unchanged code, rerun) | run-534919f14d4c | 0.5727 | 102.7 | 666 | 13.58 |
| 42a77bd | phrases checked only at their key term (least-used term) instead of every adjacent pair: no change on the box | run-f322bb3147f9 | 0.5884 | 102.0 | 676 | 13.39 |
| 92c4439 | huge pages back on (random access into term tables), one 16-byte info record per term, dense terms as per-chunk bitmaps, L2-resident term filter | run-406845fe2029 | 0.4062 | 103.5 | 824 | 15.60 |
| 5dde67a | query words straight into the token table (no HashMap): no measurable change | run-65e8b9df59ed | 0.4228 | 97.6 | 821 | 15.48 |
| b6426c7 | token lookups staged per document with slot and info prefetch | run-d56e39318972 | 0.3959 | 98.6 | 860 | 15.85 |
| dcc4444 | (unchanged code, rerun: shows ~10% board noise on B) | run-7a98cc7abed5 | 0.4355 | 98.2 | 860 | 15.13 |
| a16a1f9 | TIMING PROBE (fails the hash on purpose): exits after indexing (plan + corpus pass); T times everything before it | run-115531432605 | 0.3483 | 96.3 | - | - |
| 02def11 | TIMING PROBE 2 (fails the hash on purpose): exits after the plan (queries parsed, term table built) | run-c3fe1980b0d2 | 0.0255 | 13.0 | - | - |
| b122872 | per-thread term buffer reused across chunks and 16-bit: 99 -> 87 MB | run-afb372de08aa | 0.4144 | 87.1 | 864 | 15.87 |
| b842bd1 | four corpus chunks per thread so the last round is not half empty | run-56324b27483e | 0.3863 | 80.8 | 864 | 16.69 |
| c2a6be9 | one 32-byte slot per query term (hash, id, BOOL index, phrase keys): a token lookup is one cache miss and no key compare | run-b573b8a0720c | 0.3546 | 83.8 | 860 | 17.31 |
| fe37840 | TIMING PROBE (fails the hash on purpose): exits after the corpus pass, so queries take ~80 ms | run-79d6b34c3dc8 | 0.2718 | 83.8 | - | - |
| bd38c4c | newline search 32 bytes at a time, branch-free inlined token keys | run-1d8d70dc7964 | 0.3143 | 83.6 | 890 | 18.29 |
| 18cf889 | TIMING PROBE (fails the hash on purpose): corpus pass tokenizing only: postings and phrases cost ~100 ms | run-a80663e7b75a | 0.1684 | 37.8 | - | - |
| 7a08c2d | FAILED EXPERIMENT: bulk result summaries (popcounts): no change; reverted | run-65de97376cdd | 0.3194 | 81.6 | 915 | 18.16 |
| 9c83538 | no bit filter in front of the term table: same speed, so the filter is removed | run-ccdd0dba725c | 0.3127 | 84.6 | 886 | 18.31 |
| 87d019f | TIMING PROBE (fails the hash on purpose): tokenizing and posting collection only: phrases and counting sort cost ~95 ms | run-8fd21631ea78 | 0.1772 | 33.9 | - | - |
| cef5dc1 | TIMING PROBE (fails the hash on purpose): no counting sort: the sort costs ~84 ms | run-9fe8abd767c0 | 0.1883 | 34.8 | - | - |
| 6024f45 | postings collected by term range, scattered one range at a time; no term filter | run-718e56e8faac | 0.2914 | 99.6 | 866 | 18.42 |
| 4b83e56 | FAILED EXPERIMENT: eight chunks per thread: slower queries over twice the chunks; reverted | run-eb7d798fa4af | 0.3087 | 95.6 | 871 | 18.03 |
| 01fdee3 | noise check: docs-only commit, code unchanged from 4dd375f | run-f573abb0f149 | 0.3434 | 99.8 | 866 | 16.96 |
| 5042f52 | TIMING PROBE (fails the hash on purpose): exits after the corpus pass: 213 ms, so the merge and queries cost ~80-120 ms (full runs measure 0.29-0.34 s) | run-e94923ac235b | 0.2133 | 98.3 | - | - |
| 41f7f9f | 16-byte term slots, growth at 60%: table 8 -> 2 MB; M 99 -> 92.9. T inconclusive: the whole box ran ~15% slow this run (A unchanged at 0.0314 vs 0.0273) | run-29b9ce5806c3 | 0.3187 | 92.9 | 871 | 17.85 |
| 86e958d | queries evaluated chunk by chunk within each block of 256 (on top of the 16-byte slots): 0.2821, best so far | run-b8f1f8347870 | 0.2821 | 92.2 | 876 | 18.98 |
| 87155c8 | TIMING PROBE (fails the hash on purpose): tokenizing only (mask, hash, lookups) with the 2 MB table: 153.8 ms (168.4 with the 8 MB table in 18cf889) | run-492611e3eeca | 0.1538 | 31.6 | - | - |
| 7f10e38 | per-chunk bitset of dense terms (no binary search for absent terms): 0.2917, within noise of 0.2821 | run-654ed82480d7 | 0.2917 | 91.7 | 881 | 18.67 |
| 7a22f66 | 256 KB worker stacks: M unchanged (91.9), so thread stacks are not the ~24 MB; T 0.2787 (dense bitset in, second sample) | run-0e13ef120e87 | 0.2787 | 91.9 | 882 | 19.09 |
| bfafaeb | code unchanged from 7a22f66 (0.2787 there): ~15% board noise on B | run-49a1bdeea31b | 0.3227 | 91.1 | 882 | 17.77 |
| 1e03453 | one malloc arena (M_ARENA_MAX = 1): M 91.9 -> 88.2 | run-ae0c70a17d1f | 0.3008 | 88.2 | 892 | 18.49 |
| 8ce6ac2 | BOOL queries evaluated on each chunk while its index is in cache, index dropped, per-thread summaries merged: M 88.2 -> 80.7, T 0.2958 (same as recent samples) | run-463b8ed9da48 | 0.2958 | 80.7 | 907 | 18.93 |
| 165fb12 | last document and count side by side, chunk index buffers reused: T 0.3047, M 80.7 -> 85.5 (reused Vecs grow by doubling) | run-c11d1e528ebf | 0.3047 | 85.5 | 923 | 18.39 |
| 529b16e | tokens walked boundary by boundary, hit-first lookup (9327878): 0.2854; x86 A/B 1.018x, CI 1.012-1.025 | run-1561581721e1 | 0.2854 | 86.1 | 932 | 18.95 |
| ffbc9b1 | chunk index allocated per chunk again (side-by-side last document and count kept): M 86.1 -> 81.0, T 0.2838; x86 A/B vs reuse 1.001x (no difference) | run-467e344c1143 | 0.2838 | 81.0 | 920 | 19.28 |
| 09b5340 | TIMING PROBE (fails the hash on purpose): tokenizing only with the boundary walk: 148 ms (153.8 before); the full run is ~284, so postings, phrases, scatter and queries cost ~136 ms | run-093e92657a78 | 0.1480 | 41.8 | - | - |
| f6cc5dc | compact 1 MB term table (x86 A/B 1.017x) and bitmaps above 1/64 of a chunk (1.023x; 1/256 was 0.958x): 0.2735, M 83.6 | run-cf8c5529a0a8 | 0.2735 | 83.6 | 959 | 19.39 |
| ae1dbdb | results summarised in bulk once the ten smallest ids are known (popcounts for bitmap words; x86 A/B 1.074x): 0.2536, best so far | run-a92f27e7df26 | 0.2536 | 82.6 | 978 | 20.13 |
| 1a58518 | postings entry (u16 last offset, u16 count), x86 A/B 1.012x: T 0.2641 (board noise), M 82.6 -> 77.8 | run-776e4ea7b7a4 | 0.2641 | 77.8 | 978 | 19.96 |
| 5b22af8 | token mask walked as made (no mask buffer), hashes written by index (x86 A/B 1.066x): 0.2515, best so far | run-4008000a794b | 0.2515 | 80.5 | 975 | 20.32 |
| ecdc8d2 | TIMING PROBE (fails the hash on purpose): exits after the plan: 23.0 ms of serial start-up (~9% of the run) | run-595ce917a2d1 | 0.0230 | 10.5 | - | - |
| 35887e4 | code unchanged from 5b22af8 (resample): 0.2396 | run-a905e7520c5f | 0.2396 | 80.2 | 975 | 20.84 |
| 230409a | TIMING PROBE (fails the hash on purpose, pushed as fb8320f): exits after the parallel plan (0297d50): 19.9 ms against the serial plan's 23.0, +44 lines; the parallel plan dropped | run-4b61a35a1d87 | 0.0199 | 20.5 | - | - |
| f0410c0 | TIMING PROBE (fails the hash on purpose, pushed as a9dcca0): every chunk indexed, no query evaluated: 205 ms against ~240 for the full run, so the fused query evaluation costs ~35 ms | run-ce1a349b295c | 0.2051 | 80.3 | - | - |
| 1b38a46 | unchecked indexing where in range by construction (x86 A/B 1.015x): 0.2668, a slow board sample | run-2007d990b500 | 0.2668 | 79.4 | 976 | 19.78 |
| fec02e1 | EXPERIMENT: five chunks per thread: T 0.2446, M 73.2 (smaller per-thread buffers), K 21.00, best so far | run-215e302c1b8f | 0.2446 | 73.2 | 976 | 21.00 |
| fd68fcc | EXPERIMENT: six chunks per thread: T 0.2475, M 67.3, K 21.23 | run-5ee6705ae703 | 0.2475 | 67.3 | 976 | 21.23 |
| bea6fc6 | EXPERIMENT: eight chunks per thread: T 0.2550, M 60.4, K 21.38 | run-5f2b80fd3e88 | 0.2550 | 60.4 | 976 | 21.38 |
| 0a4806e | EXPERIMENT: twelve chunks per thread: T 0.2743, M 53.1, K 21.15: past the best (eight); back to eight | run-0a55e9be56d4 | 0.2743 | 53.1 | 976 | 21.15 |
| 0c26e3f | eight chunks per thread and the packed count/sum summary (pushed as 5c3f422): 0.2532, M 57.7, K 21.64, best so far | run-ff7cdeaf2606 | 0.2532 | 57.7 | 977 | 21.64 |
| 440c037 | one shared summary per BOOL query (atomic count/sum, ten smallest under a per-query lock) instead of one per thread: M 57.7 -> 41.0, T 0.2667, K 22.16, best so far | run-d4460d2736f9 | 0.2667 | 41.0 | 1007 | 22.16 |
| 13d1b99 | EXPERIMENT (pushed as d96fdfe): six chunks per thread with shared summaries: T 0.2435, M 47.4, K 22.85, best so far | run-246ec4cb0362 | 0.2435 | 47.4 | 1007 | 22.85 |
| 980f7e2 | EXPERIMENT: four chunks per thread with shared summaries: T 0.2400, M 61.6, K 21.84; back to six | run-2e066b2c521a | 0.2400 | 61.6 | 1007 | 21.84 |
| d9cdcc7 | six chunks per thread with shared summaries, final setting (second sample): 0.2450, M 48.6, K 22.67 | run-b6fb144e1767 | 0.2450 | 48.6 | 1007 | 22.67 |
| 16345d2 | TIMING PROBE (fails the hash on purpose): tokenizing only, current code: 141 ms of a ~244 ms run, so postings, scatter, queries and output are ~103 ms | run-9578547a6a23 | 0.1412 | 20.5 | - | - |
| 056f68d | chunk index over the touched terms only (no per-chunk tables over all 53,000 terms): T 0.2429, M 45.3, K 23.08, best so far (x86 A/B 0.989, no clear difference on the runner's 30 chunks) | run-f9140b400283 | 0.2429 | 45.3 | 1011 | 23.08 |
| 9983071 | EXPERIMENT: eight chunks per thread with the touched-terms index: T 0.2715, M 39.7, K 21.96: each chunk costs ~1 ms of query evaluation; back to six | run-f84dcbaab6f0 | 0.2715 | 39.7 | 1011 | 21.96 |
| 173ed27 | six chunks per thread, queries skipped where a required term is absent: 0.2628, M 45.1 (box slow this run? A 0.0280); resampled | run-c766d50d2fe6 | 0.2627 | 45.1 | 1025 | 22.17 |
| 932c26e | required-term skip, second sample: 0.2491 (0.2628 before; 0.2429 without it): no gain, reverted | run-d7648d6e074f | 0.2491 | 45.8 | 1025 | 22.69 |
| 2d30f22 | chunk lists and bitmaps reused per thread (no unmap per chunk): 0.2475, M 47.7: no gain, reverted | run-da2cdc79becd | 0.2475 | 47.7 | 1020 | 22.59 |
| 0c5d410 | EXPERIMENT: six threads (one per core): 0.3422, M 40.8: SMT pays (the lookups are latency-bound); back to twelve | run-17e3636ad8a6 | 0.3422 | 40.8 | 1011 | 19.55 |
| 99ef432 | EXPERIMENT: shared summaries aligned to one cache line each: 0.2453, M 45.2: within noise of 0.2429; reverted | run-8a587ccf5560 | 0.2453 | 45.2 | 1012 | 22.97 |
| 658ebc5 | TIMING PROBE (fails the hash on purpose): exits after parsing the queries and filling the term table: 18.1 ms of the plan's 23.0 | run-416c9b2f02ff | 0.0181 | 8.2 | - | - |
| c2b0f70 | plan stores BOOL and phrase terms in one array each, term table presized (x86 A/B 1.013x): 0.2392, M 43.4, K 23.42, best so far | run-95827e1ba6a8 | 0.2392 | 43.4 | 1003 | 23.42 |

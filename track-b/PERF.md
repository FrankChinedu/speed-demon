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

- 21.1M (term, doc) postings for 53,444 BOOL terms. 369 terms with df > N/32 hold half of them, so bitmaps for those are the next memory step.
- Tokenization is byte-exact ASCII: `A-Z` lowercased, runs of `a-z0-9`, every other byte (including all UTF-8 bytes) separates.

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

# Track B: Index: performance log

Measured locally with `tools/sd.py bench` (Docker on an Apple M1 Pro, 8 vCPUs, arm64, same sandbox flags as the board, data pre-warmed in a Docker volume), mean of the middle three of five runs. K uses the scoring-box reference numbers and the real S from `harness/static_axes.py`. Board run IDs are filled in from the live board once each step is pushed.

| Step | Change | Run ID | T (s) | M (MB) | K |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring box) | | 56.35 | 2,200 | 1.0 |
| 1 | Rust rewrite: index only the query terms, two tokenizer passes (count, then write one exact-size global arena), phrases matched inline by a (first, second) term pair table so no positions are stored, BOOL left to right on sorted lists | local | 1.694 | 128.7 | 7.7 |
| 2 | Failed: replaced `HashMap<&[u8], u32>` with a custom open-addressing table (32-byte padded keys, 32-bit hash tags). No measurable change, because the lookups were not the bottleneck. Kept, because it is no slower and has simpler hashing | local | ~1.69 | ~129 | 7.7 |
| 3 | A sampling profile showed the two tokenizer passes cost about the same, so the second pass was pure overhead. Now one pass: each chunk of about 10k documents builds its own posting lists by counting sort, and BOOL queries run per chunk (chunks are ascending, disjoint doc ranges, so results concatenate) | local | 0.805 | 196.5 | 10.3 |
| 4 | Memory: the 8-byte (term, doc) pairs held per chunk in flight became per-document term lists, and lists store 16-bit offsets from the chunk's first document | local | 0.719 | 91.8 | 12.7 |
| board | Same code as the step above, measured on the scoring box (commit aba13de, 12 threads, 2.5 GHz) | run-765bcea369cb | 0.753 | 102.8 | 12.11 |

## Notes

- 21.1M (term, doc) postings for 53,444 BOOL terms. 369 terms with df > N/32 hold half of them, so bitmaps for those are the next memory step.
- Tokenization is byte-exact ASCII: `A-Z` lowercased, runs of `a-z0-9`, every other byte (including all UTF-8 bytes) separates.

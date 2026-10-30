# Track C: Join: performance log

Measured locally with `tools/sd.py bench` (Docker on an Apple M1 Pro, 8 vCPUs, arm64, same sandbox flags as the board, data pre-warmed in a Docker volume), mean of the middle three of five runs. K uses the scoring-box reference numbers and the real S from `harness/static_axes.py`. Board run IDs are filled in from the live board once each step is pushed.

| Step | Change | Run ID | T (s) | M (MB) | K |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring box) | | 14.44 | 367 | 1.0 |
| 1 | Rust rewrite: parallel split by quote parity (a byte is inside quotes exactly when an odd number of `"` precede it), byte-level record parser on the map, per-thread aggregates; distinct names via per-thread `HashMap<Cow<[u8]>>`, per-thread (country, name) pairs, then a single-threaded global merge | (pending push) | 0.501 | 83.0 | 5.9 |
| 2 | Profile: interning about 80 ms, field scanning about 65 ms, single-threaded merge about 55 ms (10 threads, native). Raw field bytes identify a name value exactly (unquoted fields cannot hold `"`, and unescaping is one-to-one), so there is no unescaping for identity. One lock-free table shared by all threads hands out dense ids, and each country sets bits in a zero-initialised bitset that is never touched past the ids in use. No per-thread maps, no pairs, no merge | (pending push) | 0.272 | 25.7 | 10.1 |

## Notes

- Verified on seeds 1, 2 and 3 at tiny and sample scale, and on seed 1 at full, against the reference's output.
- `name_bytes` and `note_bytes` are raw lengths minus the number of `""` escapes, so no field is ever copied.

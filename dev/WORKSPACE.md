> Frank's original workspace notes. For how to continue, read [HANDOFF.md](HANDOFF.md).

# Speed Demon 2026 workspace

Rust, zero dependencies, all four tracks. Kickoff Oct 30 15:00 UTC, code freeze Nov 2 15:00 UTC.

## Layout

| path | what |
|---|---|
| `repo/` | the team repo (from the starter template), `speed-demon` branch |
| `starter/` | pristine clone of the starter template, for comparison |
| `harness/` | official specs, generators, references and scoring harness |
| `data/<t>/<scale>-seed1/` | generated inputs; `.expected/` next to each holds the reference output |
| `tools/sd.py` | workbench (below); `tools` is a symlink to `repo/dev/tools` |
| `tools/templates/rust/` | the Rust track template that `sd.py init` copies in |
| `tools/expected.json` | sha256 of the reference output for every track and scale |

Seed 1 `full` is exactly the live board's dataset.

## Workflow

```
python3 tools/sd.py init a --team "NAME"   # after kickoff only (guarded)
python3 tools/sd.py run a --scale tiny     # native build, fast loop, diff on mismatch
python3 tools/sd.py run a --scale full
python3 tools/sd.py bench a                # Docker sandbox: T, memory.peak, estimated K
python3 tools/sd.py check a --scale full   # the starter's own check.py
python3 tools/sd.py k a --t 1.2 --m-mb 40 --s 300
```

Solutions take `[data_dir] [out_dir]` as optional args (default `/data`, `/out`), so the same binary runs natively and on the board.

Real S: `docker build -t speeddemon/static-tools:2026-10-28 harness/harness/tools` once, then `python3 harness/harness/static_axes.py --track-dir repo/track-a`.

## Reference numbers (scoring box)

| track | T_ref | M_ref | S_ref | timeout | board cap | 50x memory target |
|---|---|---|---|---|---|---|
| A | 32.05 s | 228 MB | 56 | 321 s | 96 s | 4.6 MB |
| B | 56.35 s | 2.20 GB | 91 | 564 s | 169 s | 44 MB |
| C | 14.44 s | 367 MB | 108 | 144 s | 43 s | 7.3 MB |
| D | 72.69 s | 893 MB | 43 | 727 s | 218 s | 18 MB |

## Rules that bite

- **Nothing in `track-*` before kickoff.** Tools outside the track folders are fine.
- Only the `speed-demon` branch runs; one board run per team about every 5 min (a newer push replaces a queued one); cite run IDs in each `PERF.md`.
- Every dependency is about -10% K. Write the hasher, mmap shim and thread pool yourself.
- M is cgroup `memory.peak`: heap and `/tmp` count, the pre-warmed `/data` page cache does not. `mmap` the input instead of reading it into a buffer.
- 12 threads on 6 P-cores, turbo off (2.5 GHz). No io_uring.
- Output must hash identical on all 5 runs: no nondeterministic ordering.
- Repo public under OSI licence within 1 hour of freeze; keep the Board app installed.
- Local Docker here is arm64; the box is x86-64 (AVX2, no AVX-512). `.cargo/config.toml` sets `x86-64-v3` for x86 only.

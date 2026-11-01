# Handoff: how to keep improving our score

Team **Deez-attacher**, Speed Demon 2026. Code freeze: **Nov 2, 15:00 UTC**. Whatever is on the `speed-demon` branch at that moment is what gets scored.

The work is split in two halves with no files in common, so two people can push all day without conflicts:

| | Tracks | Why they go together |
|---|---|---|
| **Half 1** | **A** (log ingest) and **C** (CSV join) | Both are "parse a big text file fast, then group by a hashed string key". Same tricks, same bottlenecks. |
| **Half 2** | **B** (text index) and **D** (shortest paths) | Both build an in-memory index, then answer thousands of queries in parallel. |

Each person only edits their own `track-*` folders. Everything outside them (`dev/`, the top-level README) is shared: pull before you edit it.

## 1. How scoring works (read this first)

- Per track: `K = (Tref/T)^0.5 · min(Mref/M, 50)^0.2 · min(Sref/S, 1)^0.15 · (1/D)^0.15`, then points = `50 · log10 K`. Time matters most, memory next, lines of code (S) a little, dependencies (D) are about -10% K each (we have none: keep it that way).
- Track total = 65% machine points + 35% judges (they read the code, README.md and PERF.md). Overall = geometric mean of our **best three** track totals.
- **The live board is practice.** The final score is a fresh build of the freeze commit, run 5 times on a holdout seed we have never seen. T is the mean of the middle three runs, and **every one of the 5 runs must produce the right hash**. A fast wrong answer is worth zero, so never trade correctness for speed, and never rely on anything that is true of seed 1 but not guaranteed by the spec.
- The board box: Intel i5-13500, 6 P-cores / 12 threads, 2.5 GHz with turbo off, AVX2 (no AVX-512), 1.25 MB L2 per core, 24 MB L3. Board times are noisy by about ±5%. Do not keep a change for a gain smaller than that unless it also cuts lines.

## 2. Setup (about 15 minutes)

Our tools expect this layout:

```
speed-demon/            any folder name
├── repo/               this repository, branch speed-demon
├── harness/            the official harness (specs, generators, references)
├── tools -> repo/dev/tools   a symlink
└── data/               generated inputs (gitignored, several GB)
```

```sh
mkdir speed-demon && cd speed-demon
git clone -b speed-demon git@github.com:FrankChinedu/speed-demon.git repo
git clone https://github.com/hackathon-raptors/speed-demon-2026 harness
ln -s repo/dev/tools tools
# Your half only: Half 1 uses "a c", Half 2 uses "b d".
python3 tools/sd.py gen --tracks a c                                          # seed 1 (the board's data)
SD_SEED=2 python3 tools/sd.py gen --tracks a c                                # seed 2 catches overfitting
SD_SEED=3 python3 tools/sd.py gen --tracks a c --scales tiny sample
python3 tools/gate.py install            # pre-push hook: blocks pushes with a wrong hash
```

Any current stable Rust works locally (the Docker build uses `rust:1-slim-trixie`, rustc 1.99.1). You need Docker running (generation happens in a no-network `python:3.13-slim` container). `full` data is a few hundred MB per track.

If you use Claude Code: the repo ships a skill, `.claude/skills/rust-perf/SKILL.md`, with our performance rules. Open the workspace folder and ask it to "use the rust-perf skill".

## 3. The loop

```sh
cd repo/track-a && cargo build --release && cd ../..
python3 tools/verify.py a        # every local dataset vs tools/expected.json: must be all PASS
# commit, then:
git -C repo pull --rebase && git -C repo push origin HEAD:speed-demon
python3 tools/board.py           # our latest and best per track, and the top 3
```

- **Every push runs all four tracks.** The board takes one commit about every 5 minutes, and a newer push replaces a queued one. So always `pull --rebase` before pushing: your commit then carries your partner's latest work too, and both halves get measured in the same run.
- `board.py` appends each new scored run to `dev/tools/runs.tsv`. Write a short note in the last column for every run that changed something (failed experiments too). `python3 tools/perf_md.py` then regenerates the "Board runs" table in every `PERF.md`. The judges read these tables.
- **x86 paths must be tested on x86.** The box is x86-64, but our Macs are arm64, so `#[cfg(target_feature = "avx2")]` code never runs locally. Before pushing SIMD code, run it in an amd64 container:

  ```sh
  docker run --rm --platform linux/amd64 -v "$PWD/repo/track-a":/src:ro -v "$PWD/data/a":/d:ro \
    rust:1-slim-trixie sh -c 'cp -r /src /w && cd /w && cargo build --release -q &&
    for s in tiny-seed1 sample-seed1 sample-seed2; do mkdir -p /o/$s;
      ./target/release/solution /d/$s /o/$s && sha256sum /o/$s/summary.txt; done'
  ```

  Compare against `tools/expected.json`. It is emulated, so ignore the timing.
- **Local timing lies.** An M-series Mac has 12 MB of L2 per cluster and a different core design. Several changes that were big local wins did nothing on the box, and one cache problem that cost a third of track B's time was invisible locally. Trust only the board.

### Timing probes: the most useful trick we have

The board reports T and M even when the hash is wrong. To find out how long a phase takes on the real box, commit a version that writes an empty output file and exits right after that phase:

```rust
std::fs::write(out.join("summary.txt"), b"").unwrap(); // TIMING PROBE
std::process::exit(0);
```

Start the commit subject with `TIMING PROBE` (the pre-push gate skips those), log the run in `runs.tsv` as a probe, then revert. Every number in the "where the time goes" lines below came from probes like this.

### Profiling

`dev/perf/` builds a Linux image with `perf`. Use it for a rough line-level profile (on arm64, so it is only a hint):

```sh
docker build -t sd-perf repo/dev/perf
docker run --rm --privileged -v "$PWD/repo/track-b":/src -v "$PWD/repo/dev/perf/prof.sh":/prof.sh \
  -v "$PWD/data/b/full-seed1":/data0:ro sd-perf sh -c 'cp -r /data0 /data; mkdir /out; /prof.sh /data /out'
```

## 4. Half 1: tracks A and C

### Track A (ingest): our K 32.97, leader K 56.9 (T 0.016 s, 17 MB)

Our best: T 0.039 s, M 22.8 MB, S 475 (commit 4dd375f). The design is in `track-a/README.md`.

Where the time goes (probes, see `track-a/PERF.md`):
- Parsing is about 31 ms of the 39. Only reading the 239 MB file once costs about 10 ms.
- About 10 ms of the parse is the per-line endpoint lookup (`Endpoints::id`), even though the table is a 64 KB thread-local array that should be in cache. Nobody knows why yet. **This is the best lead.** Ideas: records carry a hash-derived key and get mapped to ids in phase 2 (once per hour, in cache), or find what about the lookup is slow (a mispredicted probe loop? the inlined registry path?).
- Phase 2 (sort, percentiles, output) is about 8 ms.

Tried and **not** worth it (details in PERF.md): an AVX2 bitmask parser, a branch-free 64-byte endpoint scan (2% faster, +71 lines), two line streams per thread, `MADV_POPULATE_READ`, thread-local ids without the registry, a 32 KB one-word table.

### Track C (CSV join): our K 25.48, leader K 47.5 (T 0.018 s, 13 MB)

Our best: T 0.057 s, M 11.2 MB, S 493 (commit 6024f45). The design is in `track-c/README.md`.

Where the time goes:
- Without name interning and the "seen" bitset the run takes 36 ms, so those cost about 21 ms. Only reading the file is about 8 ms.
- The leader's whole run (18 ms) is faster than our parse alone. Something structural is different.

Tried and not worth it: a simdcsv-style structural parser (no faster), owner-partitioned bitset rows with no atomics (2 ms faster, more memory and lines), a write prefetch for the bitset.

Ideas: count distinct (country, name) pairs without the 4 MB shared bitset; overlap more cache misses per batch; check whether the per-record join (`region` table, 2 MB) misses cache.

## 5. Half 2: tracks B and D

### Track B (text index): our K 18.42, leader K 41.8 (T 0.074 s, 47 MB)

Our best: T 0.291 s, M 99.6 MB, S 866 (commit 6024f45). The design is in `track-b/README.md`. This is our furthest-behind track: a 4x gap in time.

Where the time goes (before the last change, 272 ms corpus pass plus about 40 ms of queries):
- Tokenizing 36 M tokens, with the hash and term-table lookup for each, is about 168 ms. **This is the biggest single cost.**
- Collecting postings is about 9 ms, phrase matching about 11 ms.
- The per-chunk posting scatter was about 84 ms. The term-range grouping in 6024f45 cut it, but some of it is still there.
- M is 99.6 MB against the leader's 47. The posting buffers and the per-chunk offset tables over all 53,000 BOOL terms are the obvious places to look.

Ideas: a cheaper hash for tokens of 8 bytes or less; skip hashing tokens that cannot be query terms (by length, or a first-bytes filter); a compact per-chunk offset table. Note that a 128 KB bloom filter in front of the term table saved nothing on the box, because most tokens *are* query terms.

Tried and not worth it: bulk result summaries (popcounts), eight chunks per thread (the query phase slows down with more chunks).

### Track D (shortest paths): our K 19.00, leader K 29.3 (T 0.121 s, 65 MB)

Our best: T 0.191 s, M 159.4 MB, S 507 (commit 5892567). The design is in `track-d/README.md`.

Where the time goes: the build (parse, rows) is about 64 ms, and the 2,000 queries about 130 ms. Node ids are a random permutation, so every settled node is a cache miss.

- **Memory is a free win if you can find it.** 159 MB against 65 MB is worth about 1.2x in K by itself. The peak is two 32-byte row tables (111 MB) plus the build's packed edge arrays (80 MB) overlapping. Returning edge memory with `madvise` during the build did **not** lower the box's peak, at either 4 KB or 2 MB granularity: transparent huge pages stay charged. A smaller layout is needed, for example a CSR with 6-byte edges, or building one direction's rows without materializing its edge array.
- For query time: goal-directed search (ALT landmarks) would cut settled nodes, if the preprocessing fits the budget. The leaders' T of about 0.12 s suggests they do something like it.

Tried and not worth it: a per-side seen bitset, a CSR layout (slower locally, never pushed), lazy row faults, a parallel pre-touch, THP off.

## 6. Rules that bite

- **No dependencies.** Every crate costs about 10% of K. We declare `mmap`, `madvise` and `prctl` against libc by hand (`src/mmap.rs`).
- **`unsafe` only with a `SAFETY:` comment** saying why it is sound. The judges read the code.
- **Output must be deterministic** across the 5 runs: no `HashMap` iteration order, no thread-order-dependent output.
- **S counts code lines after rustfmt, inside each track folder** (the Dockerfile too, but not .md or .toml files). Deleting dead code is a real, if small, win. Everything in `dev/` is free.
- **M is the cgroup's memory.peak**: heap counts; the input file's page cache does not, which is why we `mmap` the input. Memory a thread allocates but never touches is free.
- Hash identity (comparing 64-bit hashes instead of bytes) is used in A, B and C. Each file says why the collision odds are acceptable. If you change a hash, keep it at least that strong.

## 7. Freeze checklist (Nov 2)

1. **By 14:00 UTC:** stop experimenting. Each half makes sure their tracks are at their best *measured* version, with no probes left in.
2. Run `python3 tools/verify.py <t>` for all four tracks, and the amd64 check for any SIMD changed in the last day.
3. Regenerate the docs: `python3 tools/perf_md.py`, and make sure each README describes the current code.
4. **By 14:30 UTC:** the final commit is pushed, and `board.py` shows it scored OK on all four tracks.
5. **After 15:00 UTC, within one hour:** Frank makes the repository public (`gh repo edit FrankChinedu/speed-demon --visibility public --accept-visibility-change-consequences`). Keep the Speed Demon Board GitHub app installed.

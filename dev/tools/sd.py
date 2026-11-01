#!/usr/bin/env python3
"""Speed Demon workbench. Standard library only.

    sd.py gen                         generate seed-1 data (tiny, sample, full) in a container
    sd.py refs                        run the references, save expected outputs and hashes
    sd.py init a --team NAME          put the Rust template into repo/track-a (after kickoff)
    sd.py run a [--scale sample]      native cargo build + run, hash check, diff on mismatch
    sd.py bench a [--scale full]      docker build + sandboxed run, time and memory.peak, est. K
    sd.py check a [--scale sample]    the starter's check.py, the board's own local check
    sd.py k a --t 1.2 --m-mb 40 --s 300 [--d 1]

Data lives in data/<track>/<scale>-seed1, expected outputs next to it in
data/<track>/<scale>-seed1.expected/. The team repo is ./repo (override with SD_REPO).
"""
import argparse
import datetime
import hashlib
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA = os.path.join(ROOT, "data")
HARNESS = os.path.join(ROOT, "harness")
TEMPLATE = os.path.join(ROOT, "tools", "templates", "rust")
EXPECTED_JSON = os.path.join(ROOT, "tools", "expected.json")
REPO = os.environ.get("SD_REPO", os.path.join(ROOT, "repo"))
BUILD = os.environ.get("SD_BUILD", os.path.join(ROOT, ".build"))  # gate.py builds apart
SEED = int(os.environ.get("SD_SEED", "1"))  # SD_SEED=7 sd.py gen/refs/run ... for unseen seeds

DEBIAN_IMAGE = "debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad"
PY_IMAGE ="python:3.13-slim@sha256:d98cc29d3266e535dd9da1099688d9cb3ca4753813571e071763d17725460962"
KICKOFF = datetime.datetime(2026, 10, 30, 15, 0, tzinfo=datetime.timezone.utc)
TRACKS = {"a": "a_ingest", "b": "b_index", "c": "c_join", "d": "d_traverse"}
OUTPUT = {"a": "summary.txt", "b": "results.txt", "c": "aggregates.txt", "d": "distances.txt"}
SCALES = ("tiny", "sample", "full")

# Scoring-box reference numbers (harness/harness/calibration-seed1.json, reference_axes.json).
REF = {
    "a": {"T": 32.047276, "M": 227983360, "S": 56},
    "b": {"T": 56.352376, "M": 2199961600, "S": 91},
    "c": {"T": 14.444059, "M": 367435776, "S": 108},
    "d": {"T": 72.692655, "M": 892915712, "S": 43},
}
# Published seed-1 hashes, used to cross-check our own reference runs.
PUBLISHED = {
    ("a", "full"): "495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3",
    ("a", "sample"): "ca6311bbeb536fa55d85265bf6fd3d24cc9586f13de1ac73bd2a23ac3893aac3",
    ("a", "tiny"): "c24fa507c963eaeac9224fd8435019b4f4e8c002ece7e32f316681a3957eb92f",
    ("b", "full"): "f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927",
    ("b", "sample"): "eeeec1e9319da98a6f3c664c78f93e7078fd603c25c2bb3027174e81e3638a8f",
    ("c", "full"): "d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536",
    ("c", "sample"): "9035fed36823232dd41ae2d1b9d6d21aa7c73f49d8e95647e953a89d5ebe9a66",
    ("d", "full"): "a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e",
    ("d", "sample"): "a4c76ed030bc7e365db33191f6cf74708fba7f25eb48d64c8c4279f2e57046a0",
}
SANDBOX = ["--network=none", "--read-only", "--tmpfs", "/tmp:rw,size=1g,exec",
           "--memory=8g", "--memory-swap=8g", "--pids-limit=512", "--cap-drop=ALL",
           "--security-opt=no-new-privileges", "--user", "65534:65534",
           "--ulimit", "nofile=8192:8192"]


def die(msg):
    print(f"FAIL  {msg}")
    sys.exit(1)


def sh(cmd, **kw):
    print("$ " + " ".join(cmd))
    return subprocess.run(cmd, **kw)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def data_dir(track, scale):
    d = os.path.join(DATA, track, f"{scale}-seed{SEED}")
    if not os.path.isdir(d):
        die(f"{d} is missing; run `sd.py gen` first")
    return d


def expected_hash(track, scale):
    if os.path.exists(EXPECTED_JSON):
        with open(EXPECTED_JSON) as f:
            h = json.load(f).get(track, {}).get(scale if SEED == 1 else f"{scale}-seed{SEED}")
        if h:
            return h
    if SEED == 1 and (track, scale) in PUBLISHED:
        return PUBLISHED[(track, scale)]
    die(f"no expected hash for {track}/{scale}; run `sd.py refs`")


def track_dir(track):
    d = os.path.join(REPO, f"track-{track}")
    if not os.path.isdir(d):
        die(f"{d} is missing (set SD_REPO or create ./repo)")
    return d


def compare(track, scale, out_file):
    """Hash check; on mismatch show the first differing lines against the reference output."""
    want = expected_hash(track, scale)
    got = sha256(out_file)
    if got == want:
        print(f"PASS  {track}/{scale} sha256 {got}")
        return True
    print(f"FAIL  {track}/{scale} sha256 {got}, expected {want}")
    ref = os.path.join(DATA, track, f"{scale}-seed{SEED}.expected", OUTPUT[track])
    if os.path.exists(ref):
        subprocess.run(["sh", "-c", f"diff '{ref}' '{out_file}' | head -20"])
        print(f"      full diff: diff {ref} {out_file}")
    return False


def k_report(track, t, m, s, d):
    ref = REF[track]
    k = ((ref["T"] / t) ** 0.50 * min(ref["M"] / m, 50.0) ** 0.20
         * min(ref["S"] / s, 1.0) ** 0.15 * (1.0 / d) ** 0.15) if s > 0 else None
    pts = min(100.0, max(0.0, 50 * math.log10(k))) if k else 0.0
    print(f"K     {k:.2f}  points {pts:.1f}/100  (T {ref['T'] / t:.1f}x, M {ref['M'] / m:.1f}x, "
          f"S {s} vs {ref['S']}, D {d})")


def approx_lines(folder):
    """Rough S: non-blank, non-comment lines of .rs files and the Dockerfile. The real
    number comes from harness/harness/static_axes.py (rustfmt + scc)."""
    n = 0
    for base, dirs, files in os.walk(folder):
        dirs[:] = [x for x in dirs if x not in ("target", ".git")]
        for name in files:
            if name.endswith(".rs") or name == "Dockerfile" or name.endswith(".sh"):
                with open(os.path.join(base, name), errors="replace") as f:
                    for line in f:
                        s = line.strip()
                        if s and not s.startswith(("//", "#")):
                            n += 1
    return n


def cmd_gen(args):
    os.makedirs(DATA, exist_ok=True)
    for track in args.tracks:
        for scale in args.scales:
            out = f"data/{track}/{scale}-seed{SEED}"
            sh(["docker", "run", "--rm", "--network=none", "-v", f"{ROOT}:/w", "-w", "/w", PY_IMAGE,
                "python3", f"harness/generators/{TRACKS[track]}.py", "--seed", str(SEED),
                "--scale", scale, "--out", out], check=True)


def cmd_refs(args):
    hashes = {}
    if os.path.exists(EXPECTED_JSON):
        with open(EXPECTED_JSON) as f:
            hashes = json.load(f)
    for track in args.tracks:
        for scale in args.scales:
            data_dir(track, scale)
            out = f"data/{track}/{scale}-seed{SEED}.expected"
            os.makedirs(os.path.join(ROOT, out), exist_ok=True)
            t0 = time.monotonic()
            sh(["docker", "run", "--rm", "--network=none", "-e", f"TRACK={track}", "-v", f"{ROOT}:/w",
                "-w", "/w", PY_IMAGE, "python3", f"harness/reference/{TRACKS[track]}.py",
                "--data", f"data/{track}/{scale}-seed{SEED}", "--out", out], check=True)
            took = time.monotonic() - t0
            h = sha256(os.path.join(ROOT, out, OUTPUT[track]))
            pub = PUBLISHED.get((track, scale)) if SEED == 1 else None
            note = "" if pub is None else ("  matches published" if pub == h else f"  MISMATCH published {pub}")
            print(f"ref   {track}/{scale} {took:.1f} s sha256 {h}{note}")
            hashes.setdefault(track, {})[scale if SEED == 1 else f"{scale}-seed{SEED}"] = h
            with open(EXPECTED_JSON, "w") as f:
                json.dump(hashes, f, indent=2, sort_keys=True)
                f.write("\n")


def cmd_init(args):
    now = datetime.datetime.now(datetime.timezone.utc)
    if now < KICKOFF and not args.force:
        die(f"it is {now:%Y-%m-%d %H:%M} UTC; solution code in a track folder before kickoff "
            f"({KICKOFF:%b %d %H:%M} UTC) disqualifies the submission")
    dest = os.path.join(REPO, f"track-{args.track}")
    if not os.path.isdir(REPO):
        die(f"{REPO} is missing")
    keep = {"PERF.md", "README.md", "LICENSE"}
    if os.path.isdir(dest):
        for name in os.listdir(dest):
            if name not in keep:
                p = os.path.join(dest, name)
                shutil.rmtree(p) if os.path.isdir(p) else os.remove(p)
    shutil.copytree(TEMPLATE, dest, dirs_exist_ok=True)
    toml = os.path.join(dest, ".speed-demon.toml")
    with open(toml) as f:
        text = f.read().replace("TRACK", args.track).replace("TEAM", args.team)
    with open(toml, "w") as f:
        f.write(text)
    print(f"ok    {dest} is a Rust crate now (PERF.md and README.md kept)")


def cmd_run(args):
    folder = track_dir(args.track)
    data = data_dir(args.track, args.scale)
    target = os.path.join(BUILD, f"track-{args.track}")
    env = dict(os.environ, CARGO_TARGET_DIR=target)
    if args.native:
        env["RUSTFLAGS"] = (env.get("RUSTFLAGS", "") + " -C target-cpu=native").strip()
    if sh(["cargo", "build", "--release", "--manifest-path", os.path.join(folder, "Cargo.toml")],
          env=env).returncode:
        die("cargo build failed")
    binary = os.path.join(target, "release", "solution")
    out = tempfile.mkdtemp(prefix=f"sd-{args.track}-")
    times, rss = [], 0
    for i in range(args.runs):
        for name in os.listdir(out):
            os.remove(os.path.join(out, name))
        t0 = time.monotonic()
        p = subprocess.run(["/usr/bin/time", "-l", binary, data, out], stderr=subprocess.PIPE, text=True)
        times.append(time.monotonic() - t0)
        m = re.search(r"(\d+)\s+maximum resident set size", p.stderr)
        rss = max(rss, int(m.group(1)) if m else 0)
        if p.returncode:
            sys.stderr.write(p.stderr)
            die(f"exit {p.returncode}")
    found = os.listdir(out)
    if found != [OUTPUT[args.track]]:
        die(f"out dir must hold exactly {OUTPUT[args.track]}, found {found}")
    times.sort()
    print(f"time  median {times[len(times) // 2]:.3f} s over {args.runs} (min {times[0]:.3f})  "
          f"max RSS {rss / 1e6:.1f} MB  [native macOS, not the board]")
    ok = compare(args.track, args.scale, os.path.join(out, OUTPUT[args.track]))
    sys.exit(0 if ok else 1)


def cmd_bench(args):
    folder = track_dir(args.track)
    data = data_dir(args.track, args.scale)
    tag = f"speeddemon-bench/track-{args.track}"
    if sh(["docker", "build", "--network=none", "-q", "-t", tag, folder]).returncode:
        die("docker build failed (it must work offline)")
    cfg = json.loads(subprocess.run(["docker", "image", "inspect", tag], capture_output=True,
                                    text=True, check=True).stdout)[0]["Config"]
    entry = (cfg.get("Entrypoint") or []) + (cfg.get("Cmd") or [])
    # Time exec to exit and read the container's own cgroup v2 memory.peak afterwards.
    wrapper = ('s=$(date +%s%N); "$@"; rc=$?; e=$(date +%s%N); '
               'echo "SDBENCH $rc $((e-s)) $(cat /sys/fs/cgroup/memory.peak 2>/dev/null || echo 0)" >&2; exit $rc')
    # The box pre-warms /data on the host, so the input's page cache is never charged to
    # the run. A macOS bind mount charges mapped file pages to whoever touches them first,
    # so copy the data into a Docker volume and warm it from a throwaway container.
    volume = f"sd-{args.track}-{args.scale}-seed{SEED}"
    if subprocess.run(["docker", "volume", "inspect", volume], capture_output=True).returncode:
        sh(["docker", "volume", "create", volume], check=True, capture_output=True)
        sh(["docker", "run", "--rm", "-v", f"{data}:/src:ro", "-v", f"{volume}:/dst", DEBIAN_IMAGE,
            "sh", "-c", "cp /src/* /dst/"], check=True)
    sh(["docker", "run", "--rm", "-v", f"{volume}:/d:ro", DEBIAN_IMAGE, "sh", "-c", "cat /d/* > /dev/null"],
       check=True)
    out = os.path.join(ROOT, ".out", f"bench-{args.track}")
    walls, peaks = [], []
    for i in range(args.runs):
        shutil.rmtree(out, ignore_errors=True)
        os.makedirs(out)
        os.chmod(out, 0o777)
        p = subprocess.run(["docker", "run", "--rm", *SANDBOX, "-e", f"TRACK={args.track}",
                            "-v", f"{volume}:/data:ro", "-v", f"{out}:/out:rw", "--entrypoint", "sh",
                            tag, "-c", wrapper, "sh", *entry], stderr=subprocess.PIPE, text=True)
        m = re.search(r"SDBENCH (\d+) (\d+) (\d+)", p.stderr)
        if not m or m.group(1) != "0":
            sys.stderr.write(p.stderr[-4000:])
            die(f"run {i + 1} failed")
        walls.append(int(m.group(2)) / 1e9)
        peaks.append(int(m.group(3)))
        print(f"run   {i + 1}: {walls[-1]:.3f} s, memory.peak {peaks[-1] / 1e6:.1f} MB")
    found = os.listdir(out)
    if found != [OUTPUT[args.track]]:
        die(f"/out must hold exactly {OUTPUT[args.track]}, found {found}")
    ok = compare(args.track, args.scale, os.path.join(out, OUTPUT[args.track]))
    mid = sorted(walls)[1:-1] if len(walls) >= 3 else walls
    t = sum(mid) / len(mid)
    m_peak = max(peaks)  # the harness takes the highest of the five peaks
    print(f"T     {t:.3f} s (mean of middle runs)  M {m_peak / 1e6:.1f} MB  [local Docker, not the board]")
    if not ok:
        print("K     0 (a failed gate scores zero on the track)")
    elif args.scale == "full":
        k_report(args.track, t, m_peak, args.s or approx_lines(folder), args.d)
        if not args.s:
            print("      S is approximate; pass --s from harness static_axes.py for the real count")
    sys.exit(0 if ok else 1)


def cmd_check(args):
    data = data_dir(args.track, args.scale)
    check = os.path.join(REPO, "check.py")
    if not os.path.exists(check):
        die(f"{check} is missing")
    rc = sh([sys.executable, check, args.track, "--data", data,
             "--expected", expected_hash(args.track, args.scale)], cwd=REPO).returncode
    sys.exit(rc)


def cmd_k(args):
    k_report(args.track, args.t, args.m_mb * 1e6, args.s, args.d)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    def tracks_scales(p, scales=SCALES):
        p.add_argument("--tracks", nargs="+", choices=sorted(TRACKS), default=sorted(TRACKS))
        p.add_argument("--scales", nargs="+", choices=SCALES, default=list(scales))

    tracks_scales(sub.add_parser("gen"))
    tracks_scales(sub.add_parser("refs"))
    p = sub.add_parser("init")
    p.add_argument("track", choices=sorted(TRACKS))
    p.add_argument("--team", required=True)
    p.add_argument("--force", action="store_true", help="skip the kickoff guard")
    p = sub.add_parser("run")
    p.add_argument("track", choices=sorted(TRACKS))
    p.add_argument("--scale", choices=SCALES, default="sample")
    p.add_argument("--runs", type=int, default=3)
    p.add_argument("--native", action="store_true", help="add -C target-cpu=native for local speed")
    p = sub.add_parser("bench")
    p.add_argument("track", choices=sorted(TRACKS))
    p.add_argument("--scale", choices=SCALES, default="full")
    p.add_argument("--runs", type=int, default=3)
    p.add_argument("--s", type=int, help="real S from static_axes.py")
    p.add_argument("--d", type=int, default=1)
    p = sub.add_parser("check")
    p.add_argument("track", choices=sorted(TRACKS))
    p.add_argument("--scale", choices=SCALES, default="sample")
    p = sub.add_parser("k")
    p.add_argument("track", choices=sorted(TRACKS))
    p.add_argument("--t", type=float, required=True, help="wall time, seconds")
    p.add_argument("--m-mb", type=float, required=True, help="peak memory, MB")
    p.add_argument("--s", type=int, required=True, help="code lines")
    p.add_argument("--d", type=int, default=1, help="1 + direct dependencies")
    args = ap.parse_args()
    {"gen": cmd_gen, "refs": cmd_refs, "init": cmd_init, "run": cmd_run, "bench": cmd_bench,
     "check": cmd_check, "k": cmd_k}[args.cmd](args)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""A/B timing of two commits of one track on an x86-64 Linux machine (the GitHub
Actions runner of .github/workflows/ab.yml), where AVX2 paths run as on the box.

    python3 dev/tools/ab.py <track> <base ref> <head ref> [runs] [harness dir]

Generates the full seed-1 data, builds both commits' track-<t>, checks each output's
sha256 against tools/expected.json, then runs the two binaries alternately (ABBA
order, so drift hits both equally) and reports medians and a bootstrap 95%
confidence interval of the speedup. Standard library only.
"""
import hashlib, json, os, random, shutil, statistics, subprocess, sys, time

t, base, head = sys.argv[1], sys.argv[2], sys.argv[3]
runs = int(sys.argv[4]) if len(sys.argv) > 4 else 15
harness = sys.argv[5] if len(sys.argv) > 5 else "/tmp/harness"
repo = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True).stdout.strip()
gen = {"a": "a_ingest.py", "b": "b_index.py", "c": "c_join.py", "d": "d_traverse.py"}[t]
out_name = {"a": "summary.txt", "b": "results.txt", "c": "aggregates.txt", "d": "distances.txt"}[t]
data, work = "/tmp/ab-data", "/tmp/ab-work"


def sh(*cmd, **kw):
    print("$", " ".join(cmd), flush=True)
    return subprocess.run(cmd, check=True, **kw)


if not os.path.isdir(data):
    sh(sys.executable, os.path.join(harness, "generators", gen), "--seed", "1", "--scale", "full", "--out", data)
for f in os.listdir(data):  # warm the page cache, as the board does
    with open(os.path.join(data, f), "rb") as fh:
        while fh.read(1 << 24):
            pass

want = json.load(open(os.path.join(repo, "dev", "tools", "expected.json")))[t]["full"]
bins = {}
for side, ref in (("base", base), ("head", head)):
    tree = os.path.join(work, side)
    shutil.rmtree(tree, True)
    sh("git", "-C", repo, "worktree", "add", "-f", "--detach", tree, ref)
    sh("cargo", "build", "--release", "-q", cwd=os.path.join(tree, f"track-{t}"))
    bins[side] = os.path.join(tree, f"track-{t}", "target", "release", "solution")


def run(side):
    out = os.path.join(work, f"out-{side}")
    shutil.rmtree(out, True)
    os.makedirs(out)
    t0 = time.perf_counter()
    subprocess.run([bins[side], data, out], check=True)
    dt = time.perf_counter() - t0
    got = hashlib.sha256(open(os.path.join(out, out_name), "rb").read()).hexdigest()
    return dt, got == want


times = {"base": [], "head": []}
ok = {"base": True, "head": True}
for side in ("base", "head"):  # one untimed warm-up each
    ok[side] &= run(side)[1]
for i in range(runs):
    for side in (("base", "head") if i % 2 == 0 else ("head", "base")):
        dt, good = run(side)
        times[side].append(dt)
        ok[side] &= good

mb, mh = statistics.median(times["base"]), statistics.median(times["head"])
rng = random.Random(1)
boot = sorted(
    statistics.median(rng.choices(times["base"], k=runs)) / statistics.median(rng.choices(times["head"], k=runs))
    for _ in range(4000)
)
lo, hi = boot[100], boot[3899]
cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), "?")
verdict = "head FASTER" if lo > 1 else "head SLOWER" if hi < 1 else "no clear difference"
lines = [
    f"### A/B track {t}: base {base} vs head {head}",
    f"{runs} runs each, alternating; {os.cpu_count()} CPUs, {cpu}",
    "",
    "| | median (ms) | min (ms) | output hash |",
    "|---|---|---|---|",
    f"| base | {mb * 1e3:.2f} | {min(times['base']) * 1e3:.2f} | {'OK' if ok['base'] else 'WRONG'} |",
    f"| head | {mh * 1e3:.2f} | {min(times['head']) * 1e3:.2f} | {'OK' if ok['head'] else 'WRONG'} |",
    "",
    f"speedup base/head {mb / mh:.3f}, 95% CI [{lo:.3f}, {hi:.3f}]: **{verdict}**",
]
print("\n".join(lines))
if os.environ.get("GITHUB_STEP_SUMMARY"):
    open(os.environ["GITHUB_STEP_SUMMARY"], "a").write("\n".join(lines) + "\n")
sys.exit(0 if ok["base"] and ok["head"] else 1)

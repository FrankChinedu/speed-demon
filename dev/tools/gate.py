#!/usr/bin/env python3
"""Pre-push correctness gate. Standard library only.

A board run is one slot per team per 15 minutes, and a wrong hash wastes it. This
builds every track the push changes *from the pushed commit* (not the working tree)
and checks its output hash on several seeds and scales before git sends anything.

    python3 tools/gate.py install          write repo/.git/hooks/pre-push
    python3 tools/gate.py check [SHA]      gate one commit by hand (default HEAD)

Only the pushed tip is gated, since the board scores only the tip. Skipped when its
subject starts with "TIMING PROBE" (probes fail the hash on purpose) and when
SD_SKIP_GATE=1.
"""
import os
import shutil
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.environ.get("SD_REPO", os.path.join(ROOT, "repo"))
SD = os.path.join(ROOT, "tools", "sd.py")
TRACKS = "abcd"
BRANCH = "refs/heads/speed-demon"
# (seed, scale): unseen seeds catch code that only fits seed 1's data.
CASES = [(1, "tiny"), (2, "tiny"), (3, "tiny"), (3, "sample"), (1, "full"), (2, "full")]
ZERO = "0" * 40


def git(*args):
    return subprocess.run(["git", "-C", REPO, *args], capture_output=True, text=True,
                          check=True).stdout.strip()


def changed_tracks(old, new):
    if old == ZERO:
        return list(TRACKS)
    names = git("diff", "--name-only", old, new).splitlines()
    return [t for t in TRACKS if any(n.startswith(f"track-{t}/") for n in names)]


def gate(sha, tracks):
    """True when every track passes every case at commit `sha`."""
    subject = git("log", "-1", "--format=%s", sha)
    if subject.startswith("TIMING PROBE"):
        print(f"gate: skip {sha[:7]} ({subject[:50]})")
        return True
    if not tracks:
        print(f"gate: {sha[:7]} changes no track")
        return True
    # -m: files dated now, not at the commit, so cargo never reuses an older build.
    # Inside the workspace, so the pinned rust-toolchain.toml at its root applies.
    tree = os.path.join(ROOT, ".build", "gate-src")
    shutil.rmtree(tree, ignore_errors=True)
    os.makedirs(tree)
    archive = os.path.join(ROOT, ".build", "gate-src.tar")
    subprocess.run(["git", "-C", REPO, "archive", "-o", archive, sha, *[f"track-{t}" for t in tracks]],
                   check=True)
    subprocess.run(["tar", "-mxf", archive, "-C", tree], check=True)
    path = os.path.expanduser("~/.cargo/bin") + os.pathsep + os.environ.get("PATH", "")
    env = dict(os.environ, PATH=path, SD_REPO=tree, SD_BUILD=os.path.join(ROOT, ".build", "gate"))
    ok = True
    for t in tracks:
        for seed, scale in CASES:
            t0 = time.monotonic()
            p = subprocess.run([sys.executable, SD, "run", t, "--scale", scale, "--runs", "1"],
                               env=dict(env, SD_SEED=str(seed)), capture_output=True, text=True)
            verdict = "PASS" if p.returncode == 0 else "FAIL"
            print(f"gate: {verdict} track {t} {scale}-seed{seed} ({time.monotonic() - t0:.1f} s)")
            if p.returncode:
                print("\n".join(("      " + l) for l in p.stdout.splitlines()[-12:]))
                ok = False
                break  # the next cases of this track would only repeat the news
    shutil.rmtree(tree, ignore_errors=True)
    return ok


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "hook"
    if os.environ.get("SD_SKIP_GATE") == "1":
        print("gate: SD_SKIP_GATE=1, not checking")
        return 0
    if cmd == "install":
        hook = os.path.join(git("rev-parse", "--absolute-git-dir"), "hooks", "pre-push")
        os.makedirs(os.path.dirname(hook), exist_ok=True)
        with open(hook, "w") as f:
            f.write(f'#!/bin/sh\nexec python3 "{os.path.abspath(__file__)}" hook "$@"\n')
        os.chmod(hook, 0o755)
        print(f"installed {hook}")
        return 0
    if cmd == "check":
        sha = git("rev-parse", sys.argv[2] if len(sys.argv) > 2 else "HEAD")
        return 0 if gate(sha, changed_tracks(git("rev-parse", f"{sha}~1"), sha)) else 1
    # git pre-push: stdin lines are "<local ref> <local sha> <remote ref> <remote sha>".
    ok = True
    for line in sys.stdin:
        _, new, remote_ref, old = line.split()
        if remote_ref == BRANCH and new != ZERO:
            ok = gate(new, changed_tracks(old, new)) and ok
    if not ok:
        print("gate: push blocked; fix it, or SD_SKIP_GATE=1 git push if you mean it")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())

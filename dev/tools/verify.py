#!/usr/bin/env python3
"""Run a track's release binary on every local dataset and compare with tools/expected.json.

    python3 tools/verify.py a
"""
import hashlib, json, os, shutil, subprocess, sys
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUTPUT = {"a": "summary.txt", "b": "results.txt", "c": "aggregates.txt", "d": "distances.txt"}
t = sys.argv[1]
names = {"tiny": "tiny-seed1", "sample": "sample-seed1", "full": "full-seed1"}
ok = True
for key, want in json.load(open(os.path.join(ROOT, "tools", "expected.json")))[t].items():
    out = os.path.join(ROOT, ".out", t)
    shutil.rmtree(out, True)
    os.makedirs(out)
    subprocess.run([os.path.join(ROOT, "repo", f"track-{t}", "target", "release", "solution"),
                    os.path.join(ROOT, "data", t, names.get(key, key)), out], check=True)
    got = hashlib.sha256(open(os.path.join(out, OUTPUT[t]), "rb").read()).hexdigest()
    ok &= got == want
    print(f"{key:12} {'PASS' if got == want else 'FAIL'}")
sys.exit(0 if ok else 1)

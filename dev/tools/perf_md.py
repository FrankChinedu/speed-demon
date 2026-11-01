#!/usr/bin/env python3
"""Regenerate the board-run table in each repo/track-*/PERF.md from tools/runs.tsv.

    python3 tools/perf_md.py

Rows without a note are reruns of unchanged code and are left out. Everything in a
PERF.md above the marker line is kept as written.
"""
import csv
import os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MARKER = "## Board runs"
REF = {"a": (32.05, 228), "b": (56.35, 2200), "c": (14.44, 367), "d": (72.69, 893)}

with open(os.path.join(ROOT, "tools", "runs.tsv")) as f:
    runs = list(csv.DictReader(f, delimiter="\t"))

for track in "abcd":
    path = os.path.join(ROOT, "repo", f"track-{track}", "PERF.md")
    with open(path) as f:
        head = f.read().split(MARKER)[0].rstrip() + "\n\n"
    t_ref, m_ref = REF[track]
    rows = [
        "| Commit | Change | Run ID | T (s) | M (MB) | S | K |",
        "|---|---|---|---|---|---|---|",
        f"| | The reference, unchanged | | {t_ref} | {m_ref} | | 1.0 |",
    ]
    for r in runs:
        if r["track"] == track and r["note"].strip():
            rows.append(f"| {r['commit']} | {r['note']} | {r['run_id']} | {r['T_s']} | {r['M_MB']} | {r['S']} | {r['K']} |")
    body = (f"{MARKER}\n\nEvery step measured on the scoring box by the live board, in order. "
            "Rows marked FAILED EXPERIMENT were measured and reverted.\n\n" + "\n".join(rows) + "\n")
    with open(path, "w") as f:
        f.write(head + body)
    print(f"track-{track}: {len(rows) - 3} board steps")

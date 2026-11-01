#!/usr/bin/env python3
"""Show our latest board runs and the top of each track. Standard library only.

    python3 tools/board.py [team]
"""
import json
import sys
import urllib.request

URL = "https://s3.amazonaws.com/raptors.dev/speeddemon.dev/2026/board/board.json"
team = sys.argv[1] if len(sys.argv) > 1 else "Deez-attacher"
d = json.load(urllib.request.urlopen(urllib.request.Request(URL, headers={"User-Agent": "sd-board/1.0"})))
print("generated", d["generated_at"], "| queue", json.dumps(d["queue"])[:160])
import os
log = os.path.join(os.path.dirname(os.path.abspath(__file__)), "runs.tsv")
known = open(log).read() if os.path.exists(log) else ""
for t in d["teams"]:
    if t["team"] == team:
        with open(log, "a") as out:
            for k, v in sorted(t["tracks"].items()):
                r = v.get("latest") or {}
                if r.get("run_id") and r["run_id"] not in known and r.get("stage") == "scored":
                    out.write(f"{r['commit'][:7]}\t{k}\t{r['run_id']}\t{r['T_s']:.4f}\t{r['M_bytes'] / 1e6:.1f}\t{r['S']}\t{r['K']:.2f}\t\n")
        print(team, "watch head", t["watch"]["head"][:7], "in flight", t["queue"])
        for k, v in sorted(t["tracks"].items()):
            for which in ("latest", "best"):
                r = v.get(which) or {}
                print(f"  {k} {which:6} {r.get('run_id')} {r.get('commit', '')[:7]} {r.get('stage')} ok={r.get('ok')} "
                      f"T={r.get('T_s')} M={(r.get('M_bytes') or 0) / 1e6:.1f}MB S={r.get('S')} K={r.get('K')} "
                      f"fail={r.get('failure')}")
for k in "abcd":
    rows = sorted(((v["best"]["K"], t["team"], v["best"]["T_s"], v["best"]["M_bytes"] / 1e6, v["best"]["S"])
                   for t in d["teams"] for kk, v in t["tracks"].items()
                   if kk == k and (v.get("best") or {}).get("ok") and v["best"].get("K")), reverse=True)
    rank = next((i + 1 for i, r in enumerate(rows) if r[1] == team), None)
    print(f"{k}: rank {rank}/{len(rows)}  top: " + "; ".join(f"{n} K={K:.1f} T={T:.3f} M={M:.0f} S={S}" for K, n, T, M, S in rows[:3]))
    # Where the leader's K comes from: their/our ratio per axis raised to its weight.
    # The axis with the biggest factor is where the next hour of work should go.
    ours = next((r for r in rows if r[1] == team), None)
    if ours and rows[0][1] != team:
        _, _, T, M, S = ours
        _, lead, LT, LM, LS = rows[0]
        print(f"   x{rows[0][0] / ours[0]:.2f} to {lead}: T x{(T / LT) ** 0.5:.2f}  M x{(M / LM) ** 0.2:.2f}"
              f"  S x{(S / LS) ** 0.15:.2f}   (targets T={LT:.3f} s, M={LM:.0f} MB, S={LS})")

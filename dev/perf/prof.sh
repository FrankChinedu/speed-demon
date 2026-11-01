#!/bin/sh
# usage: prof.sh <args...>; profiles /tmp/t/release/solution, prints hottest source lines
set -e
cd /src
CARGO_TARGET_DIR=/tmp/t RUSTFLAGS="-C relocation-model=static -C target-cpu=native" cargo build --release -q 2>&1 | tail -3
B=/tmp/t/release/solution
$B "$@" >/dev/null 2>&1 || true
perf record -q -o /tmp/p.data -F 15000 $B "$@" >/dev/null 2>&1 || true
perf script -i /tmp/p.data -F ip,sym 2>/dev/null | awk '{print $1}' > /tmp/ips
total=$(wc -l < /tmp/ips)
sort /tmp/ips | uniq -c | sort -rn | head -400 > /tmp/top
awk '{print $2}' /tmp/top | addr2line -e $B -i -f -C 2>/dev/null > /tmp/lines || true
# addr2line -i can print several lines per address; use non-inline (last) is complex: use plain mode
awk '{print $2}' /tmp/top | addr2line -e $B 2>/dev/null > /tmp/l1
paste -d' ' /tmp/top /tmp/l1 | awk -v t=$total '{c[$3]+=$1} END {for (k in c) printf "%6.2f%% %s\n", 100*c[k]/t, k}' | sort -rn | head -${N:-30}

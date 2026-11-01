#!/bin/sh
# usage: tools/amd64verify.sh <track letter>   (run from the workspace root)
# Builds repo/track-<t> for x86-64 (target-cpu=x86-64-v3 from .cargo/config.toml, so the
# AVX2 paths compile in) in an amd64 container and checks every dataset in expected.json.
t=$1
case $t in a) f=summary.txt;; b) f=results.txt;; c) f=aggregates.txt;; d) f=distances.txt;; esac
docker run --rm --platform linux/amd64 -v "$PWD/repo/track-$t":/src:ro -v "$PWD/data/$t":/d:ro \
  -v "$PWD/tools/expected.json":/e.json:ro rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb sh -c "
  cp -r /src /w && cd /w && rm -rf target && cargo build --release -q 2>&1 | tail -3
  grep -q avx2 /proc/cpuinfo && echo 'cpu: avx2 present' || echo 'cpu: NO avx2'
  python3 -c 'import json;[print(k,v) for k,v in json.load(open(\"/e.json\"))[\"$t\"].items()]' 2>/dev/null > /tmp/e ||
    sed -n '/\"$t\": {/,/}/p' /e.json | grep -oE '\"[a-z0-9-]+\": \"[0-9a-f]{64}\"' | tr -d '\":' > /tmp/e
  fail=0
  while read k want; do
    case \$k in tiny) d=tiny-seed1;; sample) d=sample-seed1;; full) d=full-seed1;; *) d=\$k;; esac
    mkdir -p /o/\$k; ./target/release/solution /d/\$d /o/\$k
    got=\$(sha256sum /o/\$k/$f | cut -d' ' -f1)
    if [ \"\$got\" = \"\$want\" ]; then echo \"\$k PASS\"; else echo \"\$k FAIL\"; fail=1; fi
  done < /tmp/e
  exit \$fail"

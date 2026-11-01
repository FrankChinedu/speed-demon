#!/bin/sh
# usage: tools/pgo.sh <track letter>  (run from the workspace root)
# Builds repo/track-<t> instrumented for x86-64-v3 in the pinned rust image (amd64),
# runs it on full seeds 2 and 3 (never seed 1, the board's data), merges the profile
# with the toolchain's own llvm-profdata and writes repo/track-<t>/pgo.profdata.
t=$1
docker run --rm --platform linux/amd64 -v "$PWD/repo/track-$t":/src -v "$PWD/data/$t":/d:ro \
  rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb sh -c '
  set -e
  rustup component add llvm-tools >/dev/null 2>&1
  PROFDATA=$(find / -name llvm-profdata -type f 2>/dev/null | head -1)
  cp -r /src /w && cd /w && rm -rf target pgo.profdata .cargo
  RUSTFLAGS="-C target-cpu=x86-64-v3 -C profile-generate=/tmp/pgo" cargo build --release -q
  for s in full-seed2 full-seed3; do mkdir -p /o/$s; ./target/release/solution /d/$s /o/$s; done
  $PROFDATA merge -o /src/pgo.profdata /tmp/pgo
  ls -la /src/pgo.profdata'

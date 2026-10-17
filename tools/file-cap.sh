#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
prod=${FILE_CAP_PROD:-400}
test_cap=${FILE_CAP_TEST:-600}
bad=0
n=0
while IFS= read -r f; do
  case "$f" in target/*|*/target/*) continue ;; esac
  [ -f "$f" ] || continue
  case "$f" in tests/*|*/tests/*|tests.rs|*/tests.rs) cap=$test_cap ;; *) cap=$prod ;; esac
  lines=$(wc -l <"$f")
  n=$((n + 1))
  if [ "$lines" -gt "$cap" ]; then
    echo "file-cap: $f $lines > $cap"
    bad=$((bad + 1))
  fi
done < <(git ls-files -co --exclude-standard '*.rs')
[ "$bad" -eq 0 ] || exit 1
echo "file-cap: $n files ok"

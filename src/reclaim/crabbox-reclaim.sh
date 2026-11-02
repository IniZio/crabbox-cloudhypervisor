#!/bin/sh
set -u
state=/run/crabbox-reclaim
grep -q '^/' /proc/swaps && exit 0
refaults=$(awk '$1 == "workingset_refault_file" { print $2 }' /proc/vmstat)
last=$refaults
skip=0
[ -r "$state" ] && read -r last skip <"$state"
if [ $(((refaults - last) * 4)) -gt $((@REFAULT_MIB@ * 1024)) ]; then
  skip=@SKIP_RUNS@
  echo "$refaults $skip" >"$state"
  exit 0
fi
if [ "$skip" -gt 0 ]; then
  echo "$refaults $((skip - 1))" >"$state"
  exit 0
fi
echo "$refaults 0" >"$state"
file_kib=$(awk '$1 == "Active(file):" || $1 == "Inactive(file):" { n += $2 } END { print n + 0 }' /proc/meminfo)
excess=$((file_kib - @FLOOR_MIB@ * 1024))
[ "$excess" -gt 0 ] || exit 0
echo "${excess}K" >/sys/fs/cgroup/memory.reclaim || true

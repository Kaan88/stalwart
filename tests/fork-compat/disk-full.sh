#!/usr/bin/env bash
# Step 5: a convert that runs out of disk must stop with an error, and running
# it again once there is space must finish. Uses the baseline of an earlier
# run.sh rocksdb run (blobs inside RocksDB), copied onto a small tmpfs. Needs root
# (mount) and the same environment as run.sh.
#
#   tests/fork-compat/disk-full.sh <encrypt: 0|1>
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/lib.sh"
ENCRYPT="$1"
: "${FORK_BIN:?}" "${OFFICIAL_BIN:?}" "${INSPECT_BIN:?}"
SRC="${WORK:-/tmp/fork-compat}/rocksdb-enc$ENCRYPT"
MNT="${WORK:-/tmp/fork-compat}/diskfull-mnt"
LIVE="$MNT/live"; STATE="$SRC/state.json"
[ -d "$SRC/baseline" ] || { echo "run run.sh rocksdb $ENCRYPT first"; exit 1; }
SERVER_DIR="$LIVE"

base_kb=$(du -sk "$SRC/baseline" | cut -f1)
size_mb=$(( base_kb / 1024 + 700 ))
mkdir -p "$MNT"; umount "$MNT" 2>/dev/null
mount -t tmpfs -o size=${size_mb}m tmpfs "$MNT" || { echo "cannot mount tmpfs"; exit 1; }
trap 'stop_server "$LIVE"; umount "$MNT" 2>/dev/null' EXIT
rm -rf "$LIVE"; cp -a "$SRC/baseline" "$LIVE"
python3 - "$SRC/live" "$LIVE" <<'EOF'
import sys, pathlib
p = pathlib.Path(sys.argv[2], 'etc', 'config.json')
p.write_text(p.read_text().replace(sys.argv[1].rstrip('/') + '/', sys.argv[2].rstrip('/') + '/'))
EOF
grep -q "$LIVE/data/" "$LIVE/etc/config.json" || { echo "config not rewritten"; cat "$LIVE/etc/config.json"; exit 1; }

free_kb=$(df -k --output=avail "$MNT" | tail -1)
fill_kb=$(( free_kb - 60 * 1024 ))
dd if=/dev/zero of="$MNT/filler" bs=1M count=$(( fill_kb / 1024 )) status=none
echo "tmpfs ${size_mb} MB, free before convert: $(df -h --output=avail "$MNT" | tail -1)"

OUT="$(run_tool "$FORK_BIN" "$LIVE" convert)"; RC=$?
echo "$OUT" | tail -3; echo "stderr:"; grep -v "^\s*$" "$LIVE/logs/tool.err" | tail -12 | cut -c1-300; grep -ahiE "panic|No space|ENOSPC|IO error" "$LIVE"/logs/stalwart.* | tail -6 | cut -c1-400
check "convert on a full disk stops with an error" "$RC" 1
check "it did not claim success" "$(echo "$OUT" | grep -c 'result: clean')" 0
run_tool "$FORK_BIN" "$LIVE" report | grep -E "state=converting|^report:"

rm -f "$MNT/filler"
OUT="$(run_tool "$FORK_BIN" "$LIVE" convert)"; RC=$?; echo "$OUT" | tail -2
check "convert after freeing space finishes" "$RC/$(echo "$OUT" | grep -c 'result: clean')" "0/1"
out="$("$INSPECT_BIN" "$LIVE/data")"; echo "$out"
for f in commit_values_nonempty manifests chunk_keys share_link_records; do
  check "checker $f" "$(echo "$out" | sed -n "s/.*$f=\([0-9]*\).*/\1/p")" 0
done
start_server "$OFFICIAL_BIN" "$LIVE" || bad "official start"
python3 "$HERE/compat.py" verify "$LIVE" "$STATE" all | grep -v PASS; [ "${PIPESTATUS[0]}" = 0 ] && ok "official serves everything after the disk-full convert" || bad "official after disk-full convert"
echo "== disk full, encryption $ENCRYPT: $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]

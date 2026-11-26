# Shared helpers for tests/fork-compat/run.sh. Source it; do not run it.
# Needs: curl, python3, stalwart-cli (npm install -g stalwart-cli).

PORT="${PORT:-18080}"
URL="http://127.0.0.1:${PORT}"

# start_server <binary> <server dir> [recovery]
start_server() {
  local bin="$1" dir="$2" mode="${3:-}"
  (
    cd "$dir" || exit 1
    export STALWART_RECOVERY_ADMIN="$(cat etc/admin.txt)"
    export STALWART_PUBLIC_URL="$URL" STALWART_HOSTNAME="127.0.0.1" STALWART_RECOVERY_MODE_PORT="$PORT"
    if [ "$mode" = recovery ]; then export STALWART_RECOVERY_MODE=1; fi
    nohup "$bin" --config=etc/config.json >> logs/stalwart.out 2>&1 &
    echo $! > etc/stalwart.pid
  )
  for _ in $(seq 1 240); do
    curl -s -o /dev/null "$URL/jmap/session" && return 0
    if ! kill -0 "$(cat "$dir/etc/stalwart.pid")" 2>/dev/null; then
      echo "server exited; see $dir/logs/stalwart.out" >&2; tail -5 "$dir/logs/stalwart.out" >&2; return 1
    fi
    sleep 0.5
  done
  echo "server did not answer; see $dir/logs/stalwart.out" >&2; return 1
}

# stop_server <server dir>: clean stop, waits until the process is gone.
stop_server() {
  local dir="$1" pid
  [ -f "$dir/etc/stalwart.pid" ] || return 0
  pid="$(cat "$dir/etc/stalwart.pid")"
  kill "$pid" 2>/dev/null || true
  for _ in $(seq 1 120); do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
  if kill -0 "$pid" 2>/dev/null; then kill -9 "$pid"; sleep 1; fi
  rm -f "$dir/etc/stalwart.pid"
}

# run_tool <binary> <server dir> <report|convert|nuke> [extra env assignments...]
# Prints the tool's output; returns its exit status.
run_tool() {
  local bin="$1" dir="$2" tool="$3"; shift 3
  ( cd "$dir" && env "$@" STALWART_FORK_TOOL="$tool" STALWART_PUBLIC_URL="$URL" STALWART_HOSTNAME=127.0.0.1 \
      STALWART_RECOVERY_ADMIN="$(cat etc/admin.txt)" "$bin" --config=etc/config.json 2>>logs/tool.err )
}

# copy_server <from dir> <to dir>: full copy of a stopped server, with its config
# pointing at the copy's own data.
copy_server() {
  local from="$1" to="$2"
  rm -rf "$to"; cp -a "$from" "$to"
  python3 - "$from" "$to" <<'EOF'
import sys, pathlib
src, dst = sys.argv[1], sys.argv[2]
p = pathlib.Path(dst, 'etc', 'config.json')
p.write_text(p.read_text().replace(src.rstrip('/') + '/', dst.rstrip('/') + '/'))
EOF
  : > "$to/logs/stalwart.out"; rm -f "$to/etc/stalwart.pid"
}

cli() { STALWART_URL="$URL" STALWART_USER=admin STALWART_PASSWORD="$(cut -d: -f2- "$SERVER_DIR/etc/admin.txt")" stalwart-cli --no-color "$@"; }

sha() { sha256sum "$1" | cut -d' ' -f1; }

PASS=0; FAIL=0
ok()  { echo "PASS  $*"; PASS=$((PASS+1)); }
bad() { echo "FAIL  $*"; FAIL=$((FAIL+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (got '$2', want '$3')"; fi; }

# setup_server <binary> <server dir> <blobs: rocksdb|fs> <encrypt: 0|1>
# A fresh server on RocksDB (as kweb-mail-for-stalwart/dev/stalwart-setup.sh),
# with users tester@ and second@kweb.test, file and upload limits raised to
# 300 MB, and, with encrypt=1, LZ4 compression and AES-256-GCM encryption of
# file and mail blobs.
setup_server() {
  local bin="$1" dir="$2" blobs="$3" encrypt="$4" blob_store
  rm -rf "$dir"; mkdir -p "$dir/etc" "$dir/data" "$dir/logs"
  python3 -c "import secrets;print('admin:'+secrets.token_urlsafe(18), end='')" > "$dir/etc/admin.txt"
  SERVER_DIR="$dir"
  if [ "$blobs" = fs ]; then
    mkdir -p "$dir/blobs"
    blob_store="{\"@type\":\"FileSystem\",\"path\":\"$dir/blobs/\",\"depth\":2}"
  else
    blob_store='{"@type":"Default"}'
  fi
  start_server "$bin" "$dir" || return 1
  cli update Bootstrap singleton --json "{
    \"defaultHostname\":\"localhost\",\"defaultDomain\":\"kweb.test\",
    \"requestTlsCertificate\":false,\"generateDkimKeys\":false,
    \"dataStore\":{\"@type\":\"RocksDb\",\"path\":\"$dir/data/\",\"blobSize\":4096,\"bufferSize\":134217728,\"poolWorkers\":null,\"cacheSize\":null},
    \"blobStore\":$blob_store,
    \"tracer\":{\"@type\":\"Log\",\"path\":\"$dir/logs/\",\"prefix\":\"stalwart.log\",\"rotate\":\"daily\",\"ansi\":false,\"multiline\":false,\"enable\":true,\"level\":\"info\",\"lossy\":false,\"events\":{},\"eventsPolicy\":\"exclude\"}}" >/dev/null || return 1
  stop_server "$dir"
  start_server "$bin" "$dir" recovery || return 1
  cli create NetworkListener --json \
    "{\"name\":\"http\",\"protocol\":\"http\",\"bind\":{\"127.0.0.1:$PORT\":true},\"useTls\":false,\"implicitTls\":false}" >/dev/null || return 1
  for user in tester second; do
    python3 -c "import secrets;print('$user@kweb.test:'+secrets.token_urlsafe(12), end='')" > "$dir/etc/$user.txt"
    cli create Account/User --field name=$user --field domainId=b --field 'roles={"@type":"User"}' \
      --field 'quotas={"maxDiskQuota":50000000000}' \
      --field "credentials={\"0\":{\"@type\":\"Password\",\"secret\":\"$(cut -d: -f2- "$dir/etc/$user.txt")\"}}" >/dev/null || return 1
  done
  cli update FileStorage singleton --json '{"maxSize":300000000}' >/dev/null || return 1
  cli update Jmap singleton --json '{"maxUploadSize":300000000,"uploadQuota":2000000000}' >/dev/null || return 1
  if [ "$encrypt" = 1 ]; then
    local key_id
    key_id=$(cli create SymmetricKey --json "{\"cipher\":\"aes256Gcm\",\"description\":\"compat test\",\"key\":{\"@type\":\"Value\",\"secret\":\"$(head -c 32 /dev/urandom | base64)\"}}" | sed -n 's/^Created SymmetricKey //p')
    [ -n "$key_id" ] || { echo "no key id" >&2; return 1; }
    cli update FileStorage singleton --json "{\"compressionAlgorithm\":\"lz4\",\"blobEncryptionKey\":\"$key_id\"}" >/dev/null || return 1
    cli update Email singleton --json "{\"compressionAlgorithm\":\"lz4\",\"blobEncryptionKey\":\"$key_id\"}" >/dev/null || return 1
  fi
  stop_server "$dir"
  start_server "$bin" "$dir"
}

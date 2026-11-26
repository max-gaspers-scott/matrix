#!/usr/bin/env bash
# One-shot setup for the Matrix side of the bridge stack.
#   - generates secrets into .env (kept on re-runs)
#   - generates + patches mautrix-whatsapp / mautrix-gmessages configs
#   - generates their appservice registrations
#   - writes the registration for our own Rust bridge service
#   - writes config/tuwunel.toml
# Safe to re-run: existing secrets and registrations are kept.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD

SERVER_NAME=${SERVER_NAME:-bridge.local}
# Where the homeserver reaches our Rust bridge service (compose service name).
BRIDGE_URL=${BRIDGE_URL:-http://bridge:29400}

WA_IMAGE=dock.mau.dev/mautrix/whatsapp:latest
GM_IMAGE=dock.mau.dev/mautrix/gmessages:latest

mkdir -p config/whatsapp config/gmessages config/registrations data/tuwunel data/bridge

gen() { openssl rand -hex 32; }

# ---------------------------------------------------------------- secrets
if [[ ! -f .env ]]; then
  cat > .env <<EOF
SERVER_NAME=$SERVER_NAME
# Token needed to register a normal Matrix account (e.g. to test with Element).
REGISTRATION_TOKEN=$(gen)
# Rust bridge service <-> homeserver
BC_AS_TOKEN=$(gen)
BC_HS_TOKEN=$(gen)
EOF
  chmod 600 .env
  echo "wrote .env"
fi
# secrets added after the first version of this script
grep -q '^PROVISION_SECRET=' .env || {
  echo "# mautrix provisioning API (account linking), used by the Rust bridge" >> .env
  echo "PROVISION_SECRET=$(gen)" >> .env
}
grep -q '^BRIDGE_UID=' .env || {
  echo "# bridge container runs as you, so data/bridge stays writable from the host"
  echo "BRIDGE_UID=$(id -u)"
  echo "BRIDGE_GID=$(id -g)"
} >> .env
# shellcheck disable=SC1091
source .env

# The bridge images run as root and write root-owned 0600 files; hand them back.
fix_owner() { docker run --rm -v "$ROOT/config:/c" alpine chown -R "$(id -u):$(id -g)" /c >/dev/null; }

# ---------------------------------------------------------------- mautrix bridges
# patch_bridge <dir> <port>
patch_bridge() {
  local cfg=$1/config.yaml port=$2 name
  name=$(basename "$1")
  python3 - "$cfg" "$SERVER_NAME" "$name" "$port" <<'PY'
import re, sys
path, server, name, port = sys.argv[1:]
s = open(path).read()

def sub(pattern, repl, count=1):
    global s
    new, n = re.subn(pattern, repl, s, count=count, flags=re.M)
    if n == 0 and repl not in s:
        sys.exit(f"{path}: pattern not found: {pattern}")
    s = new

# homeserver
sub(r'^    address: http://example\.localhost:8008$', '    address: http://tuwunel:8008')
sub(r'^    domain: example\.com$', f'    domain: {server}')
# appservice listener (inside the private docker network)
sub(r'^    address: http://localhost:%s$' % port, f'    address: http://{name}:{port}')
sub(r'^    hostname: 127\.0\.0\.1$', '    hostname: 0.0.0.0')
# database: SQLite, no Postgres
sub(r'^    type: postgres$', '    type: sqlite3-fk-wal')
sub(r'^    uri: postgres://user:password@host/database\?sslmode=disable$',
    f'    uri: file:/data/{name}.db?_txlock=immediate')
sub(r'^    max_open_conns: 5$', '    max_open_conns: 2')
# permissions: everyone on our server may use the bridge
sub(r'^        "\*": relay$', '        "*": relay')
sub(r'^        "example\.com": user$', f'        "{server}": user')
sub(r'^        "@admin:example\.com": admin$', f'        "@admin:{server}": admin')
# no federation anywhere
sub(r'^    federate_rooms: true$', '    federate_rooms: false')
# logging: info to stdout only (no log files filling the disk)
sub(r'^    min_level: debug$', '    min_level: info')
sub(r'^        - type: file\n(          .*\n)+', '')
open(path, 'w').write(s)
PY
  # backfill (already disabled by default) and E2EE (allow: false by default)
  # are left at their defaults; assert them so a new upstream default can't slip in.
  python3 - "$cfg" <<'PY'
import re, sys
s = open(sys.argv[1]).read()
for sect in ("backfill", "encryption"):
    m = re.search(rf'^{sect}:\n(?:    .*\n|\s*#.*\n|\n)*?    (enabled|allow): (\w+)', s, re.M)
    if not m or m.group(2) != "false":
        sys.exit(f"{sys.argv[1]}: expected {sect}.{m.group(1) if m else '?'} = false")
PY
}

# setup_bridge <dir> <image> <port>
setup_bridge() {
  local dir=$1 image=$2 port=$3
  if [[ ! -f $dir/config.yaml ]]; then
    docker run --rm -v "$ROOT/$dir:/data" "$image" >/dev/null 2>&1 || true
  fi
  fix_owner
  patch_bridge "$dir" "$port"
  sed -i -E "s/^    shared_secret: .*/    shared_secret: $PROVISION_SECRET/" "$dir/config.yaml"
  if [[ ! -f $dir/registration.yaml ]]; then
    # second run with a config present generates registration.yaml (and fills tokens into config.yaml)
    docker run --rm -v "$ROOT/$dir:/data" "$image" >/dev/null 2>&1 || true
    fix_owner
  fi
  [[ -f $dir/registration.yaml ]] || { echo "failed to generate $dir/registration.yaml" >&2; exit 1; }
  cp "$dir/registration.yaml" "config/registrations/$(basename "$dir").yaml"
  echo "ok: $dir"
}

setup_bridge config/whatsapp "$WA_IMAGE" 29318
setup_bridge config/gmessages "$GM_IMAGE" 29336

# ---------------------------------------------------------------- our Rust bridge service
cat > config/registrations/benschat.yaml <<EOF
id: benschat
url: $BRIDGE_URL
as_token: $BC_AS_TOKEN
hs_token: $BC_HS_TOKEN
sender_localpart: benschat_bot
rate_limited: false
namespaces:
  users:
    - exclusive: true
      regex: '@bc_.*:${SERVER_NAME//./\\.}'
  aliases: []
  rooms: []
EOF
echo "ok: config/registrations/benschat.yaml"

# ---------------------------------------------------------------- tuwunel
cat > config/tuwunel.toml <<EOF
# Generated by scripts/setup.sh — edit there, not here.
[global]
server_name = "$SERVER_NAME"
database_path = "/var/lib/tuwunel"
appservice_dir = "/registrations"

[global.http.listen]
address = ["0.0.0.0"]
port = 8008

[global.federation]
enable = false

[global.registration]
enable = true
token = "$REGISTRATION_TOKEN"
displayname_suffix = ""

[global.encryption]
allow = false

[global.presence]
allow_local = false
allow_incoming = false
allow_outgoing = false

[global.rocksdb]
# small VPS: keep the DB caches tiny
cache_capacity_mb = 16.0
write_buffer_capacity_mb = 8.0
parallelism_threads = 1
# required on btrfs/ZFS (dev laptop), harmless elsewhere
allow_fallocate = false

[global.database.pool]
workers = 2
workers_limit = 4

[global.cache]
capacity_modifier = 0.25
EOF
echo "ok: config/tuwunel.toml"

cat <<EOF

Done. Next:
  docker compose up -d
  # register a test user (first user becomes server admin):
  #   homeserver http://localhost:8008, registration token in .env
EOF

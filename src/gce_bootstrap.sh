#!/bin/bash
# Rendered by the borg queen into a burst VM's `startup-script` metadata.
# Runs as root on every boot; the marker makes it a no-op after the first.
# Secrets (tailnet key, one-time enrollment token) come from instance metadata
# and the queen removes them there once the drone heartbeats.
set -uo pipefail
MARK=/var/lib/borg-burst-bootstrapped
[ -e "$MARK" ] && exit 0
QUEEN_URL='__QUEEN_URL__'
DRONE='__DRONE__'
TAG='__TAG__'
MAX_VCPU='__MAX_VCPU__'
MAX_MEM_MIB='__MAX_MEM_MIB__'
MD=http://metadata.google.internal/computeMetadata/v1/instance/attributes
W=$(mktemp -d)
chmod 700 "$W"
cd "$W" || exit 1
md() { curl -fsS -H 'Metadata-Flavor: Google' "$MD/$1"; }
md borg-ts-authkey > ts.key || { echo "borg-burst: no tailnet key in metadata"; exit 2; }
md borg-enroll-token > drone.token || { echo "borg-burst: no enrollment token in metadata"; exit 3; }
export DEBIAN_FRONTEND=noninteractive
id runner >/dev/null 2>&1 || useradd -m -s /bin/bash runner
apt-get update -q >/dev/null
# The toolchain script requires the shellcheck command but does not install
# it; without it the baseline check fails (rc=1) on a fresh VM.
apt-get install -y -q curl protobuf-compiler postgresql shellcheck >/dev/null || { echo "borg-burst: apt extras failed"; exit 10; }
systemctl disable --now postgresql >/dev/null 2>&1 || true
command -v tailscale >/dev/null || curl -fsSL https://tailscale.com/install.sh | sh >/dev/null 2>&1
tailscale up --auth-key="file:$W/ts.key" --hostname="$DRONE" --advertise-tags="$TAG" || { shred -u ts.key; echo "borg-burst: tailscale up failed"; exit 11; }
shred -u ts.key
for _ in $(seq 1 30); do
  curl -fsS -o borg "$QUEEN_URL/bootstrap/borg" && break
  sleep 10
done
[ -s borg ] || { echo "borg-burst: could not fetch the drone binary from $QUEEN_URL"; exit 12; }
curl -fsS -o toolchain.sh "$QUEEN_URL/bootstrap/toolchain.sh" || { echo "borg-burst: could not fetch the toolchain script"; exit 13; }
# The script's last check, `sudo -n` as `runner`, fails by design (runner
# gets no sudo; jobs run as borg-job), after every tool check has passed.
# Any other failure is real: show the log tail on the serial console. The
# drone's `tools` contract still decides whether the host is usable.
bash ./toolchain.sh --install > /var/log/borg-toolchain.log 2>&1
rc=$?
if [ "$rc" -eq 0 ] || { [ "$rc" -eq 1 ] && tail -n 1 /var/log/borg-toolchain.log | grep -q 'sudo: a password is required'; }; then
  echo "borg-burst: toolchain ok (rc=$rc)"
else
  echo "borg-burst: toolchain FAILED rc=$rc"
  tail -n 20 /var/log/borg-toolchain.log | sed 's/^/borg-burst: toolchain| /'
fi
install -m 755 ./borg /usr/local/bin/borg
/usr/local/bin/borg drone install --queen "$QUEEN_URL" --token-file "$W/drone.token" \
  --trust shared --max-vcpu "$MAX_VCPU" --max-mem-mib "$MAX_MEM_MIB"
rc=$?
shred -u drone.token
[ "$rc" -eq 0 ] || { echo "borg-burst: drone install rc=$rc"; exit 14; }
# Jobs run as borg-job and share the toolchain's Cargo registry and git
# caches. Read CARGO_HOME from the cargo wrapper the toolchain script installs
# so this script names no host-specific path.
CARGO_CACHE_HOME=$(sed -n 's/^export CARGO_HOME=//p' /usr/local/bin/cargo 2>/dev/null)
if [ -n "$CARGO_CACHE_HOME" ]; then
  mkdir -p "$CARGO_CACHE_HOME/registry" "$CARGO_CACHE_HOME/git"
  chown -R borg-job: "$CARGO_CACHE_HOME/registry" "$CARGO_CACHE_HOME/git"
else
  echo "borg-burst: no CARGO_HOME in /usr/local/bin/cargo; Cargo caches not prepared"
fi
cd / && rm -rf "$W"
touch "$MARK"
echo "borg-burst: BOOTSTRAP_DONE $DRONE"

#!/usr/bin/env bash
# End-to-end check used by CI: switch / status / run / second switch / rollback / gc against
# real registries. Usage: tests/ci-integration.sh /path/to/droidc ROOT_DIR
set -euo pipefail
D=$(readlink -f "$1"); ROOT=$2
export DROIDC_L2S_PREFIX=${DROIDC_L2S_PREFIX:-.proot.l2s.}   # Ubuntu's proot is upstream proot-me
J() { "$D" --root "$ROOT" --json "$@" | tee -a "$ROOT.log" | tail -n1; }
fail() { echo "FAIL: $*" >&2; exit 1; }
mkdir -p "$(dirname "$ROOT")"; : > "$ROOT.log"
ROOT_REAL=$(readlink -f "$ROOT")

# Run a command inside the current deployment. proot first (what Selkie uses; Ubuntu's 5.1.0 is not
# reliable on GitHub runners), else sudo chroot with the root bind-mounted at its own absolute path
# so the absolute host paths inside link2symlink chains resolve.
run_in() {
  if [ "${USE_CHROOT:-0}" != 1 ] && PROOT_NO_SECCOMP=1 proot -r "$ROOT/current/rootfs" -w / "$@"; then return 0; fi
  echo "(proot failed, using chroot fallback)" >&2
  USE_CHROOT=1
  local rf; rf=$(readlink -f "$ROOT/current/rootfs")
  sudo mkdir -p "$rf/var/home"   # bootc images have /home -> var/home (dangling until var is bound)
  sudo mkdir -p "$rf$ROOT_REAL"
  sudo mount --bind "$ROOT_REAL" "$rf$ROOT_REAL"
  local rc=0; sudo chroot "$rf" "$@" || rc=$?
  sudo umount "$rf$ROOT_REAL"
  return $rc
}

echo "== platform"; "$D" --json platform | tail -n1

echo "== switch (centos-bootc:stream10, link2symlink)"
R1=$(J switch quay.io/centos-bootc/centos-bootc:stream10 --insecure-allow-unsigned --links link2symlink)
echo "$R1" | jq -e '.ok == true and .data.changed == true' >/dev/null || fail "switch: $R1"
ID1=$(echo "$R1" | jq -r .data.id)
echo "$R1" | jq '.data.deployment.stats'
S=$(J status)
[ "$(echo "$S" | jq -r .data.current.id)" = "$ID1" ] || fail "status current"
[ "$(echo "$S" | jq -r .data.current.insecure)" = "true" ] || fail "insecure flag"
grep -q 'CentOS Stream' "$ROOT/current/rootfs/usr/lib/os-release" || fail "os-release"
grep -q '"10' "$ROOT/current/rootfs/usr/lib/os-release" || fail "os-release version"
[ "$(readlink "$ROOT/current")" = "deployments/$ID1" ] || fail "current is not a relative link"

echo "== run in the deployment (proot)"
OUT=$(run_in /usr/bin/cat /etc/os-release)
echo "$OUT" | grep -q 'CentOS Stream' || fail "proot cat"
run_in /usr/bin/true || fail "proot true (hard-linked binary)"

echo "== shell-spec"
J shell-spec | jq -e '.ok and (.data.binds | length == 3)' >/dev/null || fail "shell-spec"

echo "== upgrade is a no-op"
J upgrade | jq -e '.ok and .data.changed == false' >/dev/null || fail "upgrade no-op"

echo "== second switch (stream9: a different digest)"
R2=$(J switch quay.io/centos-bootc/centos-bootc:stream9 --insecure-allow-unsigned --links link2symlink)
echo "$R2" | jq -e '.ok and .data.changed' >/dev/null || fail "switch2: $R2"
ID2=$(echo "$R2" | jq -r .data.id)
[ "$ID1" != "$ID2" ] || fail "digests should differ"
S=$(J status)
[ "$(echo "$S" | jq -r .data.current.id)" = "$ID2" ] || fail "current after switch2"
[ "$(echo "$S" | jq -r .data.previous.id)" = "$ID1" ] || fail "previous after switch2"
grep -q 'CentOS Stream' "$ROOT/current/rootfs/usr/lib/os-release" || fail "os-release 2"

echo "== rollback"
J rollback | jq -e '.ok and .data.changed' >/dev/null || fail "rollback"
S=$(J status)
[ "$(echo "$S" | jq -r .data.current.id)" = "$ID1" ] || fail "current after rollback"
[ "$(echo "$S" | jq -r .data.previous.id)" = "$ID2" ] || fail "previous after rollback"
run_in /usr/bin/cat /etc/os-release | grep -q 'CentOS Stream' || fail "proot after rollback"

echo "== gc"
G=$(J gc); echo "$G" | jq -e '.ok' >/dev/null || fail "gc"
test -d "$ROOT/deployments/$ID1" -a -d "$ROOT/deployments/$ID2" || fail "gc removed a kept deployment"

echo "== last line of every command is a result"
tail -n1 "$ROOT.log" | jq -e '.event == "result"' >/dev/null || fail "last line"
echo "OK"

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
# reliable on GitHub runners), else `droidc enter` as root (namespaces + chroot, what Selkie uses with
# root).
run_in() {
  if [ "${USE_CHROOT:-0}" != 1 ] && PROOT_NO_SECCOMP=1 proot -r "$ROOT/current/rootfs" -w / "$@"; then return 0; fi
  echo "(proot failed, using droidc enter)" >&2
  USE_CHROOT=1
  sudo "$D" --root "$ROOT_REAL" enter -- "$@"
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

echo "== enter (root: namespaces + chroot, link2symlink chains via the root self-bind)"
J shell-spec --launcher chroot | jq -e '.ok and .data.launcher.kind == "chroot" and (.data.binds | length == 4)' >/dev/null || fail "shell-spec chroot"
OUT=$(sudo "$D" --root "$ROOT_REAL" enter --env FOO=bar -- /bin/sh -c 'echo "pid=$$ foo=$FOO"; cat /etc/os-release; ls /run/droidc/root/deployments; echo "ptmx=$(readlink /dev/ptmx) cg=$(cat /proc/self/cgroup)"; ls /dev')
echo "$OUT" | grep -q 'pid=2 foo=bar' || fail "enter pid/env: $OUT"
echo "$OUT" | grep -q 'CentOS Stream' || fail "enter os-release"
echo "$OUT" | grep -qx 'ptmx=pts/ptmx cg=0::/' || fail "enter /dev/ptmx or cgroup namespace: $OUT"
! echo "$OUT" | grep -qE '^(sd[a-z]|nvme|loop|kmsg|mem)' || fail "enter: host /dev leaked: $OUT"
# without a pid namespace, init kills what the command left behind
sudo env DROIDC_ENTER_NO_NS=pid "$D" --root "$ROOT_REAL" enter -- /bin/sh -c '(setsid sleep 4245 &); exit 0'
pgrep -x sleep -a | grep -q 4245 && fail "enter left a process behind without a pid namespace"
sudo "$D" --root "$ROOT_REAL" enter -- /usr/bin/true || fail "enter true (link2symlink binary)"
RC=0; sudo "$D" --root "$ROOT_REAL" enter -- /bin/sh -c 'exit 3' || RC=$?
[ "$RC" = 3 ] || fail "enter exit status $RC"
[ -z "$(findmnt -rn -o TARGET | grep -F "$ROOT_REAL/" || true)" ] || fail "enter leaked mounts"

echo "== fix-ownership (root), then non-root commands refuse, then --undo"
F=$(sudo "$D" --root "$ROOT_REAL" --json fix-ownership | tail -n1)
echo "$F" | jq -c '.data | {applied, via_l2s, xattr_errors, skipped}'
echo "$F" | jq -e '.ok and .data.applied > 1000' >/dev/null || fail "fix-ownership: $F"
P=$(sudo "$D" --root "$ROOT_REAL" enter -- stat -c '%u %a' /usr/bin/passwd)
[ "$P" = "0 4755" ] || fail "passwd after fix-ownership: $P"
J shell-spec --launcher chroot | jq -e '[.data.launcher.warnings[] | select(test("ownership"))] | length == 0' >/dev/null \
  || fail "ownership warning after fix-ownership"
J upgrade | jq -e '.ok == false and .error.kind == "invalid"' >/dev/null || fail "non-root upgrade should refuse"
U=$(sudo "$D" --root "$ROOT_REAL" --json fix-ownership --undo | tail -n1)
echo "$U" | jq -e '.ok' >/dev/null || fail "undo: $U"
LEFT=$(sudo find "$ROOT_REAL" ! -user "$(id -u)" -print -quit)
[ -z "$LEFT" ] || fail "undo left files not owned by $(id -un): $LEFT"

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

echo "== switch --apply-ownership (root, activates the present stream9 deployment), upgrade keeps it"
A=$(sudo "$D" --root "$ROOT_REAL" --json switch quay.io/centos-bootc/centos-bootc:stream9 --insecure-allow-unsigned --links link2symlink --apply-ownership | tail -n1)
echo "$A" | jq -e --arg id "$ID2" '.ok and .data.changed and .data.id == $id and .data.ownership.ownership == "image" and .data.deployment.ownership == "image"' >/dev/null \
  || fail "switch --apply-ownership: $A"
P=$(sudo "$D" --root "$ROOT_REAL" enter -- stat -c '%u %a' /usr/bin/passwd)
[ "$P" = "0 4755" ] || fail "passwd after switch --apply-ownership: $P"
UP=$(sudo "$D" --root "$ROOT_REAL" --json upgrade | tail -n1)
echo "$UP" | jq -e '.ok and .data.changed == false and .data.ownership == null and .data.deployment.ownership == "image"' >/dev/null \
  || fail "root upgrade of an image-owned deployment: $UP"
sudo "$D" --root "$ROOT_REAL" --json fix-ownership --undo | tail -n1 | jq -e .ok >/dev/null || fail "final undo"

echo "== last line of every command is a result"
tail -n1 "$ROOT.log" | jq -e '.event == "result"' >/dev/null || fail "last line"
echo "OK"

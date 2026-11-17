#!/usr/bin/env bash
# End-to-end check used by CI: switch / status / run / second switch / rollback / gc against
# real registries. Usage: tests/ci-integration.sh /path/to/droidc ROOT_DIR
set -euo pipefail
D=$(readlink -f "$1"); ROOT=$2
export DROIDC_L2S_PREFIX=${DROIDC_L2S_PREFIX:-.proot.l2s.}   # Ubuntu's proot is upstream proot-me
J() { "$D" --root "$ROOT" --json "$@" | tee -a "$ROOT.log" | tail -n1; }
fail() { echo "FAIL: $*" >&2; exit 1; }
mkdir -p "$(dirname "$ROOT")"; : > "$ROOT.log"

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
OUT=$(proot -r "$ROOT/current/rootfs" -w / /usr/bin/cat /etc/os-release)
echo "$OUT" | grep -q 'CentOS Stream' || fail "proot cat"
proot -r "$ROOT/current/rootfs" -w / /usr/bin/true || fail "proot true (hard-linked binary)"

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
proot -r "$ROOT/current/rootfs" -w / /usr/bin/cat /etc/os-release | grep -q 'CentOS Stream' || fail "proot after rollback"

echo "== gc"
G=$(J gc); echo "$G" | jq -e '.ok' >/dev/null || fail "gc"
test -d "$ROOT/deployments/$ID1" -a -d "$ROOT/deployments/$ID2" || fail "gc removed a kept deployment"

echo "== last line of every command is a result"
tail -n1 "$ROOT.log" | jq -e '.event == "result"' >/dev/null || fail "last line"
echo "OK"

#!/bin/sh
# Inside a deployment, as the app uid under proot (what Selkie runs): what works and what breaks
# for Flatpak. `sudo` is droidc's shim, served by a host spawn-server running proot -0.
# Report only: every step prints its exit status and the next one runs anyway.
step() { echo; echo "### $*"; }
run() {
  echo "\$ $*"
  timeout "${T:-300}" "$@" > /tmp/probe.out 2>&1; rc=$?
  tail -n "${TAIL:-40}" /tmp/probe.out; echo "exit=$rc"
}
APP=${PROBE_APP:-com.github.tchx84.Flatseal}

step "identity and tools"
run id
run flatpak --version
run sh -c 'for b in bwrap dbus-daemon dbus-broker /usr/libexec/flatpak-system-helper /usr/libexec/polkitd /usr/lib/polkit-1/polkitd; do printf "%s: " "$b"; command -v "$b" || ls -l "$b" 2>&1; done'
run sh -c 'cat /proc/sys/user/max_user_namespaces; cat /proc/sys/kernel/unprivileged_userns_clone 2>&1'
run ls -la /etc/flatpak/remotes.d /usr/share/flatpak/remotes.d /usr/share/flatpak/preinstall.d /var/lib/flatpak
run sh -c 'cat /usr/share/flatpak/preinstall.d/*'

step "bwrap (needs user namespaces)"
run bwrap --version
run bwrap --bind / / --dev /dev --proc /proc /usr/bin/true

step "a system bus (session config, as droidc supervise starts it)"
mkdir -p /run/dbus
dbus-daemon --config-file=/usr/share/dbus-1/session.conf --address=unix:path=/run/dbus/system_bus_socket --fork --print-pid=1 && echo "system bus up"
export DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/dbus/system_bus_socket
run flatpak remotes --system --show-details

step "fake root over the spawner: sudo id, then the first-boot preinstall"
run sudo id
T=1200 TAIL=60 run sudo flatpak preinstall -y --noninteractive
run flatpak list --system --columns=application,branch,origin,installation
run flatpak remotes --system --show-details
run sh -c 'ls -la /var/lib/flatpak /var/lib/flatpak/repo; cat /var/lib/flatpak/repo/config'

step "run Bazaar as the user (bwrap)"
T=120 run flatpak run --command=bazaar io.github.kolunmi.Bazaar --help
T=120 run flatpak -v run --command=true io.github.kolunmi.Bazaar

step "system install as the user, no helper on the bus (what Bazaar does)"
T=300 run flatpak install --system -y --noninteractive flathub "$APP"

step "system install as the user, flatpak-system-helper started by hand on the system bus (no polkit)"
/usr/libexec/flatpak-system-helper -r > /tmp/helper.log 2>&1 &
HELPER=$!
sleep 3
T=600 run flatpak install --system -y --noninteractive flathub "$APP"
kill $HELPER 2>/dev/null
run cat /tmp/helper.log

step "system install as the user, droidc polkit and the helper without RevokeFS (as supervise starts them)"
droidc polkit > /tmp/polkit.log 2>&1 &
POLKIT=$!
# A fresh cache: earlier steps leave one behind that a failed revokefs mount used.
rm -f "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/.flatpak-cache"
G_MESSAGES_DEBUG=all FLATPAK_DISABLE_REVOKEFS=1 /usr/libexec/flatpak-system-helper -r -v > /tmp/helper3.log 2>&1 &
HELPER=$!
sleep 3
T=1200 TAIL=80 run flatpak install -v --system -y --noninteractive flathub "$APP"
run sh -c 'ls -la /var/tmp/ /var/tmp/flatpak-cache-*/ 2>&1 | tail -30'
run flatpak list --system --columns=application,branch,installation
T=120 run flatpak run --command=true "$APP"
kill $HELPER $POLKIT 2>/dev/null
run sh -c 'tail -n 80 /tmp/polkit.log; tail -n 120 /tmp/helper3.log'

step "system install as the user, helper in session mode (skips polkit)"
dbus-daemon --session --address=unix:path=/tmp/session_bus --fork --print-pid=1 >/dev/null
DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/session_bus /usr/libexec/flatpak-system-helper --session -r > /tmp/helper2.log 2>&1 &
HELPER=$!
sleep 3
DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/session_bus FLATPAK_SYSTEM_HELPER_ON_SESSION=1 T=600 run flatpak install --system -y --noninteractive flathub "$APP"
kill $HELPER 2>/dev/null
run cat /tmp/helper2.log

step "user installation"
run flatpak --user remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
T=600 run flatpak --user install -y --noninteractive flathub "$APP"
run flatpak list --columns=application,installation
T=120 run flatpak run --command=true "$APP"

step "appstream refresh (system, fake root) and Bazaar's data"
T=600 run sudo flatpak update --appstream --system --noninteractive
run sh -c 'ls /var/lib/flatpak/appstream/*/*/ 2>&1 | head'
echo; echo "probe done"

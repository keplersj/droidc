#!/bin/sh
# Runs inside the guest under `droidc supervise --systemd=on -- sh tests/systemd-probe.sh`: how far
# does the image's own `systemd --user` get on the cgroup shim? Prints "key: value" lines (and a few
# lists) for the CI summary; never exits non-zero for a failed probe, only reports it.
now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.1fs", b - a }'; }
t0=$(now)

state=$(timeout 60 systemctl --user is-system-running --wait 2>&1 | tail -1)
echo "is-system-running: ${state:-timeout} (after $(elapsed "$t0"))"

echo "failed units:"
systemctl --user --failed --no-legend --plain 2>&1 | head -20

# transient unit: starts, ends, and systemd notices the cgroup emptying (cgroup.events)
systemd-run --user --unit=probe-sleep -p Type=simple sleep 1 >/dev/null 2>&1
sleep 3
echo "transient unit after exit: $(systemctl --user is-active probe-sleep 2>&1)"

# a double-forked daemon is found and killed with its unit (KillMode=control-group)
systemd-run --user --unit=probe-fork sh -c '(sleep 60 &) ; sleep 1' >/dev/null 2>&1
sleep 4
if ps -eo args | grep -q '^sleep 60'; then echo "double-forked daemon: SURVIVED"; else echo "double-forked daemon: killed with its unit"; fi

# the session bus is systemd's: activatable names come from the units' D-Bus service files
echo "bus: $DBUS_SESSION_BUS_ADDRESS"
echo "activatable names: $(busctl --user list --activatable --no-legend 2>/dev/null | wc -l)"
echo "bus names owned: $(busctl --user list --acquired --no-legend 2>/dev/null | wc -l)"

# the session itself: gnome-session's target (GNOME 50 dropped X11, so the unit is
# gnome-session@gnome.target; older images use gnome-session-wayland@gnome.target)
target=
for t in gnome-session@gnome.target gnome-session-wayland@gnome.target; do
  if [ -e "/usr/lib/systemd/user/${t%%@*}@.target" ]; then target=$t; break; fi
done
if [ -n "$target" ]; then
  t1=$(now)
  timeout 60 systemctl --user start "$target" 2>&1 | tail -3
  echo "$target: $(systemctl --user is-active "$target" 2>&1) (after $(elapsed "$t1"))"
  sleep 10
  echo "active gnome units:"
  systemctl --user list-units --state=active --no-legend --plain 'gnome*' 'org.gnome*' 2>&1 | awk '{print $1}' | head -40
  echo "failed units after gnome-session:"
  systemctl --user --failed --no-legend --plain 2>&1 | awk '{print $1, $3, $4}' | head -40
else
  echo "no gnome-session target in this image"
fi
exit 0

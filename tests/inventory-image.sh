#!/bin/sh
# Inventory of what a deployment's GNOME session needs from systemd. Run INSIDE the guest rootfs
# (chroot or proot), e.g. `chroot <rootfs> /bin/sh -s < tests/inventory-image.sh`. Read-only; every
# section is best effort so one missing tool does not hide the rest.
U=/usr/lib/systemd/user
S=/usr/lib/systemd/system

section() { printf '\n## %s\n\n' "$1"; }

section "Packages"
rpm -q gnome-session gnome-shell mutter systemd dbus-broker dbus-daemon polkit accountsservice \
  upower NetworkManager power-profiles-daemon xdg-desktop-portal-gnome 2>&1

section "gnome-session"
gnome-session --version 2>&1 | head -2
echo '--help (builtin / systemd options):'
gnome-session --help 2>&1 | grep -i -E 'builtin|systemd|session' | head -10
ls "$U"/gnome-session* 2>/dev/null | head -40

section "User unit types (gnome-session*, org.gnome.*, pipewire, xdg-desktop-portal*, dbus*)"
for f in "$U"/gnome-session*.service "$U"/org.gnome.*.service "$U"/pipewire*.service \
         "$U"/wireplumber*.service "$U"/xdg-desktop-portal*.service "$U"/dbus*.service; do
  [ -f "$f" ] && printf '%s\t%s\n' "$(sed -n 's/^Type=//p' "$f" | head -1)" "$(basename "$f")"
done | sort | awk -F'\t' '{t=$1==""?"(simple)":$1; n[t]++; l[t]=l[t] " " $2} END {for (t in n) print n[t], t ":" l[t]}'

section "Sandbox / namespace directives in those user units (fail without namespaces)"
grep -h -E '^(PrivateTmp|PrivateNetwork|PrivateUsers|PrivateDevices|ProtectSystem|ProtectHome|ProtectKernel[A-Za-z]*|ProtectControlGroups|ProtectProc|RestrictNamespaces|SystemCallFilter|MemoryDenyWriteExecute|NoNewPrivileges|CapabilityBoundingSet|Delegate|Slice|OOMScoreAdjust)=' \
  "$U"/gnome-session*.service "$U"/org.gnome.*.service "$U"/xdg-desktop-portal*.service 2>/dev/null \
  | cut -d= -f1 | sort | uniq -c | sort -rn

section "D-Bus services with SystemdService= (activation goes through systemd1 under dbus-broker)"
grep -l '^SystemdService=' /usr/share/dbus-1/services/*.service 2>/dev/null | wc -l | sed 's/^/count: /'
grep -H '^SystemdService=' /usr/share/dbus-1/services/*.service 2>/dev/null \
  | sed 's|/usr/share/dbus-1/services/||; s|:SystemdService=| -> |' | head -60
echo '--- activatable session services without SystemdService=:'
for f in /usr/share/dbus-1/services/*.service; do
  grep -q '^SystemdService=' "$f" || basename "$f"
done 2>/dev/null | head -40

section "System bus services (org.freedesktop.*, needed by GNOME on the system bus)"
ls /usr/share/dbus-1/system-services 2>/dev/null | head -60

section "System units wanted by graphical.target / gdm"
ls /usr/lib/systemd/system/graphical.target.wants /etc/systemd/system/graphical.target.wants 2>/dev/null | head -30
grep -h -E '^(Wants|Requires)=' "$S"/gdm.service "$S"/graphical.target 2>/dev/null | head

section "Interfaces referenced by the GNOME binaries (weak stand-in for a busctl trace)"
for b in /usr/bin/gnome-shell /usr/bin/gnome-session-binary /usr/libexec/gnome-session-binary \
         /usr/libexec/gnome-session-ctl /usr/lib64/libmutter-*/libmutter-*.so.* /usr/lib64/gnome-shell/libgnome-shell.so; do
  [ -f "$b" ] || continue
  printf '%s: ' "$b"
  strings -n 8 "$b" 2>/dev/null | grep -o -E 'org\.freedesktop\.(systemd1|login1)[A-Za-z.]*|StartTransientUnit|StartUnit|StopUnit|TakeDevice|TakeControl|SetEnvironment|UnsetEnvironment' \
    | sort -u | tr '\n' ' '
  echo
done

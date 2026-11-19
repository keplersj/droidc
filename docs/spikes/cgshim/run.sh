#!/bin/sh
# Spike: run a real `systemd --user` with no kernel cgroup access, using cgshim.so.
# Needs unprivileged user namespaces (this stands in for Android's "no cgroup writes"; it does NOT
# reproduce Android's missing namespaces, seccomp or proot).  Usage: sh run.sh
set -e
here=$(cd "$(dirname "$0")" && pwd)
gcc -shared -fPIC -O1 -o "$here/cgshim.so" "$here/../../../shim/cgshim.c" -ldl
exec unshare -Urm --pid --fork --mount-proc sh -c '
  mount -t tmpfs none /sys/fs/cgroup        # a plain tmpfs where cgroup2 would be
  mount -t tmpfs none /run
  mkdir -p /run/systemd/system /run/u && chmod 700 /run/u   # sd_booted() check; short XDG dir
  for f in cgroup.procs cgroup.controllers cgroup.subtree_control cgroup.events cgroup.type; do : > /sys/fs/cgroup/$f; done
  export XDG_RUNTIME_DIR=/run/u HOME=/tmp/h LD_PRELOAD='"$here"'/cgshim.so; mkdir -p $HOME
  /usr/lib/systemd/systemd --user --log-target=console & sleep 6
  systemctl --user is-system-running
  systemd-run --user --unit=t1 sleep 30 && systemctl --user status t1 --no-pager
  kill %1; wait'

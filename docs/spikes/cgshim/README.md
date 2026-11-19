Reproduces the cgroup-shim spike: `sh run.sh` builds `shim/cgshim.c` and runs a real
`systemd --user` with the kernel cgroup tree hidden. Needs unprivileged user namespaces; it
stands in for Android's "no cgroup writes", not for its missing namespaces, seccomp or proot.
The shim itself lives in `shim/cgshim.c`.

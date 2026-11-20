Reproduces the cgroup-shim spike: `sh run.sh` builds `crates/cgshim` and runs a real
`systemd --user` with the kernel cgroup tree hidden. Needs unprivileged user namespaces; it
stands in for Android's "no cgroup writes", not for its missing namespaces, seccomp or proot.
The shim itself lives in `crates/cgshim`.

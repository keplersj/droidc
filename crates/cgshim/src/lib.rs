//! `LD_PRELOAD` shim that lets a real `systemd --user` run where the kernel gives it no cgroup
//! filesystem (Android apps under proot). It replaces `shim/cgshim.c`.
//!
//! systemd refuses to start without a cgroup2 hierarchy it can write. Under proot on Android there
//! is none, so droidc binds a plain directory at `/sys/fs/cgroup` and this library makes systemd
//! believe it is cgroup2:
//!
//! - `statfs`/`fstatfs` on anything under the tree report `CGROUP2_SUPER_MAGIC`;
//! - `/proc/<pid>/cgroup` reads `0::<the tree directory that lists that pid>` (an in-memory file built
//!   from the tree's `cgroup.procs`, so `GetUnitByPID` and `sd_pid_get_unit` work);
//! - `mkdir` under the tree also creates the `cgroup.*` files real cgroupfs makes by itself, and
//!   `rmdir` removes them first; an open of one that is missing in an existing directory repairs it;
//! - `readdir` reports `DT_UNKNOWN` for `DT_LNK` (proot's link2symlink files are symlinks to
//!   `getdents` only);
//! - `prctl(PR_SET_SECUREBITS)` that an app uid is refused reports success;
//! - `posix_spawn("/proc/self/fd/N")`, how systemd 257 starts the executor, runs the file the fd
//!   names (proot cannot exec through `/proc/self/fd`);
//! - `posix_spawn(POSIX_SPAWN_SETCGROUP)` becomes a plain spawn plus a write of the pid to the unit's
//!   `cgroup.procs`;
//! - installing a seccomp filter (`prctl(PR_SET_SECCOMP)` or `seccomp(2)`, which is how systemd applies a
//!   unit's `SystemCallFilter=`) reports success and does nothing: such a filter kills services with
//!   SIGSYS here and the guest has no sandbox to keep;
//! - `cgroup.procs` is opened for append, so every pid systemd writes survives;
//! - `cgroup.events` starts as `populated 1` so a waiter sees a change when droidc's helper flips it.
//!
//! Membership and "populated" accounting are NOT done here: see `cgroups.rs` in the droidc crate.
//!
//! Layout: [`paths`], [`tree`] and [`spawn`] are safe Rust with unit tests. `ffi` is the only module
//! with `unsafe`: it exports the C symbols and does nothing but convert pointers, call the safe
//! functions, and fall through to the real libc function (found with `dlsym(RTLD_NEXT)`).
//!
//! Environment: `CGSHIM_TRACE` (log to stderr), `CGSHIM_ROOT` (the tree, for tests),
//! `CGSHIM_EXECUTOR` (the executor binary for a memfd copy).

pub mod paths;
pub mod spawn;
pub mod tree;

// The exports would override libc's own symbols in the unit-test binary.
#[cfg(not(test))]
mod ffi;

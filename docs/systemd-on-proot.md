# Running Bluefin's systemd-dependent session under proot

Evaluation note: what GNOME needs from systemd, what `droidc supervise` provides today, and the
options from "patch around it" up to "write `/bin/init` as a systemd for proot".
Date: 2026-11-18. Evidence is labelled **verified** (read or run here), **cited** (a source says so,
not checked against the binary) or **inferred**. Nothing here was run on Android or under proot yet.

## Summary

1. **`dbus-broker` images and `dbus-daemon` images behave differently.** dbus-broker forwards
   *every* activation to `org.freedesktop.systemd1` (cited, dbus-broker wiki "Deviations"), so on a
   dbus-broker-only image (CentOS Stream 10 bootc) anything activated through `SystemdService=`
   fails with no systemd1 on the bus. `supervise` on `main` prefers `dbus-daemon`, which starts
   `Exec=` services itself, so activation works on Bluefin LTS (which ships it) without systemd.
   That does not make a *session* work: gnome-session's systemd mode, app scopes and logind still need
   the pieces below, and the `dbus-daemon` route has no systemd1 for anything that calls it directly.
2. **GNOME has hard systemd dependencies**, and upstream is removing the fallback (cited, Adrian
   Vovk, [GNOME's systemd dependencies](https://blogs.gnome.org/adrianvovk/2025/06/10/gnome-systemd-dependencies/)):
   logind (sessions, seats), the systemd *user instance* for session services (gnome-session's
   builtin manager is "completely unused and untested" and being removed), and systemd-userdb (GDM).
   Bluefin LTS ships GNOME 48 and can be built for 49/50 (cited, docs.projectbluefin.io), so any
   plan that relies on gnome-session's builtin mode is a stopgap.
3. **Real `systemd --user` runs on a small cgroup shim** (`shim/cgshim.c`; verified on systemd 249 and on CentOS Stream 10's 257, not yet on proot, see Results). Real
   `systemctl`, `systemd-run --user` (`StartTransientUnit`) and `is-system-running` worked against it.
   That makes "shim cgroups, run real systemd for the user session" the cheapest route to full
   `org.freedesktop.systemd1` fidelity. It is unproven on Android/proot, where more than cgroups is
   missing (see "Is proot the limit?").
4. **Recommended shape:** a small Rust `/bin/init` (grown from `supervise`) that owns what real
   systemd cannot do here (system bus, login1 stub, journal sink, process tracking, tmpfiles/sysusers
   via the image's own binaries), and starts the **real** `systemd --user` on top of a cgroup shim.
   Fall back to writing our own systemd1 only if the spike fails on-device.

## What `supervise` did, and what changed

At the time of the first evaluation (`main` fe77ad3) `supervise` used `dbus-daemon` (which activates
`Exec=` services itself, so activation was not broken on Bluefin LTS, only on dbus-broker-only images),
had a system bus under `--session`, and never set `PR_SET_CHILD_SUBREAPER` (so its claim to reap
re-parented orphans was false under proot). Now:

| Item | State |
| --- | --- |
| Subreaper | done; `tests/supervise.rs` fails without it |
| `droidc login1` (logind stand-in, system bus) | done; `login1.rs`, tested against a private `dbus-daemon` |
| Journal socket stand-in in systemd mode | done |
| Image inventory | `tests/inventory-image.sh` + a `measure-bluefin` step (not yet run in CI) |
| cgroup shim + real `systemd --user` | `shim/cgshim.c` + `supervise --systemd=on` (off by default) |
| cgroup accounting helper | `cgroups.rs` |
| Probes under bwrap and proot | `tests/systemd-probe.sh` + two report-only `measure-bluefin` steps (not yet run) |

## Results so far

Run here (x86_64 Linux, unprivileged user namespaces, tmpfs over `/sys/fs/cgroup`; **not** proot, not
Android, not the Bluefin image: the host's own systemd 249 user units):

- `droidc supervise --systemd=on -- ...` starts the real `systemd --user`; `is-system-running` reports
  `running`; the real bus (`dbus.socket`) comes up under `$XDG_RUNTIME_DIR/bus` and lists activatable
  names; `systemd-run --user` starts units, and `--scope` works (`/proc/<pid>/cgroup` is answered from
  the tree, so `GetUnitByPID`-style lookups succeed).
- A finished unit used to stay `deactivating` forever (no `populated 0`). With `cgroups.rs` it goes
  `inactive` at once, and a double-forked daemon is killed with its unit (found by `INVOCATION_ID`).
- Bug found on the way and fixed: the manager inherits whatever `INVOCATION_ID` its launcher had, which
  named the wrong unit. Only `.service` cgroups teach the id map now, and orphans re-parented to the
  manager are not attributed to it.

Run on the real image `quay.io/centos-bootc/centos-bootc:stream10` (systemd 257, glibc 2.39), unprivileged
bwrap with a tmpfs-style directory over `/sys/fs/cgroup` (still **not** proot or Android):

- Systemd 257 needed more than systemd 249: it starts every unit with `posix_spawn(POSIX_SPAWN_SETCGROUP)`
  (glibc 2.39: `clone3` + `CLONE_INTO_CGROUP` on a cgroup fd), which the kernel rejects for a plain
  directory (`Failed to spawn executor: Bad file descriptor`, every unit `Result: resources`). The shim now
  turns that into a plain spawn plus a write to the unit's `cgroup.procs`. After that: `is-system-running`
  = `running`, no failed units, `dbus-broker.service` started by systemd, a `systemd-run` unit ends
  `inactive`, a double-forked daemon is killed with its unit, the bus lists 3 activatable names.
- No system bus on this image: it ships only `dbus-broker`, whose system scope needs systemd and root, so
  there is no `dbus-daemon` to run one on, and so no login1 stand-in either. Bluefin LTS ships
  `dbus-daemon`; check in the inventory.
- Visibility: with no journald the manager's own log goes nowhere under `--log-target=journal` or
  `console` in a minimal `/dev` (bwrap has no `/dev/console`). `DROIDC_JOURNAL=stderr` prints what reaches
  the stand-in journal socket, and `DROIDC_SYSTEMD_LOG_LEVEL`/`DROIDC_SYSTEMD_STDIO=inherit` raise the log
  level and keep unit output; that is how the spawn failure above was found.

### On the tablet (Galaxy Tab S10+, Android 16, SELinux enforcing)

Run as Selkie's own uid (`run-as io.github.keplersj.selkie`, a debuggable build) with Selkie's exact proot
command line (`libproot.so`, `--link2symlink`, the same binds and fake `/proc` files) against its
real Bluefin LTS deployment (GNOME 50, systemd 257, aarch64). Four things broke that no earlier test
could show, all fixed in `shim/cgshim.c` and each found by reading the manager's own log
(`DROIDC_JOURNAL=stderr`):

1. **No unit loaded**: `Failed to read symlink ... Invalid argument` for every unit file. Under
   link2symlink `lstat`/`readlink` say "regular file" but `getdents` still reports the underlying symlink
   (`DT_LNK`), and systemd believes `d_type`. `readdir` now reports `DT_UNKNOWN` for `DT_LNK`.
2. **Every service died with status 182**: systemd 257 starts the executor with
   `posix_spawn("/proc/self/fd/N")`, and proot's loader cannot exec through `/proc/self/fd` (it exits 182
   before the program runs). The shim execs the file the fd names.
3. **`213/SECUREBITS`**: the executor calls `prctl(PR_SET_SECUREBITS)`, which an app uid is refused.
   The shim reports success (there are no capabilities to lock down).
4. A stale `/run/user/UID` from an earlier killed run makes the generator fail with `File exists`;
   a persistent `/run` bind needs clearing at session start.

After those: `systemd --user` reaches **`running` in 1.3 s with no failed units and 56 activatable bus
names**; `systemd-run --user` works and finished units go `inactive`; the image's own `xdg-user-dirs`,
`dconf` and Bluefin's wallpaper scripts ran. `gnome-session@gnome.target` stayed inactive for 60 s: no
compositor was running and `ublue-user-setup` needs a setuid `pkexec`, so that proves nothing about GNOME
itself yet.

**Process accounting is the weak spot under proot.** A double-forked daemon is killed with its unit in
some runs and escapes in others (about half of 5 runs of a 1 s unit). Each tick of `cgroups.rs` takes
~300 ms there, because every `/proc` read is a ptrace stop, so an orphan that appears and whose unit ends
within a few ticks is never attributed. Two flaws that made it worse are fixed (a new cgroup was
reported empty before its first process was attached; emptiness now has to last two ticks, and a pid's
`INVOCATION_ID` is read once). What remains is latency, not logic. Ideas: read
`/proc/<manager>/task/*/children` instead of scanning every pid (orphans become the manager's children),
or have proot report forks.

Still failing on the tablet: **`dbus-broker-launch` exits `ENOENT`** (`launcher.c +1345`) after scanning the
service files, with no failing path in proot's trace or in the shim's `connect()` trace. Bluefin LTS also
ships `dbus-daemon`, and with `dbus.service` pointed at `dbus-daemon.service` (an override in
`XDG_CONFIG_HOME`, `dbus-broker.service` masked) everything above worked, including activation through
systemd. So for now: images with `dbus-daemon` run it under systemd; dbus-broker-only images
(CentOS bootc) need this chased down. The `Failed to acquire watch file descriptor` and `/proc/swaps`
messages are Android denying things systemd treats as optional.

Not yet known: GNOME itself (needs Selkie's compositor), startup time under load, and the same run with
the CI's Ubuntu proot (it has no `--kill-on-exit`; the probe step now omits it).

## What GNOME needs (cited unless noted)

| Component | Needs | Notes |
| --- | --- | --- |
| gnome-session ≥ 3.34 | systemd user instance: starts `gnome-session@*.target`, `org.gnome.*` units, monitors them | builtin fallback exists in 48, being removed (49 or later; sources disagree). Settle with `gnome-session --help` in the image |
| gnome-shell / apps | `StartTransientUnit` to launch apps in `app-*.scope` | inferred from GNOME 42+ behaviour; confirm by trace |
| mutter (native backend) | login1 `TakeControl`/`TakeDevice` | not needed for nested/headless backends; depends on how Selkie's compositor is attached |
| dbus-broker | systemd1 for all activation; journal socket | the repo already stubs the journal socket |
| GDM / AccountsService | userdb, accounts | skip GDM entirely; Selkie starts the session |
| polkit, upower, NetworkManager, power-profiles | system bus services; some systemd system units | DroidDeck replaced these with scripts and stand-ins |

**To pin down from the image, not this host** (this host has no GNOME stack): gnome-session version
and builtin option; `Type=` mix and sandbox directives across `gnome-session*.target` and
`org.gnome.SettingsDaemon.*`; `/usr/share/dbus-1/services/*` with `SystemdService=`; which system
units GNOME pulls in. Add an inventory step to `measure-bluefin.yml`, and capture
`busctl --user monitor org.freedesktop.systemd1` plus `busctl monitor org.freedesktop.login1` during
a real GNOME 48 login in a VM. The trace is the spec for any systemd1/login1 we reimplement.

## Options

### A. Real `systemd --user` on a cgroup shim (recommended to try first)

**Verified spike** ([`docs/spikes/cgshim`](spikes/cgshim)): hide the kernel cgroup tree behind a
plain tmpfs, preload a small shim into systemd, run `systemd --user`.

- Without the shim, systemd refuses to start: `Cannot determine cgroup we are running in: No medium
  found` / `Failed to allocate manager object` (verified). Without `/run/systemd/system` it refuses
  too (`system has not been booted with systemd`).
- With the shim it starts, reports `running`, loads user units, and `systemd-run --user` starts a
  transient service with `Main PID` and a cgroup path (verified).
- The shim does four things: report `CGROUP2_SUPER_MAGIC` from `statfs`/`fstatfs` under
  `/sys/fs/cgroup`; serve `0::/` for `/proc/self/cgroup`; create the `cgroup.*` files on `mkdir`
  (real cgroupfs does that; a plain directory does not); everything else is plain files.
- It starts without `/run/systemd/system`'s other contents, but wants a short `XDG_RUNTIME_DIR`
  (AF_UNIX path limit of 108 bytes bites in deep Android app dirs).

What the shim does **not** give you, and what to build:

| Gap | Effect | Mitigation |
| --- | --- | --- |
| No membership: `cgroup.procs` is a text file | stop/kill is by main PID only; double-forking daemons leak | init sets `PR_SET_CHILD_SUBREAPER`; attribute strays by `INVOCATION_ID` in `/proc/<pid>/environ` (same-uid, survives `setsid`); emulate `cgroup.kill` that way |
| No `populated` events | transient scopes (`app-*.scope`, sessions) never end | a helper updates `cgroup.events`; writing a real file produces real inotify events, so systemd's watcher works unchanged |
| No resource controls / PSI / BPF | `MemoryMax=`, `CPUWeight=`, oomd do nothing | accepted: harmless no-ops |
| `systemd --system` needs PID 1 | cannot run real system manager | our init handles system services |
| Namespaces for sandboxing | units with `PrivateTmp=`, `ProtectSystem=` fail `226/NAMESPACE` | see below; inferred, untested |

### B. Our own `org.freedesktop.systemd1` (+ login1) in Rust: "systemd for proot"

Needed if A fails on-device or proves too slow. Components: unit loader (drop-ins, templates,
specifiers, targets, ordering), zbus `systemd1` on **both** a system and a user bus (Manager, Unit,
Service, Scope objects, `JobRemoved`, `Subscribe`, environment setters, `StartTransientUnit`),
per-unit notify socket and `LISTEN_FDS` activation, timers/paths, login1 stub, process tracking as
above. Keeps the project MIT because only the public D-Bus interfaces are reimplemented.

Cost: unit-file semantics are the long tail; every GNOME release can add a property or method.
**Real systemd1 via A removes that tail**, which is why A goes first. Existing code is not a
shortcut: none of systemctl.py, rustysd, fake-systemd or initd serves the D-Bus API (verified/cited
by the first research pass). The "systemd-rs" project that claims a full rewrite could not be
verified (404) and should not be relied on.

### C. Swap the broker for `dbus-daemon`

Cheap, restores `Exec=` activation, but no session start via gnome-session's systemd mode, no login1,
and the broker is what the image ships (we would bind a separate `dbus-daemon`). Fixes point 1 only.

### D. gnome-session builtin mode plus stubs

Works only while the image's gnome-session has it (48 yes, later unclear) and does nothing for
activation. Do not build on it.

### E. Other service managers (dinit + turnstile, s6, rustysd, systemctl.py)

Chimera shows GNOME can run without systemd, but by rewriting units, replacing gnome-session and
patching around polkit (cited, LWN/OSNews). None implements systemd1, so dbus-broker activation and
anything calling `StartTransientUnit` still fail. Not viable for an *unmodified* Bluefin image.

### F. A real kernel (AVF microVM, QEMU)

Runs genuine systemd with no shims. Out of scope for a rootless-proot design and unavailable on most
devices, but it is the honest baseline: everything above is the price of staying in a plain app.

## Is proot the limiting factor?

Mostly no: **Android's app sandbox is the floor**, and proot is what makes working above it possible.

- Unprivileged apps cannot write cgroups, mount, or (inferred; verify per device) create user/mount
  namespaces. Any rootless approach hits that, with or without proot.
- Proot supplies the hook point: path translation, fake `/proc` files (DroidDeck binds several),
  syscall rewriting. The patched proot DroidDeck ships already carries 30 patches.
- Where proot *is* a limit:
  - **Shim reach.** `LD_PRELOAD` covers dynamic glibc binaries (systemd is one); static or raw-syscall
    callers need a proot patch. Prefer preload first, a proot hook for anything that escapes.
  - **No real PID 1 and host PIDs in the guest** (the subreaper point above).
  - **Speed.** ptrace stops slow systemd, dbus and gnome-shell. Measure startup before committing.
  - **Sandboxing directives.** Services with `PrivateTmp=`/`ProtectSystem=` need namespaces we do
    not have. Options: a drop-in that clears those options for units we generate, or proot rewriting
    `unshare`/`mount` to succeed as no-ops. The latter is **unverified** and risky for security
    semantics; the GNOME *user* units are mostly unsandboxed, system services are where it bites.
  - **Maintenance.** Every proot patch is a fork to carry.

The spike ran on x86 Linux with real user namespaces and a real `/proc`; it isolates the cgroup
question only.

## Plan

1. Fix `supervise`: set subreaper, start a **system** bus alongside the session bus, and a login1 stub
   (just what gnome-session/mutter ask for, from the trace).
2. Inventory the real image in CI (units, `SystemdService=` services, gnome-session version).
3. Port the cgroup spike into the guest: build `cgshim.so` into the bundle, run it under proot on a
   device, start `systemd --user` as a child of the init, and measure startup time and failures.
4. Add the helper that maintains `cgroup.events` / membership by `INVOCATION_ID`.
5. Decide A vs B from step 3's result. Only then start on a Rust systemd1.

## Sources

- [GNOME's systemd dependencies (Adrian Vovk)](https://blogs.gnome.org/adrianvovk/2025/06/10/gnome-systemd-dependencies/)
- [GNOME 49 will require deeper systemd integration (Linuxiac)](https://linuxiac.com/gnome-49-will-require-deeper-systemd-integration/)
- [LWN: GNOME and systemd dependencies](https://lwn.net/Articles/1025560)
- [dbus-broker wiki: Deviations](https://www.github.com/bus1/dbus-broker/wiki/Deviations)
- [systemd: Container interface](https://systemd.io/CONTAINER_INTERFACE/), [cgroup delegation](https://systemd.io/CGROUP_DELEGATION/)
- [Bluefin LTS docs](https://docs.projectbluefin.io/lts/)
- [rustysd feature comparison](https://github.com/KillingSpark/rustysd/blob/master/feature-comparison.md)
- [docker-systemctl-replacement](https://github.com/gdraheim/docker-systemctl-replacement)
- [Chimera Linux GNOME](https://chimera-linux.org/docs/configuration/gnome)

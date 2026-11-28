# A systemd stand-in for droidc, shipped as a shim image

Design note, 2026-11-28. Status: accepted the same day (see "Decisions"); building, starting with the
unit loader. It follows on from
[`docs/systemd-on-proot.md`](../systemd-on-proot.md) (the evaluation, "Option B") and replaces that
note's step 5 ("decide A vs B") with a concrete plan.

Evidence is labelled as in that note: **verified** (read in droidc's code or run here), **cited** (a
source says so, not checked against a binary) or **inferred**.

Guiding rule, in Kepler's words: "We should adhere to existing Linux standards wherever possible, like
XDG and LSB". Here that means: the image's own unit files are the spec of what to run, systemd's
documented D-Bus APIs and protocols are the spec of how, and droidc adds nothing an unmodified client
would have to know about.

## The idea

`droidc supervise` is a fake PID 1 that starts a user session. Instead of growing it one workaround at a
time, droidc gets a **stand-in for systemd itself**: a manager that reads the image's real unit files,
walks the target graph from `default.target` the way systemd does, and serves systemd's public
interfaces (`org.freedesktop.systemd1` on the bus, `sd_notify`, `LISTEN_FDS`) to everything it starts.
Distros already describe their services as units, so when a new service turns up it is started from its
own unit file, not from a new branch in `supervise.rs`.

The stand-in and the other guest-side pieces (cgshim, ifshim, the guest droidc) ship as one **shim OCI
image** that the launcher binds into the guest over the paths the bootc image expects. The bootc image
itself stays untouched and verifiable.

## What it replaces (verified, from `main` at ebf6715)

Today the guest has no system manager at all and a real `systemd --user` that only runs through an
`LD_PRELOAD` shim. Each item below is a workaround in droidc's code for something a system manager
would do from the image's units:

| Workaround today | Where | What a stand-in does instead |
| --- | --- | --- |
| Hand-written system bus config; only `org.freedesktop.Flatpak.SystemHelper` is activatable, with `User=` and `SystemdService=` stripped from a copy of its `.service` file | `supervise.rs` `system_bus_config`, `SYSTEM_ACTIVATABLE` | The image's `dbus.service` (system) runs as a unit; `SystemdService=` activation goes to the stand-in's `systemd1`, as on any host |
| No system units run at all; system services are started one by one (`droidc login1`, `droidc polkit`) | `supervise.rs` `start_system_services`, `start_standin` | System units run from `/usr/lib/systemd/system`; droidc's stand-ins become ordinary units the shim image ships |
| No `systemd-tmpfiles`, `systemd-sysusers`: `/var/log`, `/var/lib/flatpak` missing (#48) | not done | `systemd-tmpfiles-setup.service` and `systemd-sysusers.service` run from their own units, using the image's own binaries |
| No `hostname1`, `timedate1`, `locale1` (#51) | not done | The image's `systemd-hostnamed` and friends are bus-activated units; whether they run as the app uid is to be tried (inferred) |
| Journal sockets bound by droidc, everything dropped; `journalctl` empty (#50) | `journal.rs` | The image's own `systemd-journald` runs as a unit, so `journalctl` reads real journal files. droidc's socket stand-in stays only as the fallback when journald won't start, so logs are never dropped silently |
| `XDG_SESSION_ID=1`, `XDG_SEAT=seat0` set by hand before the buses start | `supervise.rs` `start_user_services` | login1 creates the session and the user manager gets it from `user@.service`, as pam_systemd does |
| A cgroup2 filesystem faked by `LD_PRELOAD`: `statfs` magic, `/proc/<pid>/cgroup`, `cgroup.*` files on `mkdir`, `DT_LNK` to `DT_UNKNOWN`, `PR_SET_SECUREBITS` and seccomp reported as done, `/proc/self/fd` exec, `POSIX_SPAWN_SETCGROUP`, `creat()` | `crates/cgshim` | Not needed: the stand-in never uses cgroups (only if it also runs the user instance, see "Two instances") |
| `/proc` polled every 200 ms to fake membership, `populated` and `cgroup.kill`; ~300 ms a tick under proot, so double-forked daemons sometimes escape | `cgroups.rs` | Process tracking is the manager's own: subreaper, `pidfd`s, `INVOCATION_ID` for strays (same idea, no fake filesystem in between) |
| `dbus.service` and `dbus-broker.service` aliased to `dbus-daemon.service` because `dbus-broker-launch` exits `ENOENT` | `supervise.rs` `prefer_dbus_daemon` | Unchanged at first (the cause is still unknown); with a stand-in `systemd1` on the bus, dbus-broker's activation path at least has someone to call |
| `droidc-graphical-session.target` written into `$XDG_RUNTIME_DIR` at start | `supervise.rs` `write_graphical_target` | Shipped as a unit in the shim image |
| Stale manager state cleared, preload probed and retried (the shim sometimes fails to map on the emulator) | `supervise.rs` `clear_stale_manager_state`, `probe_preload`, `shim_mapped` | Gone with the shim, for the instance the stand-in runs |

What it does **not** replace, so the whack-a-mole doesn't vanish entirely:

- Stand-ins for hardware Android owns: login1's seat, UPower, NetworkManager connectivity, and the
  planned ones (#79-#83, #85, #96, #101). They still need code, but they become units in the shim image
  instead of children hard-wired into `supervise`.
- The PipeWire masking (#66): its cause is PipeWire's Flatpak check reading `/proc/<pid>/root`, not
  systemd. A drop-in in the shim image can carry the environment PipeWire needs instead of masking.
- `sudo`/`su`/`pkexec`/`bwrap` and ifshim: unrelated to service management.
- dbus-broker's `ENOENT`: still to be chased.

## Two instances

systemd runs as two managers: the system instance (PID 1) and one user instance per user
(`user@UID.service`). They are the same program and the same unit logic with different search paths
(cited, `systemd(1)`, `systemd.unit(5)`).

- **System instance: build it.** Nothing runs here today, and real systemd can't, because it insists on
  being PID 1 with real cgroups and mounts (verified in the earlier note). This is where the stand-in
  closes the most gaps (#48, #50, #51, the system bus).
- **User instance: real `systemd --user` on cgshim works today** (verified: Selkie's Emulator GUI job runs
  Bluefin LTS's GNOME 50 under it and passes). The stand-in can run the user instance too, which is what
  retires cgshim and `cgroups.rs`.

Decided: one engine, both instances, like systemd. Ship the system instance first. Make the user
manager selectable (`--user-manager=systemd|droidc`, keeping today's `--systemd` for the real one), and
flip the default once the stand-in passes the same Emulator GUI job and the tablet. That keeps a working
session on `main` the whole way.

## What the stand-in serves

The spec is what clients actually call, not the whole of systemd. The earlier note's tracing plan still
holds: record `busctl monitor org.freedesktop.systemd1` and `org.freedesktop.login1` during a real GNOME
login in a VM, and treat that trace as the test. What follows is the expected minimum (inferred from
GNOME's and libsystemd's use, to be checked against the trace).

### `org.freedesktop.systemd1` (both instances)

Documented in `org.freedesktop.systemd1(5)`; systemd lists its D-Bus APIs as stable interfaces (cited,
systemd.io "Interface Portability and Stability").

- **Manager:** `StartUnit`, `StopUnit`, `RestartUnit`, `ReloadUnit`, `TryRestartUnit`, `KillUnit`,
  `ResetFailed(Unit)`, `GetUnit`, `LoadUnit`, `GetUnitByPID`, `GetUnitByInvocationID`, `ListUnits`,
  `ListUnitsByPatterns`, `ListUnitFiles`, `StartTransientUnit` (for `.scope` with `PIDs` and `.service`
  with `ExecStart`), `Subscribe`/`Unsubscribe`, `SetEnvironment`, `UnsetEnvironment`,
  `UnsetAndSetEnvironment` (gnome-session and `dbus-update-activation-environment --systemd`),
  `Reload`, `Version`, `SystemState`, `Environment`; signals `UnitNew`, `UnitRemoved`, `JobNew`,
  `JobRemoved`, `StartupFinished`.
- **Unit, Service, Scope, Target:** `Id`, `Names`, `Description`, `LoadState`, `ActiveState`,
  `SubState`, `UnitFileState`, `FragmentPath`, `InvocationID`, timestamps, `MainPID`, `ControlGroup`
  (a stable fake path), `Result`, with `PropertiesChanged`.
- **Job:** objects for queued jobs, so `systemctl start` can wait.
- The private socket `/run/systemd/private` (and `$XDG_RUNTIME_DIR/systemd/private`), which `systemctl`
  uses as root instead of the bus (cited, `systemctl(1)` behaviour; inferred to matter under proot's
  fake root).

If this is served faithfully, the image's own `systemctl`, `systemd-run`, `busctl` and `loginctl` work
unmodified, because they are D-Bus clients. So the shim image should **not** replace them; that
would be one more thing to keep in step with systemd.

### `org.freedesktop.login1` (system instance)

`droidc login1` already serves one seat, one session and one user (`login1.rs`). It moves into the
manager or stays a separate unit; either way it gains what #49 lists (`ListSessionsEx`, `GetAll` with an
empty interface) and starts `user@UID.service` for the session's user, as logind does.

### Which target, and how droidc adjusts it

The stand-in reaches the image's own `default.target` (`graphical.target` on Bluefin), as systemd does.
Units that can't run here are not edited in the image; droidc changes them only through the standard
override points (cited, `systemd.unit(5)`):

- **Drop-ins** (`<unit>.d/*.conf`) shipped in the shim image under `/usr/lib/droidc/units/system` and
  `/usr/lib/droidc/units/user`, for units that need a different environment, ordering or `ExecStart`
  here. Example: the PipeWire environment from #66 instead of a mask.
- **Masks** (`/dev/null` symlinks) and per-start drop-ins written to `/run/systemd/system` and
  `/run/systemd/user` at start, for units that must not run at all (GDM, whose job droidc's session
  already does; hardware services with no hardware). Many hardware units already skip themselves through
  `ConditionVirtualization=` once the stand-in answers `container` (inferred; the inventory will show
  how many).

The shim image's list of drop-ins and masks is the record of everything droidc changes about an image's
boot, in one reviewable place.

### Units: phase 1

From the image's unit search path (`/etc/systemd/system`, `/run/systemd/system`,
`/usr/lib/systemd/system` and the user equivalents, cited `systemd.unit(5)`), with drop-ins
(`*.d/*.conf`), `.wants/` and `.requires/` directories, aliases, templates (`foo@.service`), masks
(`/dev/null` symlinks) and presets.

- Types: `.service`, `.target`, `.scope` (transient only), `.slice` (accepted, no effect).
- Service `Type=`: `simple`, `exec`, `forking` (with `PIDFile=`), `oneshot` (with
  `RemainAfterExit=`), `notify` and `notify-reload`, `dbus` (with `BusName=`).
- Dependencies and ordering: `Wants`, `Requires`, `Requisite`, `BindsTo`, `PartOf`, `Conflicts`,
  `Upholds`, `OnFailure`, `OnSuccess`, `After`, `Before`, `WantedBy`/`RequiredBy` (for enable),
  `StopWhenUnneeded`, `RefuseManualStart`, `RefuseManualStop`, `DefaultDependencies`. gnome-session's
  targets lean on `BindsTo`, `PartOf` and `Requisite` (inferred; confirm in the inventory).
- Jobs: a transaction per request (start/stop/restart, `replace`/`fail`/`isolate` modes), ordered by
  `After`/`Before`, cycles broken and logged as systemd does.
- Exec: `ExecStart`, `ExecStartPre`, `ExecStartPost`, `ExecStop`, `ExecStopPost`, `ExecReload`, the
  `-`, `@`, `:` prefixes, `Environment`, `EnvironmentFile`, `PassEnvironment`, `UnsetEnvironment`,
  `WorkingDirectory`, `StandardInput/Output/Error` (null, inherit, journal, file), `Restart=` with
  `RestartSec`, `TimeoutStartSec`/`TimeoutStopSec`, `KillMode`, `KillSignal`, `SendSIGKILL`,
  `SuccessExitStatus`, `NoNewPrivileges` (honoured with `prctl`), specifiers (`%i %I %n %N %p %h %t %u
  %U %S %C %L %E`).
- Conditions and asserts: `ConditionPathExists*`, `ConditionPathIsDirectory`, `ConditionFileNotEmpty`,
  `ConditionUser`, `ConditionGroup`, `ConditionEnvironment`, `ConditionVirtualization` (answers
  `container` with `container=droidc`, as the container interface says, cited systemd.io
  CONTAINER_INTERFACE), `ConditionKernelCommandLine` (empty), `ConditionFirstBoot`, `ConditionNeedsUpdate`.
- Protocols: `sd_notify` on `$NOTIFY_SOCKET` (`READY=1`, `RELOADING=1`, `STOPPING=1`, `STATUS=`,
  `MAINPID=`, `EXTEND_TIMEOUT_USEC=`, `FDSTORE=` accepted), `NotifyAccess=`, `$INVOCATION_ID`,
  `$MANAGERPID`, `$SYSTEMD_EXEC_PID`.
- Generators: run the image's generators (`/usr/lib/systemd/system-generators`, user ones) into
  `/run/systemd/generator*`, as systemd does; skip and log those that need what Android won't give.

### Later

- Socket activation (`.socket`, `LISTEN_FDS`/`LISTEN_FDNAMES`, `Accept=`). Needed for the image's own
  `dbus.socket` and `pipewire.socket` once the user instance is ours, so it is the first "later" item.
- Timers (`.timer`, `OnCalendar`, `OnUnitActiveSec`), paths (`.path`).
- `systemd-run` transient services with properties beyond `ExecStart`.
- `.mount`, `.automount`, `.swap`, `.device`: accepted and reported `active` or `inactive` from
  `/proc/self/mountinfo`; never mounted (an app can't).

### Never (logged, see below)

Sandboxing (`Protect*`, `Private*`, `RootDirectory`, `BindPaths`, `ReadOnlyPaths`, namespaces),
`SystemCallFilter`, capabilities (`CapabilityBoundingSet`, `AmbientCapabilities`), resource control
(`Memory*`, `CPU*`, `IO*`, `Tasks*`, `Delegate`), `User=`/`Group=` other than the app uid and fake root
(see "Decisions"), `DynamicUser`. These either need namespaces and cgroups Android won't give an app,
or protect against threats the app sandbox already covers. Today cgshim reports seccomp as installed and
nothing as failing; the stand-in does the same, but says so.

## Logging what is ignored

Every unit option the stand-in parses and does not honour is reported once per unit at load, at
`notice` level, to the journal stand-in (so `journalctl -u` shows it) and to stderr when
`DROIDC_JOURNAL=stderr`, in one form:

```
foo.service: ignoring ProtectSystem=strict (no mount namespaces under droidc)
```

Unknown options are reported the same way, with the line number, as systemd itself warns. `droidc
status --json` gets a count of units with ignored options, and `systemctl show -p` of an ignored
property returns its parsed value, so tools that read it don't break. A strict mode
(`DROIDC_SYSTEMD_STRICT=1`) fails units that use ignored sandboxing options, for testing.

## Process tracking without cgroups

Reuses what `cgroups.rs` and `supervise.rs` already learned, without the fake filesystem:

- The manager is a child subreaper (`PR_SET_CHILD_SUBREAPER`), so orphans come to it.
- Each process it starts gets `$INVOCATION_ID`; strays are matched by that, read once from
  `/proc/<pid>/environ` (same uid). Orphans are found from `/proc/<manager>/task/*/children`, not by
  scanning all pids (the earlier note's idea for the ~300 ms tick under proot).
- `pidfd_open` for the main and control processes, where the kernel and proot allow it (inferred; Android
  12+ kernels are 5.10 or later), else `waitpid` on our own children.
- `ControlGroup` and `/proc/<pid>/cgroup` are answered from the manager's own table where clients ask
  through `GetUnitByPID`; nothing writes a cgroup tree.

The same hole as today remains: a daemon that clears its environment and double-forks escapes.

## Writing it: Rust in droidc, not an adopted project

**Prior art** (none of it runs unmodified GNOME here):

| Project | What it shows | Why not adopt |
| --- | --- | --- |
| [docker-systemctl-replacement](https://github.com/gdraheim/docker-systemctl-replacement) (`systemctl.py`) | Unit files can drive a container's PID 1 without systemd; it runs as init and starts enabled units (cited, its README) | No D-Bus API, so no `systemd1` for GNOME or dbus-broker; Python, which a bootc image need not ship |
| Ubuntu's `systemd-shim` (14.04 to 16.04) | A partial `org.freedesktop.systemd1` was enough for logind under Upstart | Its bug tracker is a record of chasing missing methods (`Subscribe`, abandoning scopes, unknown `user@.service`) as logind changed (cited, Launchpad bugs 1348039, 1363134, 1359439). The warning for us: test against a trace, keep the supported list explicit |
| [elogind](https://github.com/elogind/elogind) | login1 can live apart from systemd | Needs root and real cgroups; a reference for login1 semantics, not code to run |
| rustysd | A Rust service manager that reads unit files, with socket activation | No D-Bus API (cited, its feature comparison); unmaintained as far as the earlier note found |
| genie / bottle-imp, subsystemctl, wsl-distrod | Real systemd in a PID namespace on WSL | Needs namespaces, which Android apps don't get. WSL later added native systemd, which is the "real kernel" baseline (Option F in the earlier note) |
| Chimera Linux (dinit + turnstile), DroidDeck | GNOME without systemd, by rewriting units or replacing services with scripts | Not unmodified images; what we're trying not to do |

**Recommendation: write it in Rust inside droidc** (inferred from the above and the code):

- The guest droidc is already a static musl binary bound into the guest, and already speaks D-Bus with
  `zbus` (`login1.rs`, `polkit.rs`, `upower.rs`, `network.rs`). A static PID 1 doesn't depend on the
  image's libc, so it needs no preload and survives images droidc has never seen.
- Keep droidc MIT: implement from the man pages and D-Bus documentation, not from systemd's LGPL source.
  Check licences before borrowing anything from the projects above.
- A new crate in the droidc workspace (decided; working name `crates/droidc-systemd` until the name is
  picked), broken out into its own repository later only if that turns out to be warranted. It has no
  Android-specific code, so it can be tested on a plain Linux CI runner against real unit files and, as a **differential test**, against real systemd
  running the same units in a container with namespaces: same transactions, same `ActiveState`
  sequence, same `systemctl show` output for the supported properties.

## The shim image

The guest pieces droidc brings become one OCI artifact, bound in at launch.

### What it contains and where it binds

| Path in the guest | What | Today |
| --- | --- | --- |
| `/usr/lib/systemd/systemd` and `/usr/sbin/init` | the stand-in (system instance; the user instance too once selected, since `user@.service` runs `/usr/lib/systemd/systemd --user`) | not bound |
| `/usr/bin/droidc` | the guest droidc | bound by the launcher (`libdroidc-guest.so`) |
| `/usr/bin/sudo`, `/usr/bin/su`, `/usr/bin/pkexec`, `/usr/bin/bwrap` | droidc by `argv[0]` | bound by the launcher (the shell spec's `shims`) |
| `/usr/lib/droidc/` | `cgshim.so` (while the real user manager is used), `ifshim.so` | provided by Selkie |
| `/usr/lib/droidc/units/system`, `/usr/lib/droidc/units/user` | droidc's own units: login1, polkit, upower, network stand-ins, `droidc-graphical-session.target`, drop-ins for image units that need different environment here | generated at runtime or hard-wired |
| `/run/systemd/system`, `/run/systemd/user` | runtime drop-ins and masks the stand-in writes at start | written into `$XDG_RUNTIME_DIR/systemd/user` |

The image's `systemctl`, `loginctl`, `busctl`, `systemd-run`, `systemd-tmpfiles` and `systemd-sysusers`
stay the image's own (see above). This differs from the first sketch, which bound over `systemctl` and
`loginctl` too: they are D-Bus clients, and serving the API is the standard way to make them work.

Units in the shim image reach the manager through the standard search path, not by writing into the
image's `/usr`: the stand-in adds `/usr/lib/droidc/units/*` to its path the way systemd honours
`$SYSTEMD_UNIT_PATH` (cited, `systemd.unit(5)`; a trailing `:` keeps the defaults). Overrides of image
units are drop-ins in `/run/systemd/*`, which outranks `/usr/lib` (cited, same page).

### No overlayfs: binds only

Without user namespaces there is no overlayfs and no merged layer (verified for Android apps in the
earlier note). Shims are proot (or bwrap) binds of single files and directories. Binds replace binaries
fine, but two artifacts can't both add files to the same directory: a bind over `/usr/share/vulkan/icd.d`
hides the image's own files there.

So every artifact follows one rule: **own a directory under `/usr/lib/droidc/<artifact>/`, bind single
files at standard paths, and reach shared directories through the search paths their loaders already
have.** Each standard already offers one:

- systemd units: the unit search path above.
- D-Bus services: a `<servicedir>` or `<includedir>` in the bus config, or `XDG_DATA_DIRS`
  (`dbus-1/services` is looked up there for the session bus, cited, D-Bus specification).
- Vulkan drivers (#108): `VK_ADD_DRIVER_FILES` adds ICD JSON files on top of the system ones (cited,
  Vulkan loader docs); glvnd EGL: `__EGL_VENDOR_LIBRARY_DIRS` (replaces, so list the image's directory
  too) (cited, libglvnd docs).
- Desktop entries, icons, schemas: `XDG_DATA_DIRS`, per the XDG Base Directory spec.
- Libraries: `RPATH`/`LD_LIBRARY_PATH` scoped to the units that need them, never a global bind over
  `/usr/lib64`.

Where no search path exists, droidc can build a merged view itself at unpack time: a directory under
`/run/droidc/merged/<path>` of symlinks to the image's files plus the artifact's, bound over the image
path. That is a last resort, because it hides later changes the image makes to that directory until the
next start.

### One "layer an artifact into the guest" path

This shape is shared with two other planned artifacts, so droidc gets one mechanism for all three:

- **GPU drivers** (#108): capability annotations pick the artifact; its files land under
  `/usr/lib/droidc/gpu-<id>/` and are reached through the loader variables above.
- **droidc itself** (#126, keplersj/Selkie#157): the guest droidc, `cgshim.so` and `ifshim.so` are
  already listed there as part of the engine artifact. The shim image is the guest half of that
  artifact, or the same artifact with a guest layer. Its compatibility annotation should carry the
  emulated systemd interface version, next to the host-link protocol version.
- **The shim image** (this note).

Mechanism (inferred, to be settled in #126): each artifact is an OCI artifact manifest with an
`artifactType` under the droidc-dev namespace, standard `org.opencontainers.image.*` annotations, verified
with the same trust policy as images (#62), unpacked once per digest under `<root>/artifacts/<digest>/`,
and carrying a small `binds.json` (guest path, artifact path, file or directory) plus the environment it
adds. The launcher's shell spec, which already has `shims` and `systemd.binds`, gets those binds and
environment from the artifacts the container uses instead of from code. Artifact binds may only target
paths under `/usr/lib/droidc/`, the explicit init paths, and the four spawner shims, and a clash between
two artifacts on one path is an error at start, not a silent win.

## Plan

1. Inventory the image (the earlier note's step 2, `measure-bluefin.yml`): every unit in the default and
   graphical targets with its options, so the phase 1 list is checked, not guessed. Record the
   `systemd1`/`login1` traces of a real GNOME login in a VM.
2. New crate: unit loader (search path, drop-ins, templates, specifiers, conditions), with tests against
   the inventory's real units. Log ignored options from the start.
3. Job engine and process supervision (types, dependencies, ordering, transactions, restart, notify),
   tested on Linux CI, then differentially against real systemd in a container.
4. `systemd1` on the system bus, plus `/run/systemd/private`. Run it as the system instance in place of
   `start_system_services`, with login1, polkit and the host-link stand-ins as units, and the image's
   `systemd-journald` (on the fake-root allow-list). Closes #48 and #50, and likely #51.
5. Shim image: build it in droidc's release, bind it through the shell spec's existing lists, then move
   those lists to the shared artifact path when #126 lands.
6. User instance: run the stand-in as `user@.service`, add socket activation, pass the Emulator GUI job
   and the tablet with it, then make it the default and retire cgshim and `cgroups.rs`.

## Decisions

Settled by Kepler on 2026-11-28, answering this note's open questions:

1. **User instance later.** The system instance comes first; the user manager is selectable and its
   default flips after the Emulator GUI job and the tablet pass with the stand-in.
2. **System units that want root** run as the app uid by default. A short allow-list of units runs under
   proot's fake root through the spawner (`spawn.rs`), starting with `systemd-journald`,
   `systemd-tmpfiles-setup` and `systemd-sysusers`. Anything else that needs a real `User=` is skipped
   and logged.
3. **Target:** the image's `default.target`, adjusted only with drop-ins and masks (see "Which target,
   and how droidc adjusts it").
4. **Journal:** the image's real `systemd-journald`. Phase 1 starts it as a plain service (it opens its
   own sockets when none are passed, inferred, to check in step 4); socket activation comes with the
   other `.socket` work. droidc's socket stand-in remains the fallback.
5. **Crate:** a new crate in the droidc workspace, broken out later if reasonable.

Still open:

6. **Name.** Kepler's candidate is `dsystemd`; he also likes a seal name to match the images. The crate
   uses the working name `droidc-systemd` until one is picked.

## Sources

- [`docs/systemd-on-proot.md`](../systemd-on-proot.md), and its sources
- `systemd(1)`, `systemd.unit(5)`, `systemd.service(5)`, `systemd.exec(5)`, `org.freedesktop.systemd1(5)`,
  `org.freedesktop.login1(5)`, `sd_notify(3)`, `sd_listen_fds(3)`
- [systemd: Interface Portability and Stability](https://systemd.io/PORTABILITY_AND_STABILITY/)
- [systemd: Container Interface](https://systemd.io/CONTAINER_INTERFACE/)
- [docker-systemctl-replacement](https://github.com/gdraheim/docker-systemctl-replacement)
- Ubuntu systemd-shim bugs: [1348039](https://bugs.launchpad.net/ubuntu/+source/systemd-shim/+bug/1348039),
  [1363134](https://bugs.launchpad.net/bugs/1363134), [1359439](https://bugs.launchpad.net/bugs/1359439)
- [elogind](https://github.com/elogind/elogind)
- [genie](https://github.com/arkane-systems/genie), [bottle-imp](https://github.com/arkane-systems/bottle-imp),
  [subsystemctl](https://github.com/sorah/subsystemctl)
- [D-Bus Specification](https://dbus.freedesktop.org/doc/dbus-specification.html)
- [Vulkan loader: driver interface](https://github.com/KhronosGroup/Vulkan-Loader/blob/main/docs/LoaderDriverInterface.md),
  [libglvnd EGL ICD loading](https://gitlab.freedesktop.org/glvnd/libglvnd/-/blob/master/src/EGL/icd_enumeration.md)
- [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/latest/)

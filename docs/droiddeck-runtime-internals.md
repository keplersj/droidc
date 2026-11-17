# DroidDeck runtime internals

Phase 0 recon for droidc: how DroidDeck builds, ships, installs, updates and runs its Linux runtime,
and what that means for running generic bootc images.

Read at [Droid-Deck/DroidDeck@6fe7b4e](https://github.com/Droid-Deck/DroidDeck/tree/6fe7b4ec1109169c71c10b77c648eb649ee97423)
(2026-11-16). Paths below are relative to that tree. DroidDeck is GPL-3.0; droidc is MIT, so this
note records design, not code to copy.

## Summary

- The runtime is a plain rootfs tarball (Ubuntu 26.04 arm64), not an OCI image. Hardware support is
  **baked into it**: a rebuilt Mesa, a patched KWin and DD-Turnip replace Ubuntu's own packages.
- The app then **writes ~100 more files into the tree** at every session start (shims, preloads,
  gamescope, Vulkan layers). There is no separation between "image" and "hardware layer".
- There is **no systemd**. A 2,000-line bash session script starts dbus, a fake login1/UPower, a
  NetworkManager shim and a bundled PipeWire by hand.
- Updates are staging + rename with crash recovery, but no retained rollback, no `/etc` merge and no
  signature check on device (SHA-256 from a catalog fetched over HTTPS).
- File ownership is **not preserved** at all. Sessions run as the app's own uid; root-only work goes
  to a separate fake-root proot through a socket.

For droidc the big shift is the first two points: a bootc image can't be rebuilt per device, so the
hardware layer has to live outside the image.

## 1. Build

| Step | Where | What happens |
| --- | --- | --- |
| Custom debs | `tools/ubuntu/build-debs.sh`, `.github/workflows/build-ubuntu-runtime.yml` | In an `ubuntu:26.04` container on an arm64 runner: Mesa rebuilt with `-Dfreedreno-kmds=msm,kgsl` (so Turnip/freedreno reach Android's `/dev/kgsl-3d0`) plus KGSL UBWC and Zink patches; KWin rebuilt with nested-desktop patches. Both versioned `+droiddeck`. |
| Base | `tools/ubuntu/build.sh`, `base.env` | Downloads `ubuntu-base-26.04.1-base-arm64.tar.gz`, verifies cdimage's signed `SHA256SUMS` against a pinned key and a pinned hash. |
| Runtime stage | `tools/ubuntu/setup.sh runtime` | Chroot install of `packages-runtime.txt`; local apt repo with the `+droiddeck` debs pinned at 1001 and held; DD-Turnip (`DD_TURNIP_URL`) installed over Ubuntu's `libvulkan_freedreno.so` via `dpkg-divert`; `sudo`/`su` replaced; a `Protected: yes` meta-package (`droiddeck-runtime`) owns DroidDeck's files and diverts any Ubuntu file it replaces. Boot-only packages (kernel, grub, initramfs, plymouth) pinned to -1. |
| Desktop stage | `setup.sh desktop`, `desktop-pack.py` | Kubuntu Plasma installed on top; only the delta is packed (`files.tsv.gz` of added/changed/removed paths plus `originals/`), so the desktop is removable. |
| Publish | workflow, `tools/release/` | `<tag>.tar.zst` and `<tag>-desktop.tar.zst` (zstd -19) released to `Droid-Deck/DroidDeck-Components`; `linuxfs-ubuntu.json` on that repo's `main` names the current release (`version`, `url`, `sha256`, `size`). Published tags are never replaced. |

## 2. Install and update

`app/src/main/java/com/droiddeck/launcher/runtime/LinuxRuntimeInstaller.java`

1. Fetch the catalog from `raw.githubusercontent.com/Droid-Deck/DroidDeck-Components/main/linuxfs-ubuntu.json`.
2. Download the tarball into the cache dir and check its SHA-256 against the catalog. No signature.
3. Extract into `linuxfs.new` with commons-compress (zstd + tar):
   - Ownership, uid/gid, setuid bits and xattrs are dropped; only the executable bit survives.
   - Hard links: `Files.createLink`, and when Android's SELinux denies it, proot's link2symlink
     layout (`.l2s.<name>NNNN` chains), falling back to a copy.
   - Symlinks are created last; entries escaping the root are skipped.
4. Swap: current tree to `linuxfs.old`, carry user paths across by rename, `linuxfs.new` to
   `linuxfs`, then delete `linuxfs.old`. A marker file lets an interrupted swap recover on the next
   start. **The old tree is deleted, so there is no rollback after a successful swap.**
5. User data carried (`RuntimeFileTree.userPaths`): `/root` (the only home), AppImages, added apps
   and Windows components. **`/etc` is not carried**; instead an apt hook
   (`tools/ubuntu/overlay-runtime/usr/share/droiddeck/apt/remember`) records user-added packages,
   sources and keys, and `restore` replays them after an update.
6. `RuntimeAutoUpdate` retries a new catalog release up to 3 times, 10 minutes apart; per the 0.6.9
   release notes, updates never run during a game or desktop session.

## 3. Launch

`LinuxRuntime.java`, `RootSpawner.kt`, `tools/proot/`

- proot is Termux's `4dba3afb` plus 30 patches (`tools/proot/PATCHES.md`), shipped as an add-on
  pack (`libproot.so` run by Android's linker). Notable patches: seccomp filtering by argument to cut
  stops, `clone3` handling, `/proc/self` thread-group fixes, hard-link and exec-stack denials turned
  into errors that Flatpak/ostree/ld.so fall back from, and an in-process path fast path.
- Normal sessions: `proot --kill-on-exit --kernel-release=… -i <app uid>:<app uid> -r linuxfs -w /root`.
  The guest runs as the app's uid, not fake root.
- Root work: `sudo`, `su`, `pkexec` and apt are shims that call `droiddeck-root`, which asks the app
  over an abstract socket to run the command in a second proot with `-0 --link2symlink`
  (fake root, hard-link emulation). PackageKit and polkit are started the same way.
- Binds: `/dev`, `/proc`, `/sys`, the app's files/cache dirs, external storage, a tmpfs-like
  `/dev/shm`, system fonts; an empty dir over `/sys/fs/selinux`; and fake `/proc` files
  (`stat`, `version`, `loadavg`, `uptime`, `vmstat`, `cap_last_cap`, PCI devices…) from
  `tools/ubuntu/proc` where Android hides the real ones.

## 4. The integration layer written into the tree

`app/src/main/java/com/droiddeck/launcher/session/SessionFiles.kt`, `stage()`

At every session start the APK copies its assets into the rootfs, tracked by a ledger keyed on the
APK identity: about 100 fixed entries (84 of them `droiddeck-*` scripts and libraries, mostly in
`/usr/local/bin`), `LD_PRELOAD` libraries
(`libblsession.so`, `libfakeinput.so`, `libblfastpath.so`…), a patched `gamescope`, gamescope's WSI
layer, implicit Vulkan layers (GPU load, VRAM cap, VR, Mali compat), FEX preloads, mangoapp, and a
PipeWire bundle. It also overrides `systemd-run` with a shim. Several files land under `/usr/lib`
and `/usr/share/vulkan`, not only `/usr/local`.

## 5. Session services (no systemd)

`tools/linuxfs/overlay/usr/local/bin/`

| Need | DroidDeck's answer |
| --- | --- |
| Init | `droiddeck-session` (2,084 lines of bash) as the session root, launched by the app. |
| System bus | `dbus-daemon --config-file=/usr/local/share/droiddeck/system-bus.conf`. |
| Session bus | `dbus-daemon --session` on `$XDG_RUNTIME_DIR/bus`. |
| logind / UPower | `droiddeck-login1`, a Python/Gio service exposing just `Suspend`, `CanSuspend`, `PrepareForSleep` and UPower battery, wired to Android pause/resume. |
| Network | `droiddeck-netmanager`, a NetworkManager stand-in. |
| PipeWire | `droiddeck-pipewire`: unpacks DroidDeck's own PipeWire build, starts `pipewire` and `wireplumber --profile video-only` with `LD_LIBRARY_PATH`, access control off. Audio goes to Android through the DirectAudio PulseAudio sink. |
| `systemd-run --user` | `droiddeck-systemd-run` runs the command directly (fork + setsid). |
| Compositor | An in-app Wayland compositor in C (`app/src/main/cpp/waylandcomp`); gamescope and a patched KWin run nested on it. Desktop compositing is software rendered. |

## Implications for droidc

1. **The hardware layer must move out of the image.** DroidDeck can rebuild Mesa and KWin because it
   owns its distro. Bluefin's Mesa is built without the KGSL kmd, so stock EGL/GBM won't reach
   `/dev/kgsl-3d0`. Vulkan can come from a bind-mounted DD-Turnip plus an ICD JSON, and GL likely
   has to go through the image's Zink on top of it. Buffer sharing with the in-app compositor is the
   unknown to prove first. This is the highest-risk item for Phase 3.
2. **Bind-mount, don't write into deployments.** DroidDeck's ledgered copy into `/usr` would break
   digest verification and swap-by-digest. droidc should present its bundle (shims, preloads,
   gamescope, layers) as proot binds over an otherwise untouched deployment.
3. **Ownership DB may not be needed for v1.** DroidDeck drops ownership entirely and gets by, because
   sessions run as the app uid and package changes go through a fake-root side channel. Bootc images
   are meant to be immutable (users add software via Flatpak and Homebrew, not `dnf`), so droidc can
   skip the metadata DB at first and add it only if a setuid or capability-dependent path matters.
4. **Hard links are the real storage issue.** Android denies them to apps; image layers and any
   ostree/composefs content use them heavily. Unpack must use link2symlink (as DroidDeck does) or
   copy, and should measure the size cost on a Bluefin LTS image.
5. **Updates: keep their crash safety, add what bootc has.** Reuse the staging/marker/rename pattern,
   and add a retained previous deployment, a real 3-way `/etc` merge, and signature verification on
   device. Pull by digest instead of a mutable catalog file.
6. **The supervisor already has a reference implementation.** DroidDeck shows exactly which services
   a session needs without systemd. GNOME will need more than Plasma did (gnome-session, gsd-*,
   more logind surface), so the `systemd --user` spike in Phase 2 matters; if it fails, the shim set
   grows.
7. **proot is reusable as a separate binary.** Their patched proot ships as its own pack and is
   GPL-2; running it as a separate process from MIT droidc is fine. Kotlin installer code is GPL-3
   and should be reimplemented, not copied.

## Open questions this raises

- Can stock Fedora/CentOS Zink on DD-Turnip share buffers with the in-app compositor, or does droidc
  need its own Mesa build in the bundle (and an `LD_LIBRARY_PATH` override)?
- Does the GNOME stack in Bluefin LTS require systemd for session startup? Verify before choosing the
  supervisor design.
- How large does a Bluefin LTS arm64 deployment get after link2symlink conversion?
- Would DroidDeck accept moving its staged files to binds, so both projects share one hardware bundle?

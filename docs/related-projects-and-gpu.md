# Related projects, and libhybris as a GPU driver source

How other "Linux on Android" projects run a Linux userland and reach the GPU, what droidc can borrow
from each, and whether [libhybris](https://github.com/libhybris/libhybris) can supply GPU drivers for
the OCI driver artifacts in #108.

Researched 2026-11-28 from project repos, release notes and official docs (sources at the end).
Background on DroidDeck's runtime is in [`droiddeck-runtime-internals.md`](droiddeck-runtime-internals.md);
the engine contract is in [`design-mvp.md`](design-mvp.md). Anything marked *unverified* could not be
confirmed from a primary source.

## Summary

- **There are four families.** Projects that *replace* Android with a Linux system (Halium/libhybris
  ports: Ubuntu Touch, Droidian, Sailfish OS; mainline ports: postmarketOS (now Nura), Mobian),
  projects that run Linux *as an app* beside stock Android (DroidDeck, Termux and proot-distro,
  UserLAnd, Andronix, Winlator), a *VM* (Android's own AVF Linux Terminal), and the *reverse*
  direction (Waydroid runs Android on Linux). droidc is in the app family; its closest peers are
  DroidDeck and proot-distro, which now also pulls OCI images.
- **libhybris is proven only on a Halium-patched Android.** Its own README says the Android side is
  patched to remove bionic's TLS accesses that clash with glibc, and Halium carries those patches
  (extern `__get_tls`, emulated TLS, fdsan off). Stock Android has none of them. Stock SELinux
  policy does let an app map GPU driver libraries and open the GPU device, and libhybris brings its
  own linker, so the platform's linker namespaces are not the blocker. The unpatched TLS is. No
  project we found runs libhybris GPU acceleration as an ordinary app on stock Android. So libhybris
  is **an experiment for #108, not the plan**.
- **Recommended driver order for #108:**
  1. **Turnip on KGSL (Adreno), built from upstream Mesa for glibc.** Upstream Mesa has
     `-Dfreedreno-kmds=kgsl`; Termux ships it and DroidDeck builds on it. This is the proven
     unprivileged path. Window-system support on KGSL is still a downstream patch.
  2. **A bridge to Android's own driver for everything else** (Mali, Xclipse, PowerVR): Venus or
     virgl in the guest, with a bionic-side renderer in Selkie on Android's Vulkan/EGL. Termux
     (virgl over ANGLE), DroidDeck (MaliBridge + Venus) and Winlator (Vortek) all do a version of
     this. It is the #73 "host renderer" path.
  3. **libhybris** as an experimental artifact type, only if a spike shows it works on an
     unpatched device.
  4. **llvmpipe** stays the fallback, as #108 says.
- **Standards line up well.** Flatpak's `org.freedesktop.Platform.GL` extension point already
  ships drivers apart from the runtime and merges exactly the directories #108 names
  (`vulkan/icd.d`, `glvnd/egl_vendor.d`, `lib/dri`, `lib/gbm`). OCI artifact manifests
  (`artifactType`, `subject`, referrers) cover delivery. Fedora is moving system extensions to
  container layers in a registry. droidc should match these rather than invent a layout.

## Comparison

| Project | How Linux runs | GPU path | Root / unlock / per-device port | License | Status (seen) |
| --- | --- | --- | --- | --- | --- |
| **droidc + Selkie** | bootc OCI image as a guest under proot (namespaces where allowed) | llvmpipe today; #73/#108 | None / none / none | MIT | in development |
| **DroidDeck** | Ubuntu 26.04 rootfs; proot until 0.7.1, "no proot by default" since 0.7.11 (method *unverified*); FEX for x86 | DD-Turnip on KGSL (Mesa main + patches); Zink on PowerVR; MaliBridge + Venus on Mali; RADV on Xclipse | None / none / none (Adreno 730+ best; a developer option on Android 14+) | GPL-3.0 | 0.7.11, Nov 25 |
| **Termux + proot-distro** | proot; distros pulled as OCI images from any registry | Termux's Mesa (below) | None | GPL-3.0 | 5.10.3, Oct 2026 |
| **Termux:X11 + Termux Mesa** | X server in an app; proot or chroot clients | Turnip (`freedreno-kmds=msm,kgsl`), Zink, virgl server over ANGLE, llvmpipe; termux-gfx-wrapper onto Android `libEGL` | None (chroot needs root) | GPL-3.0 | nightly; Mesa 26.3.1 |
| **UserLAnd / Andronix** | proot | software, VNC | None | MIT | UserLAnd source dormant since 2021; Andronix 8.0, Oct 2025 |
| **Winlator** (reference) | own runtime, Box64 + Wine | Turnip, Vortek (Vulkan bridge onto the vendor driver), Zink, VirGL | None | LGPL-2.1 | 11.2 |
| **AVF Linux Terminal** | Debian VM on pKVM/crosvm | gfxstream over virtio-gpu on AHardwareBuffer (Pixel 10 only), else lavapipe | None, but only for the system app: AVF permissions are signature-level | AOSP | Android 16 QPR2 / 17 |
| **Halium** | Minimal Android in an LXC container under a Linux distro | libhybris onto vendor blobs | Root, unlocked bootloader, Halium kernel per device | Apache-2.0 (Android) | branches to 16.0 |
| **Ubuntu Touch (UBports)** | Halium | libhybris; "partial Vulkan" in 26.04 work | Per-device port | GPL / mixed | 24.04-2.2, Oct 2026 |
| **Droidian** | Halium GSI + Debian | libhybris | Per-device port (GSI-based) | mixed | 101 (Nov 2025 patchlevel), slow |
| **Sailfish OS** | Halium-style hardware adaptation; origin of libhybris | libhybris | Per-device port | proprietary + open | 5.0 |
| **postmarketOS (Nura), Mobian, Plasma Mobile** | Mainline kernel, replaces Android | Mesa freedreno/panfrost on mainline DRM | Per-device mainline port, unlocked bootloader | mixed | Nura 26.06; Mobian trixie |
| **Waydroid** (reverse) | Android 13 in LXC on Linux | Host DRM node to Mesa inside the Android image (gbm gralloc); SwiftShader fallback | A Linux host with binder | GPL-3.0 | 1.6.3, May 2026 |

## How each GPU path works on stock Android

| Path | Who uses it | Works unprivileged on stock Android? | Fit for droidc |
| --- | --- | --- | --- |
| **Turnip on KGSL** (Mesa Vulkan for Adreno, talking to Android's `/dev/kgsl-3d0`) | DroidDeck, Termux, Winlator | Yes; KGSL is an app-accessible GPU device | Best first artifact. Build from upstream Mesa (MIT) for glibc; carry the KGSL WSI patch until it lands. |
| **Zink** (GL on Vulkan) | everyone above | Yes, on top of a Vulkan driver | Stock in Fedora/Bluefin Mesa; GL comes free once Vulkan works. |
| **PanVK / Panfrost** (Mali) | mainline only | No: upstream targets the panfrost/panthor DRM drivers, not Android's Mali `kbase`. kbase support exists only in the Panfork fork | Not for stock Android Mali. |
| **Venus / virgl over a host renderer** | Termux (virgl + ANGLE), DroidDeck (Venus on Mali) | Yes; the renderer runs in the app on Android's own driver | Covers every non-Adreno GPU. Needs a bionic renderer in Selkie (#73) and the buffer path (Selkie #109). |
| **Vulkan bridge onto the vendor driver** (Vortek, MaliBridge) | Winlator, DroidDeck | Yes | Same idea as Venus with less overhead; both are copyleft projects to study, not copy. |
| **Android EGL wrapper** (termux-gfx-wrapper) | Termux | Yes for X11 GLES | Bionic-side, like the renderer above; glvnd vendor is the right shape. |
| **gfxstream on virtio-gpu** | AVF Terminal | Only inside AVF, which third-party apps can't use | Not available to Selkie. Worth watching if AVF opens up. |
| **libhybris** | Halium ports | Unproven (see below) | Experimental only. |

## libhybris for #108

### How it works

- libhybris loads Android (bionic) libraries into a process whose libc is glibc or musl. It ships
  its own bionic linker per Android generation (`hybris/common/{jb,mm,n,o,q}`, with `q` covering
  Android 10 and later) and `hooks.c`, which maps bionic libc symbols onto glibc. A build is tied to
  one Android version through android-headers.
- The `q` linker reads Android's `ld.config` files and implements linker namespaces itself, so the
  platform linker's public-library rules don't apply to it. Default search path is `/vendor/lib64`,
  `/system/lib64`, `/odm/lib64`.
- It wraps EGL, GLES 1/2, OpenCL, gralloc and hwcomposer, and has a Wayland EGL platform with
  server-side buffers (`wayland-android` protocol). It has had **Vulkan** (`hybris/vulkan`, null and
  Wayland platforms) since 2021, last changed in 2022.
- On Android 10+, gralloc goes through `libui_compat_layer`, which Halium builds inside an AOSP tree
  for each Android version and which talks to the allocator HAL over binder. Stock phones don't have
  it.
- It is maintained, slowly: last commit 2026-06-24, Android 15/16 support merged in 2025/2026, mostly
  by Jolla engineers with Droidian and UBports contributors.

### What it assumes

From its README: "Android is patched to remove TLS accesses in bionic c library which usually cause
problems with glibc TLS slots. Android is also patched to remove zygote, surfaceflinger, …" and the
stripped Android runs as a service or container "to provide hardware adaptation services". Halium's
`hybris-patches` for Android 16 make `__get_tls` extern "so libhybris can intercept TLS access",
build libc with `-femulated-tls` because "ELF TLS currently breaks libhybris", and turn off fdsan.

### What stock Android allows and forbids

- AOSP `app.te` lets apps `execute read open getattr map` files labelled `same_process_hal_file`
  (which GPU drivers must be, so apps can use them) and read/write `gpu_device`. A glibc process in
  Selkie's domain can therefore map `/vendor/lib64/egl/*.so` and open `/dev/kgsl-3d0` or `/dev/mali0`
  (our reading of those rules).
- `app_neverallows.te` keeps untrusted apps off vendor binder and protected HAL services, so
  hwcomposer is out. droidc doesn't need it: Selkie presents.

### Obstacles on an unpatched device

1. **TLS.** Stock system and vendor libraries use bionic's TLS slots or ELF TLS through
   `tpidr_el0`, which in a glibc process points at glibc's thread block. Halium patches this away;
   stock Android doesn't. This is the risk most likely to kill the idea, and only a device test
   settles it.
2. **Gralloc compat layer** has to be built per Android release and depends on reaching the
   allocator HAL. Apps can allocate through `AHardwareBuffer`, so the service is reachable in
   principle (*unverified* for this layer).
3. **No app-surface platform.** libhybris renders to null, Wayland or hwcomposer. droidc would
   render into gralloc/AHardwareBuffer buffers and hand them to Selkie (`AHardwareBuffer_sendHandleToUnixSocket`),
   which is the same buffer path the Venus route needs (Selkie #109).
4. **proot binds**: `/vendor`, `/system`, `/apex`, `/linkerconfig` and `/dev/__properties__` must be
   visible in the guest, read-only. Whether bionic property reads work fully from an untrusted app
   is *unverified*.
5. **Build matrix**: per Android API level and, in practice, per SoC vendor (UBports needed a
   separate Qualcomm build).

### Fit with OCI artifacts

If a spike succeeds, a libhybris driver artifact fits #108's model with one difference from Mesa
artifacts: it carries the **shim, not the driver**. Vendor blobs are never redistributed; they are
loaded from the device's own `/vendor`, bound read-only into the guest.

- **Contents**: libhybris's `libEGL`/`libGLESv2`/`libvulkan` wrappers, its linker and hooks, the
  gralloc compat layer for that Android release, a glvnd EGL vendor JSON under
  `/usr/share/glvnd/egl_vendor.d` and a Vulkan ICD JSON under `/usr/share/vulkan/icd.d`, matching
  the Flatpak GL extension layout so Flatpak apps can get the same driver.
- **Extra capability annotations** beyond #108's list: Android API level (or range), ABI, SoC
  vendor, and a driver kind of `libhybris` so the picker never confuses it with a Mesa driver.
- **Signature policy** is the same as images (#62).

### Licensing

- libhybris is mainly Apache-2.0; the newer linker directories are BSD (`MODULE_LICENSE_BSD`).
  The repo also carries GPL-3, LGPL-2.1, BSD-4, ISC and MIT licence files. A header search of
  `hybris/` and `compat/` (commit 7079712) found LGPL headers only in the Wayland platform files
  (`hybris/egl/platforms/wayland/*`, `hybris/vulkan/platforms/wayland/*`,
  `hybris/platforms/wayland/*`) and no GPL headers; which build outputs the GPL-3 file covers is
  *unverified* and needs a check before shipping.
- Shipping it as a separate artifact that droidc loads at runtime is aggregation: it doesn't affect
  droidc's MIT licence. Ship its LICENSE and NOTICE files in the artifact, keep LGPL parts as
  shared libraries with source available, and never ship vendor blobs.
- For Turnip artifacts, build from upstream Mesa (MIT) rather than redistributing DroidDeck's
  DD-Turnip release, whose repo is GPL-3.0.

### Risks, in order

1. The TLS clash on unpatched systems may simply not be solvable from userspace.
2. A per-Android-version, per-SoC build matrix that the factory (#63) would have to carry.
3. A small maintainer base.
4. SELinux denials on property or service access we couldn't test.
5. No precedent anywhere for unprivileged use.

### What a spike has to show

On one Adreno and one Mali device on stock Android, from a glibc guest under proot with `/vendor`
and friends bound in: libhybris loads the vendor `libEGL`/`libvulkan`, creates a context, renders
offscreen into an AHardwareBuffer, and survives a multi-threaded test (TLS). If any of these fail
without system patches, close the libhybris option and keep to Turnip plus the bridge.

## What droidc can borrow

- **DroidDeck**: drivers delivered apart from the app, as versioned bundles with a manifest that
  records the Mesa commit and each library's SHA-256, with Auto / matched pair / mix-and-match modes
  and automatic rollback to the last pair that worked. That is #108's picker, user override and
  fallback, already shipping. Also worth learning how 0.7.11 runs without proot.
- **proot-distro**: OCI pulls with per-architecture manifest resolution, layer verification and
  caching, hard links turned into copies, whiteouts applied. A useful cross-check for droidc's
  unpacker.
- **Termux**: the Mesa build flags for KGSL, and the virgl-over-ANGLE server as the simplest
  bionic-side renderer.
- **Flatpak**: the GL extension point (`versions` tied to the runtime branch for ABI, `add-ld-path`,
  `active-gl-driver`, NVIDIA's version-matched extension names) as the model for matching driver
  artifacts to an image's Mesa/glibc ABI.
- **Waydroid**: picking the right render node and mapping kernel driver to Vulkan driver
  (msm → freedreno, panfrost → panfrost), the same decision droidc's picker makes from Android's
  GPU info.
- **AVF**: the zero-copy design (gfxstream on AHardwareBuffer, presented through SurfaceControl) is
  the reference for Selkie's buffer path, even though Selkie can't use AVF itself.
- **Halium/libhybris**: the knowledge of exactly which bionic behaviours break glibc callers, which
  is what the spike tests.

## Standards to follow

His principle: "We should adhere to existing Linux standards wherever possible, like XDG and LSB".

- Driver files go where the loaders already look (FHS): `/usr/share/vulkan/icd.d`,
  `/usr/share/glvnd/egl_vendor.d`, `/usr/lib64/dri`, `/usr/lib64/gbm`, bound over the deployment
  rather than written into it (the deployment stays digest-verified).
- Delivery uses the OCI image spec's artifact manifest (`artifactType`, empty config
  `application/vnd.oci.empty.v1+json`, `subject` for referrers) and standard
  `org.opencontainers.image.*` annotations; capability annotations go under one droidc-dev namespace
  (for example a reverse-DNS `io.github.droidc-dev.gpu.*`; to be settled in #108).
- Matching the Flatpak extension layout keeps the door open to exposing the same driver to Flatpak
  apps as an `org.freedesktop.Platform.GL.*` extension.

## Open questions

- How does DroidDeck 0.7.11 run Steam and the desktop "directly on Android" without proot, and does
  it apply to a bootc guest?
- Does Turnip's KGSL WSI patch have an upstream path, so droidc can build plain upstream Mesa?
- Can a bootc image's own Mesa (Fedora's Zink) sit on a Turnip artifact's ICD, or does the artifact
  need its own Zink to match Mesa versions?
- Would DroidDeck share a driver bundle format, so both projects publish one set of drivers?

## Sources

- libhybris: https://github.com/libhybris/libhybris (README, `hybris/common`, `hybris/vulkan`,
  licence files; read at commit 7079712, 2026-06-24)
- Halium: https://docs.halium.org/en/latest/project/Scope.html,
  https://github.com/Halium/hybris-patches/tree/halium-16.0/bionic, https://github.com/Halium/android
- UBports porting docs: https://docs.ubports.com/porting/introduction/Intro.html; releases:
  https://ubports.com/blog/ubports-news-1; 26.04 plans: https://phoronix.com/news/Ubuntu-Touch-Toward-26.04
- Droidian: https://droidian.org/blog/hybris-mobian-is-now-droidian/,
  https://github.com/droidian-images/droidian/releases
- Sailfish OS / libhybris origin: https://lwn.net/Articles/531671/,
  https://blog.jolla.com/sailfish-os-5-0-tampella-is-here/
- postmarketOS → Nura: https://itsfoss.com/news/postmarketos-nura-rebranding/, https://endoflife.date/nura,
  https://gitlab.com/postmarketOS/pmbootstrap/-/issues/678
- Mobian: https://theregister.com/2025/10/21/mobian_trixie
- Plasma Mobile leaving Halium: https://plasma-mobile.org/2020/12/14/plasma-mobile-technical-debt/
- Lindroid (libhybris with a DRM emulation layer; custom-ROM requirement *unverified*): https://opencollective.com/lindroid/updates/april-2026-update
- AOSP SELinux policy: https://android.googlesource.com/platform/system/sepolicy/+/master/private/app.te,
  https://android.googlesource.com/platform/system/sepolicy/+/master/private/app_neverallows.te;
  linker namespaces: https://source.android.com/docs/core/permissions/namespaces_libraries
- DroidDeck: https://raw.githubusercontent.com/Droid-Deck/DroidDeck/main/README.md,
  https://github.com/Droid-Deck/DroidDeck/releases/tag/0.7.11,
  https://raw.githubusercontent.com/Droid-Deck/Drivers/main/README.md
- proot-distro: https://raw.githubusercontent.com/termux/proot-distro/master/README.md,
  https://pypi.org/pypi/proot-distro/json
- Termux:X11 and Termux Mesa: https://raw.githubusercontent.com/termux/termux-x11/master/README.md,
  https://raw.githubusercontent.com/termux/termux-packages/master/packages/mesa/build.sh,
  https://raw.githubusercontent.com/termux/termux-packages/master/packages/virglrenderer-android/build.sh,
  https://www.github.com/tareksander/termux-gfx-wrapper
- Mesa: https://fuchsia.googlesource.com/third_party/mesa/+/main/meson.options (`freedreno-kmds`),
  https://gitlab.freedesktop.org/mesa/mesa/-/issues/9311 (KGSL WSI),
  https://www.collabora.com/news-and-blog/news-and-events/mesa-on-android-how-we-brought-turnip,-skiavk,-and-cuttlefish-to-ci.html,
  https://github.com/HidingSky/panfork_offscreen_rootless (Panfrost on kbase, downstream)
- UserLAnd: https://raw.githubusercontent.com/CypherpunkArmory/UserLAnd/master/README.md,
  https://f-droid.org/repository/browse/?fdid=tech.ula
- Winlator / Vortek: https://github.com/brunodev85/winlator/releases,
  https://raw.githubusercontent.com/brunodev85/vortek/main/README.md
- Waydroid: https://github.com/waydroid/waydroid (`tools/helpers/gpu.py`, `tools/helpers/lxc.py`)
- AVF: https://source.android.com/docs/core/virtualization,
  https://lpc.events/event/19/contributions/2123/,
  https://www.androidauthority.com/pixel-10-linux-apps-gpu-acceleration-3608754/,
  AVF permissions: https://android.googlesource.com/platform/packages/modules/Virtualization/+/df3037fc/javalib/README.md
- Flatpak GL extensions: https://docs.flatpak.org/en/latest/extension.html,
  https://gitlab.com/freedesktop-sdk/freedesktop-sdk/-/raw/master/elements/flatpak-images/platform.bst
- Fedora sysexts as container layers: https://fedora-sysexts.github.io/install-update,
  https://discussion.fedoraproject.org/t/unsupported-systemd-confext-and-sysext-on-bootc/202199
- OCI artifacts: https://github.com/opencontainers/image-spec/blob/main/manifest.md

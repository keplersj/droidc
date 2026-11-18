# droidc

Rust engine that runs unmodified [bootc](https://containers.github.io/bootc/) container images on
Android (and Linux) under proot, without root: pull from a registry, verify the cosign signature,
unpack the layers into a deployment directory, merge `/etc`, swap `current`/`previous` atomically.
The contract is in [`docs/design-mvp.md`](docs/design-mvp.md); background in
[`docs/droiddeck-runtime-internals.md`](docs/droiddeck-runtime-internals.md).

## Build

```sh
cargo build --release                       # host binary
cargo test --workspace                      # unit tests
DROIDC_NET_TESTS=1 cargo test -p droidc --test net   # + signature checks against ghcr.io/quay.io
```

Static musl binaries (same binary runs on Linux, Android `nativeLibraryDir`, and in the guest):

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
pip install ziglang==0.13.0 && cargo install cargo-zigbuild --locked
cargo zigbuild --release --target x86_64-unknown-linux-musl  -p droidc
cargo zigbuild --release --target aarch64-unknown-linux-musl -p droidc
file target/*-musl/release/droidc          # "statically linked"
```

Android host-side builds (bionic, run by Selkie as `libdroidc.so`; the musl build is the guest-side
`libdroidc-guest.so`). Android's app seccomp filter has no legacy syscalls (`mkdir`, `chmod`, ...) on
x86_64, so musl binaries get SIGSYS there; on Android rustix uses its libc backend (bionic wrappers):

```sh
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk --locked
export ANDROID_NDK_HOME=/path/to/android-ndk-r27c
cargo ndk -t arm64-v8a -t x86_64 --platform 26 build --release -p droidc
# -> target/aarch64-linux-android/release/droidc, target/x86_64-linux-android/release/droidc
```

`zig cc` is the C compiler for `ring` and `zstd-sys`; it needs no musl-gcc packages and no NDK, and
works the same on x86_64 and arm64 GitHub runners (see `.github/workflows/ci.yml`).

## Usage

```sh
droidc --root ~/droidc --json switch ghcr.io/projectbluefin/bluefin-lts:stable [--links link2symlink]
droidc --root ~/droidc switch quay.io/centos-bootc/centos-bootc:stream10 --insecure-allow-unsigned
droidc switch <ref> --verify-only      # resolve + verify signature, download no layers
droidc upgrade | rollback | status | gc | shell-spec | platform
droidc supervise -- bash -l            # inside the guest
droidc supervise --systemd=on -- bash -l   # run the image's own systemd --user (cgroup shim, see docs/systemd-on-proot.md)
droidc supervise --session --width 1920 --height 1200   # inside the guest: the graphical session
droidc session set --renderer screencast --exec '["gnome-shell","--headless","--virtual-monitor","{width}x{height}"]' \
  --services '[["pipewire"],["wireplumber"]]' --name GNOME   # for images without session labels
sudo droidc --root ~/droidc enter -- bash -l   # with root: namespaces + chroot instead of proot
sudo droidc --root ~/droidc fix-ownership [--undo]   # apply the image's owners/setuid (root only from then on)
sudo droidc --root ~/droidc switch <ref> --apply-ownership   # same, for the deployment switch activates
```

`--root`/`DROIDC_ROOT`, `--dns`/`DROIDC_DNS` (comma separated IPs; else `/etc/resolv.conf`, else
1.1.1.1,8.8.8.8), `--json` (NDJSON events; last line is always `result`; exit 0 iff ok).
Other environment: `HTTPS_PROXY`/`NO_PROXY`, `DROIDC_EXTRA_CA_FILE` or `SSL_CERT_FILE` (extra PEM CAs
on top of webpki-roots), `DROIDC_L2S_PREFIX` (`.l2s.` for Termux proot, `.proot.l2s.` for upstream
proot). Without a matching trust policy entry and valid signature, `switch` fails with kind
`signature` unless `--insecure-allow-unsigned` is given.

Try a deployment under proot: `proot -r <root>/current/rootfs -w / /usr/bin/cat /etc/os-release`.

## Dependencies

- `clap` CLI parsing (derive). `serde`, `serde_json` records, NDJSON events, OCI JSON.
- `sha2`, `hex` blob/manifest digests. `base64` xattr values in the metadata file.
- `reqwest` (blocking, rustls, no default features) HTTP client; `rustls` (ring provider) +
  `webpki-roots` TLS with built-in trust anchors, no OpenSSL or system store;
  `hickory-resolver` custom DNS resolver plugged into reqwest. We use reqwest directly instead of
  `oci-client` because we need the custom resolver/CA/proxy setup, raw manifest bytes and
  hash-while-streaming resumable downloads (see the module comment in `registry.rs`).
- `tar` layer parsing, `flate2` gzip (pure-Rust backend), `zstd` zstd layers and the metadata file.
- `rustix` uname, statvfs, flock, wait, dup2, and for `enter` unshare, mount, openat2, chroot,
  without libc.
- `zbus` (blocking) D-Bus client for `droidc bridge` (Mutter ScreenCast/RemoteDesktop).
- `wayland-client` (pure Rust backend) + `wayland-protocols` (xdg-shell): `droidc bridge` as a
  Wayland client of the host compositor.
- `droidc-verify` (workspace crate) offline cosign verification (keyless and key).
- dev: `tempfile`.

## Layout of the code

`registry` (OCI client), `unpack` (layers, whiteouts, drop rules, metadata), `link` (hardlink /
link2symlink / copy / auto), `etcmerge` (3-way /etc), `deploy` (staging, PENDING, recovery, switch,
upgrade, rollback, gc), `status` (status, shell-spec), `supervise` (in-guest supervisor, graphical session), `session` (session labels and override),
`bridge` (headless compositor → host compositor), `events` (NDJSON), `platform`,
`verify_glue` (single seam to `droidc-verify`).

License: MIT.

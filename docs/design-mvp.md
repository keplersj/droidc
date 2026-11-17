# droidc MVP design and contracts

This is the contract between droidc (Rust engine) and Selkie (Android app), and between the parts
of droidc. Read `droiddeck-runtime-internals.md` first for background.

## Build targets

- droidc is built as a **fully static musl binary**: `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl`. The same binary runs on a Linux host, on Android (shipped as
  `jniLibs/<abi>/libdroidc.so`, exec'd from `nativeLibraryDir`) and *inside* the guest under proot
  (bound to `/usr/bin/droidc`), so `droidc status --json` works in the in-app terminal and the
  session supervisor can be the same binary.
  - No NDK is needed for droidc. Android has no `/etc/resolv.conf` and no system CA store usable by
    musl, so networking uses **rustls + webpki-roots** and a resolver that takes nameservers from
    `--dns`/`DROIDC_DNS` (comma-separated IPs; Selkie passes Android's current DNS servers),
    falling back to `/etc/resolv.conf`, then `1.1.1.1,8.8.8.8`.
- Extra CA bundle for development/proxies: `DROIDC_EXTRA_CA_FILE` (then `SSL_CERT_FILE`) adds PEM
  certificates on top of webpki-roots. `HTTPS_PROXY`/`NO_PROXY` are honored.
- proot is built separately with the NDK (bionic), GPL-2, see Selkie `third_party/proot/`.

## Platform

Platform comes from `uname(2)` `machine`, never from Android's ABI list:
`aarch64`→`arm64`, `x86_64`→`amd64`. `--platform linux/arm64` overrides (used to unpack an arm64
image on an x86_64 host for measurement only).

## On-disk layout (`--root`, env `DROIDC_ROOT`, default `$XDG_DATA_HOME/droidc`)

```
<root>/
  policy.json                 trust policy (written with defaults on first run if absent)
  blobs/sha256/<hex>          content-addressed blobs (manifests, configs, layers); GC'd
  deployments/<id>/           id = first 16 hex chars of the image manifest digest
    deployment.json           metadata, see "Deployment record"
    metadata.jsonl.zst        per-path uid, gid, mode, setuid/setgid/sticky, xattrs, link info
    rootfs/                   the guest root; never written after it is deployed
      usr/etc/                pristine /etc from the image (ostree style)
      etc/                    merged /etc (3-way merge result)
      var -> (empty dir; persistent var is bound over it)
  current  -> deployments/<id>   relative symlink, replaced atomically (symlink + rename)
  previous -> deployments/<id>   exactly one previous
  staging/<id>/               in-progress unpack; deleted on success or on recovery
  staging/PENDING             marker JSON {op, id, started}; present only during an operation
  var/                        persistent /var for every deployment (bound to /var)
  home/                       persistent /home (bound to /home); /root is bound to home/root
  lock                        flock(2) lock; one mutating command at a time
```

Crash safety: write `staging/PENDING`, unpack into `staging/<id>`, fsync, rename into
`deployments/<id>`, then update `previous` and `current` with symlink-to-temp + `rename(2)`, then
remove `PENDING`. Any command that finds `PENDING` first recovers: deletes `staging/*`, and if
`deployments/<id>` exists but `current` doesn't point at it, leaves it for GC. GC removes
deployments other than current/previous and blobs not referenced by them.

**Deviation (implemented):** recovery is *roll-forward* once the deployment is complete. `PENDING`
is `{op: switch|rollback, id, prev, started}` where `id` becomes `current` and `prev` (the old
`current`) becomes `previous`. If `deployments/<id>/deployment.json` exists (the staging directory was
already renamed in), recovery finishes the swaps (idempotent: seed `var/`, `previous` := `prev`,
`current` := `id`). Only when the deployment is not yet in place does it discard `staging/*`. This
avoids ending up with `previous == current` after a crash between the two symlink renames and makes
`rollback` restartable. A successful `switch`/`upgrade` runs GC at the end (phase `gc`).

## Unpacking

- Layers are applied in order (gzip, zstd, uncompressed tar). OCI whiteouts: `.wh.<name>` deletes,
  `.wh..wh..opq` makes the directory opaque.
- Dropped: `/boot`, `/usr/lib/modules`, `/usr/lib/firmware`, `/usr/lib/ostree-boot`, kernel and UKI
  files (`vmlinuz*`, `initramfs*`, `*.efi` under `/usr/lib/systemd/boot` and `/boot`).
- No chown. Mode bits that are safe (rwx) are applied; uid/gid/setuid/xattrs go to
  `metadata.jsonl.zst`. Directories are made owner-writable so the tree can be updated and deleted.
  Judgment call: regular files additionally get owner read+write (`mode | 0o600`) because the app uid
  must be able to read/replace everything (e.g. a `000` `/etc/shadow`); the true mode is in the
  metadata. Mtimes of regular files are kept; special files (devices, fifos) are skipped.
- `metadata.jsonl.zst`: one JSON object per final path (sorted), `{"p","t","u","g","m","x","l"}`:
  type `f|d|l|h`, uid, gid, full mode incl. setuid/setgid/sticky, xattrs (name -> base64 value, from
  PAX `SCHILY.xattr.*`), symlink/hard-link target. Paths are as in the final rootfs (image `etc/`
  appears as `usr/etc/`). The directories `/boot`, `/usr/lib/modules`, ... are kept but empty.
- `/etc` from the image is moved to `usr/etc` after the last layer.
- Hard links go through `trait LinkStrategy`: `hardlink`, `link2symlink` (proot-compatible layout,
  so `proot --link2symlink` sees real hard links), `copy`, and `auto` (try `link(2)`, on
  EPERM/EACCES/EXDEV fall back to link2symlink). Selkie passes `--links link2symlink` on Android.
  - `link2symlink` creates exactly the structure proot's extension creates (format below), with
    absolute host paths that point at the **final** location (`<root>/deployments/<id>/rootfs/...`,
    `etc/` mapped to `usr/etc/`, seeded `var/` mapped to `<root>/var/`). `<root>` is canonicalized
    when droidc opens it, so Selkie must run proot on that same canonical path and must not move or
    rename the root afterwards (existing chains would dangle). Prefix: `.l2s.` (Termux proot,
    default); upstream proot-me uses `.proot.l2s.` - set `DROIDC_L2S_PREFIX` to match the proot in use.
  - Hard links whose target is later replaced or whited out in a later layer keep working (the
    unpacker never writes through an existing inode and maintains l2s link counts on removal).

### link2symlink on-disk format

For a regular file `D/name` that gets a second name `E/other`:

```
D/.l2s.name0001.0002   regular file, the real data ("final"); suffix = link count, >= 4 digits
D/.l2s.name0001        symlink -> <abs path of D/.l2s.name0001.0002>               ("intermediate")
D/name                 symlink -> <abs path of D/.l2s.name0001>
E/other                symlink -> <abs path of D/.l2s.name0001>
```

`0001` is the first free 4-digit suffix (1..9999) for that name in `D`. Another link renames the
final file to `...0003` and re-creates the intermediate; removing a name reverses this and deletes
intermediate and final at zero. Names too long for the suffixes (ENAMETOOLONG) or full chains
(EMLINK) fall back to a copy. See `crates/droidc/src/link.rs`.
- Recorded in `deployment.json`: hard-link count, strategy used, file count, bytes on disk, unpack
  seconds, download bytes.
- Before unpacking, check free space (`statvfs`) against an estimate (2.5x compressed size, plus
  compressed size if blobs are not yet cached); fail with error kind `storage`.

## /etc 3-way merge (on deploy)

Inputs: old pristine (`current/rootfs/usr/etc`), old live (`current/rootfs/etc`), new pristine.
For each path: unchanged locally (old live == old pristine) → take new pristine; modified or added
locally → keep local; deleted locally → stays deleted. First deployment: live = copy of pristine.

## Signatures and trust policy

`policy.json` (droidc format, inspired by containers-policy.json):

```json
{
  "version": 1,
  "default": "reject",
  "images": [
    {"match": "ghcr.io/projectbluefin/bluefin-lts",
     "keyless": {"issuer": "https://token.actions.githubusercontent.com",
                 "subject_regex": "^https://github\\.com/projectbluefin/"}},
    {"match": "ghcr.io/ublue-os/*", "keys": ["-----BEGIN PUBLIC KEY-----..."]}
  ]
}
```

- Signatures are found as cosign "simple signing" tags `sha256-<hex>.sig` on the per-platform
  manifest digest (falling back to the index digest). Bluefin LTS is signed **keyless**
  (Fulcio cert + Rekor bundle in layer annotations), not with `cosign.pub`.
- Keyless verification is offline: Sigstore public-good trusted root embedded in the binary
  (overridable), leaf cert chain to Fulcio, Rekor SET verified, cert valid at the integrated time,
  issuer and SAN checked against policy, payload `critical.image.docker-manifest-digest` equals the
  pulled digest.
- An image with no matching policy entry, or no signature, fails with error kind `signature` unless
  `--insecure-allow-unsigned` is given; the deployment then records `"insecure": true` and status
  shows it.

## CLI

```
droidc [--root DIR] [--json] [--dns IPS] <command>
  switch <ref> [--insecure-allow-unsigned] [--platform os/arch] [--links auto|hardlink|link2symlink|copy]
  upgrade                       re-pull current ref; no-op if digest unchanged
  rollback                      swap current and previous
  status                        current, previous, staged
  gc
  shell-spec                    how to enter the current deployment (rootfs, binds, env, cwd)
  platform                      detected platform
  supervise -- <cmd...>         (inside the guest) start session services, dbus first, then cmd
```

### `--json` output: newline-delimited JSON on stdout

Every line is one object with an `event` field. Human text goes to stderr.

```json
{"event":"progress","phase":"resolve|download|verify|unpack|etc-merge|deploy|gc","current":123,"total":456,"unit":"bytes|files|layers","message":"layer 3/68"}
{"event":"log","level":"info|warn|error","message":"..."}
{"event":"result","ok":true,"data":{...}}
{"event":"result","ok":false,"error":{"kind":"network|signature|storage|not-found|invalid|locked|internal","message":"..."}}
```

The last line is always a `result`. Exit status is 0 iff `ok`.

### Deployment record (`deployment.json`, and each entry of `status`)

```json
{"id":"d2a12ef344f80bb0","image_ref":"ghcr.io/projectbluefin/bluefin-lts:stable",
 "manifest_digest":"sha256:...","index_digest":"sha256:...|null","config_digest":"sha256:...",
 "platform":"linux/arm64","created":"<image created>","deployed_at":"<rfc3339>",
 "session_label":"gnome|null","labels":{},
 "signature":{"verified":true,"mode":"keyless|key|none","subject":"...","issuer":"...","policy_match":"..."},
 "insecure":false,
 "stats":{"download_bytes":0,"files":0,"bytes_on_disk":0,"hardlinks":0,"link_strategy":"link2symlink","unpack_seconds":0.0}}
```

`status` data: `{"root":..., "platform":{...}, "current":<record>|null, "previous":<record>|null,
"staged":<PENDING>|null}` (`staged` is non-null only while an operation runs or was interrupted).

`switch`/`upgrade` result data: `{"changed":true,"id","previous","deployment":<record>,"gc":{...}}`, or
`{"changed":false,"id","manifest_digest","deployment"}` when the digest is already current.
`switch --verify-only` resolves and verifies the signature (manifests and cosign `.sig` only, no
layers) and returns `{"changed":false,"verify_only":true,"id","manifest_digest","signature",...}`.
`rollback`: `{"changed":true,"current","previous"}`. `gc`: `{"deployments_removed":[...],
"blobs_removed":n,"bytes_freed":n}`. `stats` additionally has `link_strategy_used`.

### `shell-spec` data

```json
{"rootfs":"<root>/current/rootfs","binds":[{"host":"<root>/var","guest":"/var"},{"host":"<root>/home","guest":"/home"},{"host":"<root>","guest":"/run/droidc/root"}],
 "env":{"DROIDC_ROOT":"/run/droidc/root","HOME":"/root","PATH":"...","TERM":"xterm-256color","LANG":"C.UTF-8"},
 "cwd":"/root","session_label":"gnome"}
```

Selkie adds Android binds (`/dev`, `/proc`, `/sys`, fake proc files, `/usr/bin/droidc` → the
`libdroidc.so` path) and runs `proot`.

## Session label

`containers.droidc.session` from the image config labels (`gamescope`, `plasma`, `gnome`), recorded
as `session_label`; `null` when absent (Bluefin LTS doesn't set it yet, so Selkie defaults GNOME
images by name only for display).

## `supervise` (seed of the session supervisor)

`droidc supervise -- cmd...` runs inside the guest: creates `XDG_RUNTIME_DIR` (0700) if needed, starts
a session bus and exports `DBUS_SESSION_BUS_ADDRESS`, runs `cmd` as a child, reaps all children and
exits with the command's status (the command always runs, even if no bus could be started).
CentOS/Bluefin images ship only `dbus-broker` and `systemd --user` cannot run under proot, so droidc
does the socket activation itself: bind+listen `$XDG_RUNTIME_DIR/bus`, spawn
`dbus-broker-launch --scope user` with that socket as fd 3 and `LISTEN_FDS=1`,
`LISTEN_FDNAMES=dbus.socket`, `LISTEN_PID=<broker pid>` (set by a tiny `sh -c ... exec` wrapper so the
pid matches). Falls back to `dbus-daemon --session --fork --print-address` when present. Untested
against a real broker inside proot (the plumbing has a unit test with a fake broker).

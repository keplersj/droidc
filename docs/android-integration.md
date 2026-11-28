# Android integration

Where droidc can hand the guest something Android already has, and how to get it there. Written after
reviewing the systemd shim work (PR #5) and Termux's API bridge. **Only the journal → logcat part is
implemented**; the rest is a ranked list with a proposed mechanism. Nothing here was tried on a device.

## How Termux bridges Android APIs

Termux:API is a separate app. Its `termux-api` client binary (in the guest) talks to it with two
abstract Unix sockets and, when no API server is already listening, an `am broadcast`
(source: [termux-api-package](https://github.com/termux/termux-api-package/blob/master/termux-api.c)):

1. Try an abstract socket the API app listens on (`com.termux.api://listen`, Android < 14 only), and
   check the peer's uid.
2. Otherwise create two abstract sockets with random names (one for stdin, one for stdout), and run
   `am broadcast --user 0 -n com.termux.api/.TermuxApiReceiver --es socket_input … --es socket_output …
   --es api_method <name>`. The app's receiver connects to both: stdin goes in, results come out.
3. File descriptors travel as `SCM_RIGHTS` messages (e.g. a USB device fd); the exit code is carried back.

Two lessons carry over. Abstract sockets work across proot (it shares the network namespace) and need no
filesystem path or bind. And the `am` hop exists only because Termux:API is a *different* app from the
one running the shell. Selkie runs the guest as its own uid, so droidc can skip that hop: the app listens,
the guest connects, and `SO_PEERCRED` confirms it is the same uid. (When the app is not running, the
client gets `ECONNREFUSED` and reports it.)

## Proposed shape

Three layers, each usable without the next:

- **A. A socket protocol and a client.** Newline-delimited JSON over an abstract socket, in the style of
  droidc's `--json` events: `{"id":1,"method":"settings.get","params":{…}}` → `{"id":1,"result":…}` or
  `{"id":1,"error":{"kind":…}}`, plus server-pushed `{"event":…}` messages and `SCM_RIGHTS` for fds
  (a document picked through the storage framework arrives as an fd). `droidc android <method> [args]`
  is the scriptable client (the Termux CLI equivalent); a small Rust client module serves the layers
  below. The server is Selkie's. droidc owns the protocol document and tests it against a fake server.
- **B. D-Bus stand-ins** for what GNOME already asks the system and session buses (the pattern
  `login1.rs` set), fed by layer A.
- **C. An `xdg-desktop-portal` backend** (`org.freedesktop.impl.portal.desktop.droidc`): apps written for
  GNOME or Flatpak reach Android through portals without knowing it, so this is the widest door.

## Integration points, ranked

Value for effort, cheapest first. "Code" names where it would attach in the current tree.

| # | What | Android side | Guest side (layer) | Code |
| --- | --- | --- | --- | --- |
| 1 | **Logs** | logd | journal sockets → `adb logcat` (**done**, `DROIDC_LOGCAT`) | `journal.rs`, `logcat.rs` |
| 2 | **Appearance, locale, time zone, hostname** | night mode, accent, font scale, animation scale, contrast, `Locale`, `TimeZone`, device name | `droidc host-link`: an `appearance` line sets GNOME's `color-scheme`, `accent-color`, `text-scaling-factor`, `enable-animations` and `high-contrast` (**appearance done**), which the GNOME portal hands on as `org.freedesktop.portal.Settings`; still open: `timedate1`, `localed`, `hostname1` stand-ins (GNOME Settings and `gsd-datetime` ask them); `TZ`/`LANG` in the `shell-spec` env (today fixed to `C.UTF-8`, no `TZ`) | `host_link.rs`, B, `status.rs` |
| 3 | **Open links and files** | `ACTION_VIEW` intents | `droidc open` and a hidden `dev.droidc.AndroidOpen` desktop entry, the default `x-scheme-handler` for http, https, mailto, tel, sms and geo where the user has none, so `xdg-open`, GNOME apps and the GNOME `OpenURI` portal open links on Android (**links done**); still open: files (`file:` paths need a copy into shared storage first) | `open.rs` |
| 4 | **Suspend, lock, inhibit** | Activity pause/resume, `KeyguardManager`, wake locks | `droidc host-link`: `keep-awake` while a desktop app holds a gnome-session idle inhibitor (the host keeps the screen on), and the host's `screen-lock` turns GNOME's lock screen, idle blank and idle suspend off since Android locks the device (**done**); still open in `login1`: `PrepareForSleep`, `LockedHint`/`IdleHint`, `CanSuspend`/`CanPowerOff` (today `na`, and the power calls are refused) | `awake.rs`, `login1.rs` |
| 5 | **Battery and power** | `BatteryManager`, power-save mode | `droidc host-link` serves `org.freedesktop.UPower` on the container's system bus (one battery and the display device), fed by the host's `battery` lines (**battery done**); still open: `power-profiles-daemon` from power-save mode | `upower.rs` |
| 6 | **Network state** | `ConnectivityManager` | `droidc host-link` serves the `org.freedesktop.NetworkManager` manager object on the container's system bus (`Connectivity`, `Metered`, `State`, `PrimaryConnectionType`), fed by the host's `network` lines, so GLib's network monitor, and the NetworkMonitor portal behind it, sees Android's captive portals, lost connections and metered networks (**done**); not served: devices and connections, so gnome-shell's network menu stays hidden and Android's Quick Settings stay in charge | `network.rs` |
| 7 | **Memory pressure** | `onTrimMemory`, lmkd | `org.freedesktop.LowMemoryMonitor` (GNOME and portals watch it) fed by trim events; Android's own kill policy is the real limit | B |
| 8 | **Notifications** | `NotificationManager` | mirror `org.freedesktop.Notifications` calls (a bus monitor, since the shell owns the name) with actions routed back | B/C |
| 9 | **File chooser** | storage access framework | `FileChooser` portal returning fds; `xdg-user-dirs` bound to shared storage (Selkie) | C |
| 10 | **Brightness, sensors, location, haptics** | `SensorManager`, `LocationManager`, `Vibrator` | `login1` `SetBrightness` (refused today), `iio-sensor-proxy` (auto-rotate, ambient light), GeoClue (Weather, Maps, automatic time zone), `feedbackd` | B |
| 11 | **App launcher** | launcher shortcuts | `droidc apps --json`: read the deployment's `.desktop` files (name, icon path, categories, `NoDisplay`) with no guest process, so Selkie can list and launch guest apps | new, host side |
| 12 | **Media controls** | `MediaSession` (lock screen, shade, headset buttons; later Android Auto) | `droidc media`: watch the guest's MPRIS players on its session bus, JSON lines out, `play-pause <player>` and the like in (**done**) | `media.rs` |
| 13 | **Clipboard, IME, audio** | `ClipboardManager`, IME, AAudio | belong to Selkie's compositor and audio path, not droidc | out of scope |

Items 2 to 4 are what makes a session feel native and most are read-only or one-way, so start there.

## The host link

`droidc host-link` is the one channel for everything above that is neither sound (`audio-out`) nor
media (`media`). The host starts it in the guest under `supervise`, next to the desktop, and talks
JSON lines over its stdio, one object per line with a `kind`, instead of the abstract socket of layer A:
the host already owns the process, so there is no peer to check and nothing to listen on.

- Host to guest (stdin): `appearance` (`dark`, `accent` as `#rrggbb`, `text_scale`, `animations`,
  `high_contrast`; each optional). Settings are written with the image's `gsettings` (or
  `DROIDC_GSETTINGS`), only when they change; a key the image's GNOME lacks is tried once.
- Guest to host (stdout): `ready` with droidc's version, first; then `open-uri` (`uri`) for each link
  the desktop opens. host-link serves `dev.droidc.HostLink1.OpenUri` on the session bus for
  `droidc open` to call, and only passes the schemes in `open::SCHEMES` (no `file:`, no `intent:`).
  The host opens it with `ACTION_VIEW`. `ready` comes after the bus name is taken, so a host that has
  seen it can rely on links working. `keep-awake` (`on`) follows bit 8 (idle) of gnome-session's
  `InhibitedActions`; it is sent only when it changes, starting from off.
- Host to guest also: `battery` (`present`, `percent`, `status` as `charging`, `discharging`, `full` or
  `not-charging`, `plugged`, `time_to_full` in seconds) updates the UPower stand-in, which host-link
  serves on `$DBUS_SYSTEM_BUS_ADDRESS` unless a real upowerd owns the name.
- Host to guest also: `network` (`connected`, `validated`, `portal`, `metered`, `transport` as `wifi`,
  `cellular`, `ethernet`, `bluetooth` or `vpn`) updates the NetworkManager stand-in beside it, unless a
  real NetworkManager owns the name.
- Host to guest also: `screen-lock` (`by_android: true`) writes `lock-enabled=false`,
  `idle-delay=0` and the power plugin's `sleep-inactive-*-type='nothing'`.

Unknown kinds and bad lines are skipped with a warning on stderr, so the host and droidc can be updated
separately. Later integration points add kinds in both directions.

## Review notes on the systemd shim (PR #5)

- `login1.rs` is the natural attachment point for 4 and 10. Its stubs already have the right method
  surface; what they lack is a source of truth, which layer A would provide.
- The shim owns a fake cgroup tree, so `cgroup.freeze` (and `cgroup.kill`, already done) can be honoured
  by signalling the processes the helper already tracks. That would let the app freeze the session when
  it is backgrounded, instead of leaving Android's phantom-process killer to pick victims.
- `/proc/pressure/*` is likely unavailable to apps; `LowMemoryMonitor` fed by `onTrimMemory` replaces
  what `systemd-oomd` and GNOME read there.
- Unit output no longer vanishes: with `DROIDC_LOGCAT` set, the stdout/stderr of every unit and the
  manager's own messages reach logcat under the unit's identifier.

## Logcat (implemented)

`droidc log [-p debug|info|warn|error] [-t tag] message...` writes one message from a script, in the
guest or on the host, whatever `DROIDC_LOGCAT` says. Checked on a Galaxy Tab S10+ (Android 16, SELinux
enforcing): from the adb shell uid, from Selkie's app uid, and from inside proot, each shows up in
`adb logcat -v uid` under the right uid with the right priority.


`DROIDC_LOGCAT=off|auto|debug|info|warn|error` (default off; `auto` means info). `supervise` binds the
journal's datagram socket (`sd_journal_send`, the manager's log) and, with `--systemd`, its stdout
stream socket, and forwards what arrives to `/dev/socket/logdw`, which is how `liblog` itself writes.
The identifier is the tag; journal `PRIORITY` (and `<N>` prefixes on unit output) become the Android
priority; long lines are split. Opt-in because guest logs can hold paths and user names and logcat
lands in bug reports; logd also throttles floods, so debug level on a busy shell will lose lines.
`DROIDC_LOGDW` points at another socket for tests. The wire format is logd's ABI to liblog rather than a
documented API, stable since Android 7, and checked on the device above (the guest sees `/dev/socket/logdw` through Selkie's `/dev` bind). The
forwarding of journal and unit output (`DROIDC_LOGCAT` with `supervise`) has so far been run against a
fake logd only.

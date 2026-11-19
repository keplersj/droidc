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
| 2 | **Appearance, locale, time zone, hostname** | night mode, font scale, animation scale, `Locale`, `TimeZone`, device name | `org.freedesktop.portal.Settings` (color scheme, contrast, reduced motion, text scale); `timedate1`, `localed`, `hostname1` stand-ins (GNOME Settings and `gsd-datetime` ask them); `TZ`/`LANG` in the `shell-spec` env (today fixed to `C.UTF-8`, no `TZ`) | B/C, `status.rs` |
| 3 | **Open links and files** | `ACTION_VIEW` intents | `OpenURI` portal backend and an `xdg-open` shim, so GNOME apps open links in the Android browser | C |
| 4 | **Suspend, lock, inhibit** | Activity pause/resume, `KeyguardManager`, wake locks | `login1`: emit `PrepareForSleep`, drive `LockedHint`/`IdleHint`, make `Inhibit` hold a wake lock; `CanSuspend`/`CanPowerOff` return `yes` where they map (today `na`, and the power calls are refused) | `login1.rs` |
| 5 | **Battery and power** | `BatteryManager`, power-save mode | `org.freedesktop.UPower`, `power-profiles-daemon` stand-ins | B |
| 6 | **Network state** | `ConnectivityManager` | `org.freedesktop.NetworkManager` stand-in (shell indicator, GIO network monitor) | B |
| 7 | **Memory pressure** | `onTrimMemory`, lmkd | `org.freedesktop.LowMemoryMonitor` (GNOME and portals watch it) fed by trim events; Android's own kill policy is the real limit | B |
| 8 | **Notifications** | `NotificationManager` | mirror `org.freedesktop.Notifications` calls (a bus monitor, since the shell owns the name) with actions routed back | B/C |
| 9 | **File chooser** | storage access framework | `FileChooser` portal returning fds; `xdg-user-dirs` bound to shared storage (Selkie) | C |
| 10 | **Brightness, sensors, location, haptics** | `SensorManager`, `LocationManager`, `Vibrator` | `login1` `SetBrightness` (refused today), `iio-sensor-proxy` (auto-rotate, ambient light), GeoClue (Weather, Maps, automatic time zone), `feedbackd` | B |
| 11 | **App launcher** | launcher shortcuts | `droidc apps --json`: read the deployment's `.desktop` files (name, icon path, categories, `NoDisplay`) with no guest process, so Selkie can list and launch guest apps | new, host side |
| 12 | **Clipboard, IME, audio** | `ClipboardManager`, IME, AAudio | belong to Selkie's compositor and audio path, not droidc | out of scope |

Items 2 to 4 are what makes a session feel native and most are read-only or one-way, so start there.

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

# USB devices in the guest

Issue #100 (Selkie side: droidc-dev/Selkie companion issue). Goal: `fastboot`, `adb` and other tools in
the guest reach a USB device the person allowed in Android, so a Pinniped OS flash target can be
flashed from the desktop. Tablet and phone first.

Status: the guest side (`crates/usbshim`) is built here; the host side is Selkie's. Nothing was tried
on a device yet.

## Why a shim

An app cannot open `/dev/bus/usb/BBB/DDD` itself: the node is `root:usb 0660`, and an app cannot make
device nodes. Android hands out access one device at a time: after its own permission prompt,
`UsbManager.openDevice()` returns a `UsbDeviceConnection` whose file descriptor is an open usbfs file.
Every usbfs operation (`read()` of the descriptors, the `USBDEVFS_*` ioctls, bulk transfers) works on
that fd. Reopening it through `/proc/self/fd/N` or a bind mount does not help: that is a fresh open of
the same inode, and the permission check fails again.

So the fd has to travel from Selkie to the guest process that wants it, as `SCM_RIGHTS`, the way
Termux:API passes a USB fd to `termux-usb`. Programs do not know how to ask for one, so a preloaded
library asks for them at the moment they `open()` the node.

## The pieces

```
  Android                              Selkie (host)                       guest
  ───────                              ─────────────                       ─────
  permission prompt ──grant──▶  openDevice(), raw descriptors  ──writes──▶ /dev/bus/usb/BBB/DDD (empty file)
                                                               ──writes──▶ /sys/bus/usb/devices/B-D/…
                                 abstract socket server  ◀── usb.open ──── usbshim.so in fastboot/adb
                                 fresh openDevice() fd  ──SCM_RIGHTS─────▶ returned from open()
```

1. **Device trees (Selkie).** For each device the person allowed, Selkie writes placeholder files into
   two host directories it binds into proot at `/dev/bus/usb` and `/sys/bus/usb/devices`. Only allowed
   devices appear, so a scan in the guest sees exactly what Android granted. The layout copies the
   kernel's, because that is what the tools scan:

   | Path | Contents |
   | --- | --- |
   | `/dev/bus/usb/BBB/DDD` | empty regular file (`BBB`, `DDD` zero-padded to 3 digits, from `UsbDevice.getDeviceName()`) |
   | `/sys/bus/usb/devices/B-D/busnum`, `devnum` | decimal, newline |
   | `…/idVendor`, `idProduct` | `%04x`, newline |
   | `…/manufacturer`, `product`, `serial` | strings, newline; `serial` only when Android gives it |
   | `…/descriptors` | the device and configuration descriptors (`UsbDeviceConnection.getRawDescriptors()`, the same bytes `read()` on usbfs gives) |
   | `…/bConfigurationValue` | the active configuration, decimal |
   | `…/B-D:1.N/interface` | the interface's name string, when it has one |

   The sysfs name `B-D` (bus, then device number) stands in for the port path, which Android does not
   expose. It has the shape `fastboot` and libusb expect (digits, dots and dashes).

2. **The fd server (Selkie).** An abstract `SOCK_STREAM` socket, `io.github.keplersj.selkie.usb` by
   default (`DROIDC_USB_SOCKET` names another). Abstract sockets work across proot, which shares the
   network namespace. Each connection carries one request line and one reply line, in the layer A
   shape of [android-integration.md](../android-integration.md):

   ```
   → {"id":1,"method":"usb.open","params":{"bus":1,"dev":5}}
   ← {"id":1,"result":{}}                                    + one fd in SCM_RIGHTS
   ← {"id":1,"error":{"kind":"not-allowed","message":"…"}}   no fd
   ```

   Error kinds: `not-allowed` (no permission, `EACCES` in the guest), `gone` (unplugged, `ENODEV`),
   `bad-request` (`EINVAL`); anything else is `EIO`. The server checks the client's `SO_PEERCRED` uid
   is its own and refuses anyone else. Each `usb.open` calls `openDevice()` again, sends that fd and
   closes Selkie's copy, so every guest `open()` gets its own usbfs file, as on Linux (claimed
   interfaces are per file and are released when the guest closes it).

3. **`usbshim.so` (droidc, this repo).** Preloaded by `supervise` next to `ifshim.so`
   (`/usr/lib/droidc/usbshim.so`, `DROIDC_USBSHIM`, empty turns it off; only on glibc guests). It
   wraps `open`, `open64`, `openat`, `openat64` and their fortified `__open*_2` forms:

   - it calls the real `open` first, and keeps the result unless the path is a `/dev/bus/usb/BBB/DDD`
     node *and* what opened is a regular file (a placeholder). A real character device (root mode, a
     desktop Linux) is never touched;
   - for a placeholder it closes it, checks the server's `SO_PEERCRED` uid against its own real uid
     (taken from a socketpair, since proot's `-0` fakes `getuid()`), sends `usb.open` and returns the
     fd it gets, with `O_CLOEXEC` as the caller asked;
   - an error reply becomes `-1` with the errno above; an unreachable server (`ECONNREFUSED`, Selkie
     not running) is `ENODEV`.

   `USBSHIM_TRACE` logs each swap to stderr.

## Which tools this covers

- **fastboot** (Linux backend, `usb_linux.cpp`): scans `/sys/bus/usb/devices`, reads `busnum`,
  `devnum` and `serial`, opens `/dev/bus/usb/BBB/DDD` read-write, reads the descriptors from it and
  claims the interface. All of that is covered. It polls while it waits for a device, so it also finds
  the device again after `fastboot reboot bootloader` once Selkie has written the new entries.
- **adb** with its native Linux backend: `supervise` sets `ADB_LIBUSB=0` when the shim is preloaded
  and the variable is unset, so adb scans usbfs instead of using libusb.
- **libusb** programs: device discovery reads the same sysfs files and opens the same nodes, but
  `libusb_init` also opens a `NETLINK_KOBJECT_UEVENT` socket for hotplug and fails if the sandbox
  refuses it (the udev build needs `/run/udev` too). Not covered yet; see the follow-up issue.
  Programs that call `libusb_wrap_sys_device()` on an fd they got some other way need no discovery.
- Static binaries and programs that issue `open` syscalls directly are not covered (same limit as
  `ifshim`).

## Re-enumeration (bootloader, recovery, sideload)

A device that reboots into another mode leaves the bus and comes back with a new device number, and
Android's permission does not carry over: it asks again. Selkie:

- on `ACTION_USB_DEVICE_DETACHED`, removes the device's entries (an open fd then fails with `ENODEV`);
- on `ACTION_USB_DEVICE_ATTACHED`, asks for permission again (a notification the person taps, which
  opens Android's prompt) and writes the entries once it is granted;
- declares a `USB_DEVICE_ATTACHED` intent filter with a device filter for the adb interface
  (class `0xff`, subclass `0x42`, protocol `0x01`) and the fastboot interface (protocol `0x03`).
  Android's prompt for a matching device then offers "Always open Selkie", which grants it on every
  attach, so a flash that reboots the target more than once does not stop at each step.

## Not in this design

- A `droidc usb` command to list or request devices from a guest shell (the request would go over
  host-link to Selkie, which shows the prompt). Useful, not needed for the first flash.
- Hotplug events for libusb (synthesised `uevent`s on the shim's stand-in socket).
- Root mode: a guest running as root can open the real nodes, and the shim leaves those alone.

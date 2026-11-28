//! `LD_PRELOAD` shim that gives guest programs the USB devices Android granted (droidc #100).
//!
//! An Android app cannot open `/dev/bus/usb/BBB/DDD`; it gets an already-open usbfs fd from
//! `UsbManager.openDevice()` after Android's permission prompt. The host (Selkie) writes an empty
//! placeholder file at the device's usbfs path, and sysfs entries next to it, for each device the
//! person allowed. This library makes an `open()` of such a placeholder return the real usbfs fd,
//! which it asks the host for over an abstract Unix socket and receives as `SCM_RIGHTS`:
//!
//! - the real `open` runs first, and its result is kept unless the path is a usbfs node
//!   ([`node::parse`]) and the file it opened is a regular file (a placeholder): a real character
//!   device is never touched;
//! - for a placeholder, it sends `usb.open` ([`proto`]) to the host's socket (`DROIDC_USB_SOCKET`,
//!   default [`client::DEFAULT_SOCKET`]), after checking the host runs as the same uid, and returns
//!   the fd it gets back ([`client`]); an error reply becomes `-1` and an errno.
//!
//! The design, the host's half and the device trees are in `docs/design/usb.md`.
//!
//! Layout: [`node`] and [`proto`] are safe Rust with unit tests. [`client`] makes the socket calls,
//! and `ffi` exports the C symbols and falls through to the real libc functions (found with
//! `dlsym(RTLD_NEXT)`).
//!
//! Environment: `DROIDC_USB_SOCKET` (the host's abstract socket name), `USBSHIM_TRACE` (log each
//! swap to stderr), `USBSHIM_USBFS`
//! (another usbfs directory, for tests).

pub mod client;
pub mod node;
pub mod proto;

// The exports would override libc's own symbols in the unit-test binary.
#[cfg(not(test))]
mod ffi;

//! `LD_PRELOAD` shim that lists network interfaces where the kernel refuses the netlink socket glibc
//! asks for them (Android apps targeting API 30 and up, so every guest under Selkie).
//!
//! glibc's `getifaddrs()` and `if_nameindex()` read `RTM_GETLINK`/`RTM_GETADDR` dumps over a
//! `NETLINK_ROUTE` socket. Android's app sandbox refuses that bind, so both fail with `EACCES`, and
//! Node's `os.networkInterfaces()`, Python's `socket.if_nameindex()` and `hostname -I` fail with them
//! (Selkie #14). The legacy ioctls still work, so this library:
//!
//! - calls the real function first, and returns its answer whenever it succeeds;
//! - otherwise builds the list from `SIOCGIFCONF` (interfaces with an IPv4 address) and the
//!   per-interface `SIOCGIF*` ioctls, plus `/proc/net/if_inet6` when it is readable.
//!
//! The fallback has one `AF_PACKET` entry per interface (hardware address when the kernel gives it),
//! then the `AF_INET` and `AF_INET6` addresses, like glibc. It is allocated as one `malloc` block, as
//! glibc's is, so the real `freeifaddrs` (a plain `free`) releases it; `if_nameindex` allocates the
//! array and each name separately, as glibc's `if_freenameindex` expects. Neither is overridden.
//!
//! Not covered: programs that open netlink themselves (`ip`, Go's `net.Interfaces()`, static
//! binaries). That needs netlink emulation in proot.
//!
//! Layout: [`list`] is safe Rust with unit tests. `ffi` is the only module with `unsafe`: the C
//! exports, the ioctls and the C allocation.
//!
//! Environment: `IFSHIM_TRACE` (log the fallback to stderr), `IFSHIM_FORCE` (skip the real functions,
//! for tests).

pub mod list;

// The exports would override libc's own symbols in the unit-test binary.
#[cfg(not(test))]
mod ffi;

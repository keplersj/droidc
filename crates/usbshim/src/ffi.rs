//! The exported C symbols. Each one makes the real call first and hands the result to [`swap`],
//! which replaces a placeholder usbfs node with the fd the host sends.
//!
//! Variadic libc functions (`open`, `openat`) cannot be defined in stable Rust, so they are declared
//! with their fixed arguments, the mode included (as in cgshim). On x86_64 and aarch64 Linux a
//! variadic call passes integer arguments exactly like a fixed one, so the callee reads the same
//! registers; a caller that passed no mode leaves garbage in it, which only `O_CREAT` and
//! `O_TMPFILE` look at, and the real function gets it unchanged.

use crate::{client, node};
use libc::{c_char, c_int, mode_t};
use std::ffi::CStr;
use std::io::Write;
use std::os::fd::IntoRawFd;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// The next definition of a libc function after this library's, resolved once.
macro_rules! real {
    ($name:literal, $ty:ty) => {{
        static CELL: AtomicUsize = AtomicUsize::new(0);
        let mut p = CELL.load(Ordering::Relaxed);
        if p == 0 {
            // SAFETY: a NUL-terminated name; RTLD_NEXT is always a valid handle.
            p = unsafe { libc::dlsym(libc::RTLD_NEXT, concat!($name, "\0").as_ptr().cast()) }
                as usize;
            CELL.store(p, Ordering::Relaxed);
        }
        // SAFETY: the symbol is the libc function of that name, whose type `$ty` is.
        (p != 0).then(|| unsafe { std::mem::transmute::<usize, $ty>(p) })
    }};
}

fn set_errno(e: c_int) {
    // SAFETY: __errno_location is the calling thread's errno.
    unsafe { *libc::__errno_location() = e };
}

fn trace(msg: std::fmt::Arguments) {
    static T: OnceLock<bool> = OnceLock::new();
    if *T.get_or_init(|| std::env::var_os("USBSHIM_TRACE").is_some()) {
        let _ = writeln!(std::io::stderr(), "usbshim: {msg}");
    }
}

/// The usbfs directory: `/dev/bus/usb/`, or `USBSHIM_USBFS` for tests.
fn usbfs() -> &'static [u8] {
    static U: OnceLock<Vec<u8>> = OnceLock::new();
    U.get_or_init(|| match std::env::var_os("USBSHIM_USBFS") {
        Some(v) if !v.is_empty() => {
            let mut v = v.as_bytes().to_vec();
            if !v.ends_with(b"/") {
                v.push(b'/');
            }
            v
        }
        _ => node::USBFS.to_vec(),
    })
}

/// The guest path an open names: `path` itself when absolute, else joined to the directory of
/// `dirfd` (the working directory for `AT_FDCWD`). `None` when it can't be a usbfs node.
fn full_path(dirfd: c_int, path: &[u8]) -> Option<Vec<u8>> {
    if path.starts_with(b"/") {
        return Some(path.to_vec());
    }
    // A relative usbfs open names the device ("005") or "BBB/DDD"; skip the lookup for the rest.
    if !path.first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let dir = if dirfd == libc::AT_FDCWD {
        std::env::current_dir().ok()?
    } else {
        std::fs::read_link(format!("/proc/self/fd/{dirfd}")).ok()?
    };
    Some(node::join(dir.as_os_str().as_bytes(), path))
}

fn is_regular_file(fd: c_int) -> bool {
    // SAFETY: an all-zero stat is a valid out-buffer.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is the descriptor the real open just returned.
    unsafe { libc::fstat(fd, &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFREG }
}

/// What every export returns: the real result `r`, unless it opened a placeholder usbfs node, in
/// which case the placeholder is closed and the host's fd (or `-1` and an errno) takes its place.
fn swap(dirfd: c_int, path: *const c_char, flags: c_int, r: c_int) -> c_int {
    if r < 0 || path.is_null() {
        return r;
    }
    // SAFETY: `path` is the caller's NUL-terminated argument to an open function.
    let p = unsafe { CStr::from_ptr(path) }.to_bytes();
    let Some((bus, dev)) = full_path(dirfd, p).and_then(|f| node::parse_under(usbfs(), &f)) else {
        return r;
    };
    if !is_regular_file(r) {
        return r; // a real usbfs node (root mode, a desktop Linux)
    }
    // SAFETY: the placeholder's fd, which nothing else has seen yet.
    unsafe { libc::close(r) };
    match client::open(&client::socket_name(), bus, dev) {
        Ok(fd) => {
            let fd = fd.into_raw_fd();
            if flags & libc::O_CLOEXEC == 0 {
                // Received close-on-exec; the caller didn't ask for that.
                // SAFETY: an fd we own.
                unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
            }
            trace(format_args!("{bus:03}/{dev:03}: fd {fd} from the host"));
            fd
        }
        Err(e) => {
            trace(format_args!("{bus:03}/{dev:03}: {e}"));
            set_errno(e.errno());
            -1
        }
    }
}

type OpenFn = unsafe extern "C" fn(*const c_char, c_int, mode_t) -> c_int;
type OpenAtFn = unsafe extern "C" fn(c_int, *const c_char, c_int, mode_t) -> c_int;
type Open2Fn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
type OpenAt2Fn = unsafe extern "C" fn(c_int, *const c_char, c_int) -> c_int;

macro_rules! export_open {
    ($sym:ident, $name:literal) => {
        /// # Safety
        /// Called as libc's `open`; `path` is a NUL-terminated string.
        // The mode is a fixed argument (see the module docs). rustc's
        // `invalid_runtime_symbol_definitions` flags that for symbols std itself calls; the preload
        // tests exercise it. The lint is newer than some toolchains, hence `unknown_lints`.
        #[allow(unknown_lints, invalid_runtime_symbol_definitions)]
        #[no_mangle]
        pub unsafe extern "C" fn $sym(path: *const c_char, flags: c_int, mode: mode_t) -> c_int {
            let Some(real) = real!($name, OpenFn) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // SAFETY: the real function, with the caller's arguments.
            let r = unsafe { real(path, flags, mode) };
            swap(libc::AT_FDCWD, path, flags, r)
        }
    };
}

macro_rules! export_openat {
    ($sym:ident, $name:literal) => {
        /// # Safety
        /// Called as libc's `openat`; `path` is a NUL-terminated string.
        #[allow(unknown_lints, invalid_runtime_symbol_definitions)]
        #[no_mangle]
        pub unsafe extern "C" fn $sym(
            dirfd: c_int,
            path: *const c_char,
            flags: c_int,
            mode: mode_t,
        ) -> c_int {
            let Some(real) = real!($name, OpenAtFn) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // SAFETY: the real function, with the caller's arguments.
            let r = unsafe { real(dirfd, path, flags, mode) };
            swap(dirfd, path, flags, r)
        }
    };
}

// The fortified forms `_FORTIFY_SOURCE` builds call when the flags are known to need no mode.
macro_rules! export_open_2 {
    ($sym:ident, $name:literal) => {
        /// # Safety
        /// Called as glibc's `__open_2`; `path` is a NUL-terminated string.
        #[no_mangle]
        pub unsafe extern "C" fn $sym(path: *const c_char, flags: c_int) -> c_int {
            let Some(real) = real!($name, Open2Fn) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // SAFETY: the real function, with the caller's arguments.
            let r = unsafe { real(path, flags) };
            swap(libc::AT_FDCWD, path, flags, r)
        }
    };
}

macro_rules! export_openat_2 {
    ($sym:ident, $name:literal) => {
        /// # Safety
        /// Called as glibc's `__openat_2`; `path` is a NUL-terminated string.
        #[no_mangle]
        pub unsafe extern "C" fn $sym(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
            let Some(real) = real!($name, OpenAt2Fn) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // SAFETY: the real function, with the caller's arguments.
            let r = unsafe { real(dirfd, path, flags) };
            swap(dirfd, path, flags, r)
        }
    };
}

export_open!(open, "open");
export_open!(open64, "open64");
export_openat!(openat, "openat");
export_openat!(openat64, "openat64");
export_open_2!(__open_2, "__open_2");
export_open_2!(__open64_2, "__open64_2");
export_openat_2!(__openat_2, "__openat_2");
export_openat_2!(__openat64_2, "__openat64_2");

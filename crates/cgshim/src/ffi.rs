//! The exported C symbols. Everything unsafe in the crate is here, and none of it decides anything:
//! each function turns pointers into paths, asks the safe modules, and otherwise calls the real libc
//! function.
//!
//! Variadic libc functions (`open`, `openat`, `prctl`) cannot be defined in stable Rust, so they are
//! declared with their fixed arguments, the optional ones included. On x86_64 and aarch64 Linux a
//! variadic call passes integer arguments exactly like a fixed one, so the callee reads the same
//! registers. A caller that did not pass a mode leaves garbage in it, which only `O_CREAT` and
//! `O_TMPFILE` look at.

use crate::{paths, spawn, tree};
use libc::{c_char, c_int, c_short, c_ulong, mode_t, pid_t};
use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::io::{Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

// ---- plumbing ------------------------------------------------------------------------------------

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

thread_local! {
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

/// Run the shim's own logic, unless it is already running on this thread: its file work goes through
/// the hooked libc functions, which must then pass straight through. `None` means "do not intercept".
fn guarded<R>(f: impl FnOnce() -> R) -> Option<R> {
    BUSY.try_with(|b| {
        if b.replace(true) {
            return None;
        }
        let r = f();
        b.set(false);
        Some(r)
    })
    .ok()
    .flatten()
}

fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn set_errno(e: c_int) {
    // SAFETY: __errno_location is the calling thread's errno.
    unsafe { *libc::__errno_location() = e };
}

fn tracing() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| std::env::var_os("CGSHIM_TRACE").is_some())
}

fn trace(msg: std::fmt::Arguments) {
    if tracing() {
        let _ = writeln!(std::io::stderr(), "cgshim: {msg}");
    }
}

/// A failed call on a `/sys` path, whether or not it is under the cgroup tree.
fn trace_sys(op: &str, dirfd: c_int, p: &Path, e: c_int) {
    if p.starts_with("/sys") {
        trace(format_args!(
            "{op}({dirfd}) {}: {}",
            p.display(),
            std::io::Error::from_raw_os_error(e)
        ));
    }
}

/// The path a C string names.
///
/// # Safety
/// `p` is null or a NUL-terminated string that outlives `'a`.
unsafe fn path_arg<'a>(p: *const c_char) -> Option<&'a Path> {
    if p.is_null() {
        return None;
    }
    // SAFETY: by the caller's contract.
    Some(Path::new(std::ffi::OsStr::from_bytes(
        unsafe { CStr::from_ptr(p) }.to_bytes(),
    )))
}

/// `p`, made absolute against `dirfd`, when it is under the tree.
fn tree_path(dirfd: c_int, p: &Path) -> Option<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else if dirfd == libc::AT_FDCWD {
        return None;
    } else {
        std::fs::read_link(format!("/proc/self/fd/{dirfd}"))
            .ok()?
            .join(p)
    };
    paths::under(paths::root(), &abs).then_some(abs)
}

fn fd_in_tree(fd: c_int) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).is_ok_and(|t| paths::under(paths::root(), &t))
}

fn cstring(p: &Path) -> Option<CString> {
    CString::new(p.as_os_str().as_bytes()).ok()
}

// ---- /proc/<pid>/cgroup --------------------------------------------------------------------------

/// A readable memfd holding what `/proc/<pid>/cgroup` would say if the tree were a real hierarchy.
fn fake_cgroup_fd(pid: i32) -> Option<c_int> {
    let pid = if pid == 0 {
        std::process::id() as i32
    } else {
        pid
    };
    let body = paths::cgroup_file_body(tree::find_cgroup(paths::root(), pid).as_deref());
    trace(format_args!("/proc/{pid}/cgroup -> {}", body.trim_end()));
    // SAFETY: memfd_create takes a NUL-terminated name and flags.
    let fd = unsafe { libc::syscall(libc::SYS_memfd_create, c"cgroup".as_ptr(), 0) } as c_int;
    if fd < 0 {
        return None;
    }
    // SAFETY: a fresh fd that nothing else owns.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    (f.write_all(body.as_bytes()).is_ok() && f.rewind().is_ok()).then(|| f.into_raw_fd())
}

// ---- open family ----------------------------------------------------------------------------------

/// What every `open*` does before and after the real call; `call` makes the real call with the flags.
fn open_with(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    call: &dyn Fn(c_int) -> c_int,
) -> c_int {
    // SAFETY: `path` is the caller's argument to an open function.
    let Some(p) = (unsafe { path_arg(path) }) else {
        return call(flags);
    };
    let pre = guarded(|| {
        if let Some(pid) = paths::proc_cgroup_pid(p.as_os_str().as_bytes()) {
            if let Some(fd) = fake_cgroup_fd(pid) {
                return Err(fd);
            }
        }
        let append =
            tree_path(dirfd, p).is_some_and(|a| paths::wants_append(paths::root(), &a, flags));
        Ok(if append {
            flags | libc::O_APPEND
        } else {
            flags
        })
    });
    let flags = match pre {
        Some(Err(fd)) => return fd,
        Some(Ok(f)) => f,
        None => return call(flags),
    };
    let mut r = call(flags);
    if r < 0 {
        let e = errno();
        // A cgroup directory without its cgroup.* files: make them and try once more.
        let repaired = e == libc::ENOENT
            && guarded(|| {
                tree_path(dirfd, p)
                    .and_then(|a| paths::cgroup_file_dir(&a).map(Path::to_path_buf))
                    .is_some_and(|dir| {
                        let ok = tree::repair(&dir);
                        if ok {
                            trace(format_args!("repaired {}", dir.display()));
                        }
                        ok
                    })
            })
            .unwrap_or(false);
        if repaired {
            r = call(flags);
        }
        if r < 0 {
            let e = if repaired { errno() } else { e };
            trace_sys("open", dirfd, p, e);
            set_errno(e);
        }
    }
    r
}

type OpenFn = unsafe extern "C" fn(*const c_char, c_int, mode_t) -> c_int;
type OpenAtFn = unsafe extern "C" fn(c_int, *const c_char, c_int, mode_t) -> c_int;

macro_rules! export_open {
    ($sym:ident, $name:literal) => {
        /// # Safety
        /// Called as libc's `open`; `path` is a NUL-terminated string.
        // Stable Rust cannot define a variadic function, so this takes the mode as a fixed argument (see
        // the module docs). rustc's `invalid_runtime_symbol_definitions` flags that for the symbols std
        // itself calls; on x86_64 and aarch64 Linux a variadic call passes the same registers, which the
        // preload tests exercise. The lint is newer than some toolchains, hence `unknown_lints`.
        #[allow(unknown_lints, invalid_runtime_symbol_definitions)]
        #[no_mangle]
        pub unsafe extern "C" fn $sym(path: *const c_char, flags: c_int, mode: mode_t) -> c_int {
            let Some(real) = real!($name, OpenFn) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // SAFETY: the real function, with the caller's arguments.
            open_with(libc::AT_FDCWD, path, flags, &|f| unsafe {
                real(path, f, mode)
            })
        }
    };
}
macro_rules! export_openat {
    ($sym:ident, $name:literal) => {
        /// # Safety
        /// Called as libc's `openat`; `path` is a NUL-terminated string.
        // Stable Rust cannot define a variadic function, so this takes the mode as a fixed argument (see
        // the module docs). rustc's `invalid_runtime_symbol_definitions` flags that for the symbols std
        // itself calls; on x86_64 and aarch64 Linux a variadic call passes the same registers, which the
        // preload tests exercise. The lint is newer than some toolchains, hence `unknown_lints`.
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
            open_with(dirfd, path, flags, &|f| unsafe {
                real(dirfd, path, f, mode)
            })
        }
    };
}
export_open!(open, "open");
export_open!(open64, "open64");
export_openat!(openat, "openat");
export_openat!(openat64, "openat64");

type FopenFn = unsafe extern "C" fn(*const c_char, *const c_char) -> *mut libc::FILE;

fn fopen_with(path: *const c_char, mode: *const c_char, real: FopenFn) -> *mut libc::FILE {
    // SAFETY: `path` is the caller's argument to fopen.
    if let Some(p) = unsafe { path_arg(path) } {
        let fake =
            guarded(|| paths::proc_cgroup_pid(p.as_os_str().as_bytes()).and_then(fake_cgroup_fd))
                .flatten();
        if let Some(fd) = fake {
            // SAFETY: an fd we own and a constant mode.
            return unsafe { libc::fdopen(fd, c"r".as_ptr()) };
        }
    }
    // SAFETY: the real function, with the caller's arguments.
    unsafe { real(path, mode) }
}

/// # Safety
/// Called as libc's `fopen`.
#[no_mangle]
pub unsafe extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> *mut libc::FILE {
    match real!("fopen", FopenFn) {
        Some(real) => fopen_with(path, mode, real),
        None => std::ptr::null_mut(),
    }
}

/// # Safety
/// Called as libc's `fopen64`.
#[no_mangle]
pub unsafe extern "C" fn fopen64(path: *const c_char, mode: *const c_char) -> *mut libc::FILE {
    match real!("fopen64", FopenFn) {
        Some(real) => fopen_with(path, mode, real),
        None => std::ptr::null_mut(),
    }
}

// ---- statfs ---------------------------------------------------------------------------------------

macro_rules! export_statfs {
    ($sym:ident, $name:literal, $ty:ty) => {
        /// # Safety
        /// Called as libc's `statfs`.
        #[no_mangle]
        pub unsafe extern "C" fn $sym(path: *const c_char, buf: *mut $ty) -> c_int {
            let Some(real) = real!(
                $name,
                unsafe extern "C" fn(*const c_char, *mut $ty) -> c_int
            ) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // SAFETY: the real function, with the caller's arguments.
            let r = unsafe { real(path, buf) };
            // SAFETY: `path` is the caller's.
            let hit = r == 0
                && guarded(|| {
                    unsafe { path_arg(path) }.is_some_and(|p| paths::under(paths::root(), p))
                }) == Some(true);
            if hit {
                // SAFETY: the call succeeded, so `buf` is a filled statfs.
                unsafe { (*buf).f_type = paths::CGROUP2_MAGIC as _ };
            }
            r
        }
    };
}
macro_rules! export_fstatfs {
    ($sym:ident, $name:literal, $ty:ty) => {
        /// # Safety
        /// Called as libc's `fstatfs`.
        #[no_mangle]
        pub unsafe extern "C" fn $sym(fd: c_int, buf: *mut $ty) -> c_int {
            let Some(real) = real!($name, unsafe extern "C" fn(c_int, *mut $ty) -> c_int) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // SAFETY: the real function, with the caller's arguments.
            let r = unsafe { real(fd, buf) };
            if r == 0 && guarded(|| fd_in_tree(fd)) == Some(true) {
                // SAFETY: the call succeeded, so `buf` is a filled statfs.
                unsafe { (*buf).f_type = paths::CGROUP2_MAGIC as _ };
            }
            r
        }
    };
}
export_statfs!(statfs, "statfs", libc::statfs);
export_statfs!(statfs64, "statfs64", libc::statfs64);
export_fstatfs!(fstatfs, "fstatfs", libc::statfs);
export_fstatfs!(fstatfs64, "fstatfs64", libc::statfs64);

// ---- mkdir and friends ----------------------------------------------------------------------------

/// # Safety
/// Called as libc's `mkdir`.
#[no_mangle]
pub unsafe extern "C" fn mkdir(path: *const c_char, mode: mode_t) -> c_int {
    let Some(real) = real!(
        "mkdir",
        unsafe extern "C" fn(*const c_char, mode_t) -> c_int
    ) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: the real function, with the caller's arguments.
    let r = unsafe { real(path, mode) };
    made_dir(libc::AT_FDCWD, path, r, "mkdir");
    r
}

/// # Safety
/// Called as libc's `mkdirat`.
#[no_mangle]
pub unsafe extern "C" fn mkdirat(dirfd: c_int, path: *const c_char, mode: mode_t) -> c_int {
    let Some(real) = real!(
        "mkdirat",
        unsafe extern "C" fn(c_int, *const c_char, mode_t) -> c_int
    ) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: the real function, with the caller's arguments.
    let r = unsafe { real(dirfd, path, mode) };
    made_dir(dirfd, path, r, "mkdirat");
    r
}

/// After a `mkdir*`: give a new directory in the tree its cgroup files; trace a failure.
fn made_dir(dirfd: c_int, path: *const c_char, result: c_int, op: &str) {
    let e = errno();
    // SAFETY: `path` is the caller's argument.
    let Some(p) = (unsafe { path_arg(path) }) else {
        return;
    };
    guarded(|| {
        if result == 0 {
            if let Some(dir) = tree_path(dirfd, p) {
                for (name, err) in tree::populate(&dir) {
                    trace(format_args!("populate {}/{name}: {err}", dir.display()));
                }
            }
        } else {
            trace_sys(op, dirfd, p, e);
        }
    });
    set_errno(e);
}

/// # Safety
/// Called as libc's `rmdir`.
#[no_mangle]
pub unsafe extern "C" fn rmdir(path: *const c_char) -> c_int {
    let Some(real) = real!("rmdir", unsafe extern "C" fn(*const c_char) -> c_int) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: `path` is the caller's argument.
    if let Some(p) = unsafe { path_arg(path) } {
        guarded(|| {
            if paths::under(paths::root(), p) {
                tree::unpopulate(p);
            }
        });
    }
    // SAFETY: the real function, with the caller's argument.
    unsafe { real(path) }
}

/// # Safety
/// Called as libc's `unlinkat`.
#[no_mangle]
pub unsafe extern "C" fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
    let Some(real) = real!(
        "unlinkat",
        unsafe extern "C" fn(c_int, *const c_char, c_int) -> c_int
    ) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: `path` is the caller's argument.
    if let (true, Some(p)) = (flags & libc::AT_REMOVEDIR != 0, unsafe { path_arg(path) }) {
        guarded(|| {
            if let Some(dir) = tree_path(dirfd, p) {
                tree::unpopulate(&dir);
            }
        });
    }
    // SAFETY: the real function, with the caller's arguments.
    unsafe { real(dirfd, path, flags) }
}

// ---- spawning units -------------------------------------------------------------------------------

fn remembered() -> &'static spawn::Remembered {
    static R: OnceLock<spawn::Remembered> = OnceLock::new();
    R.get_or_init(spawn::Remembered::default)
}

/// # Safety
/// Called as glibc's `posix_spawnattr_setcgroup_np` (2.39); `attr` is the caller's attribute object.
#[no_mangle]
pub unsafe extern "C" fn posix_spawnattr_setcgroup_np(
    attr: *mut libc::posix_spawnattr_t,
    cgroup: c_int,
) -> c_int {
    if let Ok(dir) = std::fs::read_link(format!("/proc/self/fd/{cgroup}")) {
        remembered().remember(attr as usize, dir);
    }
    0
}

/// # Safety
/// Called as libc's `posix_spawnattr_setflags`.
#[no_mangle]
pub unsafe extern "C" fn posix_spawnattr_setflags(
    attr: *mut libc::posix_spawnattr_t,
    flags: c_short,
) -> c_int {
    let Some(real) = real!(
        "posix_spawnattr_setflags",
        unsafe extern "C" fn(*mut libc::posix_spawnattr_t, c_short) -> c_int
    ) else {
        return libc::ENOSYS;
    };
    // SAFETY: the real function; the flag it must not see is dropped (the shim does that work).
    unsafe { real(attr, flags & !spawn::POSIX_SPAWN_SETCGROUP) }
}

/// # Safety
/// Called as libc's `posix_spawnattr_destroy`.
#[no_mangle]
pub unsafe extern "C" fn posix_spawnattr_destroy(attr: *mut libc::posix_spawnattr_t) -> c_int {
    let Some(real) = real!(
        "posix_spawnattr_destroy",
        unsafe extern "C" fn(*mut libc::posix_spawnattr_t) -> c_int
    ) else {
        return libc::ENOSYS;
    };
    remembered().forget(attr as usize);
    // SAFETY: the real function, with the caller's argument.
    unsafe { real(attr) }
}

/// The path to really spawn: the file an fd names, when proot could not run it through
/// `/proc/self/fd`.
fn spawn_path(path: *const c_char) -> Option<CString> {
    // SAFETY: `path` is the caller's argument to a spawn function.
    let p = unsafe { path_arg(path) }?;
    let executor = std::env::var("CGSHIM_EXECUTOR").ok();
    let chosen = spawn::exec_path(
        p,
        |l| std::fs::read_link(l).ok(),
        |x| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(x).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        },
        executor.as_deref(),
    )?;
    trace(format_args!(
        "spawn {} -> {}",
        p.display(),
        chosen.display()
    ));
    cstring(&chosen)
}

type SpawnFn = unsafe extern "C" fn(
    *mut pid_t,
    *const c_char,
    *const libc::posix_spawn_file_actions_t,
    *const libc::posix_spawnattr_t,
    *const *mut c_char,
    *const *mut c_char,
) -> c_int;
type PidfdSpawnFn = unsafe extern "C" fn(
    *mut c_int,
    *const c_char,
    *const libc::posix_spawn_file_actions_t,
    *const libc::posix_spawnattr_t,
    *const *mut c_char,
    *const *mut c_char,
) -> c_int;

/// # Safety
/// Called as libc's `posix_spawn`.
#[no_mangle]
pub unsafe extern "C" fn posix_spawn(
    pid: *mut pid_t,
    path: *const c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attr: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    let Some(real) = real!("posix_spawn", SpawnFn) else {
        return libc::ENOSYS;
    };
    let fixed = guarded(|| spawn_path(path)).flatten();
    let use_path = fixed.as_ref().map_or(path, |c| c.as_ptr());
    // SAFETY: the real function; `use_path` outlives the call.
    let r = unsafe { real(pid, use_path, file_actions, attr, argv, envp) };
    if r == 0 && !pid.is_null() {
        if let Some(cg) = remembered().recall(attr as usize) {
            // SAFETY: the call succeeded, so `pid` holds the child's pid.
            let child = unsafe { *pid };
            guarded(|| tree::attach(&cg, child));
        }
    }
    r
}

/// # Safety
/// Called as glibc's `pidfd_spawn` (2.39).
#[no_mangle]
pub unsafe extern "C" fn pidfd_spawn(
    pidfd: *mut c_int,
    path: *const c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attr: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    let Some(real) = real!("pidfd_spawn", PidfdSpawnFn) else {
        return libc::ENOSYS;
    };
    let fixed = guarded(|| spawn_path(path)).flatten();
    let use_path = fixed.as_ref().map_or(path, |c| c.as_ptr());
    // SAFETY: the real function; `use_path` outlives the call.
    let r = unsafe { real(pidfd, use_path, file_actions, attr, argv, envp) };
    if r == 0 && !pidfd.is_null() {
        if let Some(cg) = remembered().recall(attr as usize) {
            // SAFETY: the call succeeded, so `pidfd` holds a pidfd.
            let fd = unsafe { *pidfd };
            guarded(|| {
                let info =
                    std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap_or_default();
                if let Some(child) = info
                    .lines()
                    .find_map(|l| l.strip_prefix("Pid:")?.trim().parse::<i32>().ok())
                {
                    tree::attach(&cg, child);
                }
            });
        }
    }
    r
}

// ---- smaller fixes --------------------------------------------------------------------------------

macro_rules! export_readdir {
    ($sym:ident, $name:literal, $ty:ty) => {
        /// # Safety
        /// Called as libc's `readdir`; `dir` is an open directory stream.
        #[no_mangle]
        pub unsafe extern "C" fn $sym(dir: *mut libc::DIR) -> *mut $ty {
            let Some(real) = real!($name, unsafe extern "C" fn(*mut libc::DIR) -> *mut $ty) else {
                return std::ptr::null_mut();
            };
            // SAFETY: the real function; a non-null entry is valid until the next call.
            let e = unsafe { real(dir) };
            // proot's link2symlink makes lstat/readlink call such a file regular while getdents still
            // reports the symlink; systemd believes d_type, readlinks every unit file, gets EINVAL and
            // loads no units. DT_UNKNOWN makes callers stat(), which gives the consistent answer.
            // SAFETY: `e` is null or a valid entry.
            if let Some(entry) = unsafe { e.as_mut() } {
                if entry.d_type == libc::DT_LNK {
                    entry.d_type = libc::DT_UNKNOWN;
                }
            }
            e
        }
    };
}
export_readdir!(readdir, "readdir", libc::dirent);
export_readdir!(readdir64, "readdir64", libc::dirent64);

const PR_SET_SECUREBITS: c_int = 28;
const PR_SET_SECCOMP: c_int = 22;
const SECCOMP_MODE_FILTER: c_ulong = 2;
const SECCOMP_SET_MODE_FILTER: c_ulong = 1;

/// A unit's `SystemCallFilter=`, `MemoryDenyWriteExecute=` and the like are installed as seccomp filters
/// by the executor. Under proot on Android they only do harm: a syscall that the unit's filter does not
/// list, or that proot rewrites, kills the service with SIGSYS (PipeWire, on the API 34 emulator), and
/// there is no sandbox here to keep: the guest already runs inside the app's. So a filter install
/// reports success and installs nothing. Strict mode and everything else pass through.
fn seccomp_filter_install(option: c_int, mode: c_ulong) -> bool {
    option == PR_SET_SECCOMP && mode == SECCOMP_MODE_FILTER
}

/// The executor calls `prctl(PR_SET_SECUREBITS)` for every service, which an app uid (no
/// `CAP_SETPCAP`) is refused with EPERM: the unit then fails with 213/SECUREBITS before it starts.
/// There are no capabilities here to lock down, so say it worked.
///
/// # Safety
/// Called as libc's `prctl`; the arguments are passed through untouched.
#[allow(unknown_lints, invalid_runtime_symbol_definitions)]
#[no_mangle]
pub unsafe extern "C" fn prctl(
    option: c_int,
    a2: c_ulong,
    a3: c_ulong,
    a4: c_ulong,
    a5: c_ulong,
) -> c_int {
    let Some(real) = real!(
        "prctl",
        unsafe extern "C" fn(c_int, c_ulong, c_ulong, c_ulong, c_ulong) -> c_int
    ) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    if seccomp_filter_install(option, a2) {
        trace(format_args!("prctl(PR_SET_SECCOMP, FILTER) skipped"));
        return 0;
    }
    // SAFETY: the real function, with the caller's arguments.
    let r = unsafe { real(option, a2, a3, a4, a5) };
    if r < 0 && errno() == libc::EPERM && option == PR_SET_SECUREBITS {
        trace(format_args!(
            "PR_SET_SECUREBITS {a2:#x} refused, pretending"
        ));
        return 0;
    }
    r
}

/// libseccomp (which systemd dlopens) installs a filter with `syscall(__NR_seccomp, ...)`, not `prctl`.
/// Every other call passes through untouched.
///
/// # Safety
/// Called as libc's `syscall`; the arguments are passed through untouched.
#[allow(unknown_lints, invalid_runtime_symbol_definitions)]
#[no_mangle]
pub unsafe extern "C" fn syscall(
    number: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
    a5: libc::c_long,
    a6: libc::c_long,
) -> libc::c_long {
    if number == libc::SYS_seccomp && a1 as c_ulong == SECCOMP_SET_MODE_FILTER {
        trace(format_args!("seccomp(SET_MODE_FILTER) skipped"));
        return 0;
    }
    type SyscallFn = unsafe extern "C" fn(
        libc::c_long,
        libc::c_long,
        libc::c_long,
        libc::c_long,
        libc::c_long,
        libc::c_long,
        libc::c_long,
    ) -> libc::c_long;
    let Some(real) = real!("syscall", SyscallFn) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: the real function, with the caller's arguments (unused ones are whatever the caller left).
    unsafe { real(number, a1, a2, a3, a4, a5, a6) }
}

/// Debugging aid, silent unless `CGSHIM_TRACE` is set: say which unix socket a failed `connect` wanted.
///
/// # Safety
/// Called as libc's `connect`.
#[no_mangle]
pub unsafe extern "C" fn connect(
    fd: c_int,
    addr: *const libc::sockaddr,
    len: libc::socklen_t,
) -> c_int {
    let Some(real) = real!(
        "connect",
        unsafe extern "C" fn(c_int, *const libc::sockaddr, libc::socklen_t) -> c_int
    ) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: the real function, with the caller's arguments.
    let r = unsafe { real(fd, addr, len) };
    if r < 0 && tracing() {
        let e = errno();
        // SAFETY: `addr` is readable for `len` bytes by connect's contract.
        if let Some(a) = unsafe { addr.as_ref() } {
            if i32::from(a.sa_family) == libc::AF_UNIX {
                let n = (len as usize).saturating_sub(2).min(108);
                // SAFETY: a sockaddr_un is `len` bytes long; `sun_path` starts after the 2-byte family.
                let raw = unsafe { std::slice::from_raw_parts(addr.cast::<u8>().add(2), n) };
                let name = if raw.first() == Some(&0) {
                    &raw[1..]
                } else {
                    raw
                };
                let name = name.split(|&b| b == 0).next().unwrap_or(&[]);
                trace(format_args!(
                    "connect({}) failed: {}",
                    String::from_utf8_lossy(name),
                    std::io::Error::from_raw_os_error(e)
                ));
            }
        }
        set_errno(e);
    }
    r
}

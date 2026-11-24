//! Loads the built shim into a child process with `LD_PRELOAD` and checks its answers to
//! `getifaddrs` and `if_nameindex` against glibc's own netlink answer on the same machine. The child
//! is this test binary itself (the `helper` test), so the lists are freed with glibc's own
//! `freeifaddrs` and `if_freenameindex`: a list laid out wrong aborts there.
//!
//! The child can deny itself netlink sockets with a seccomp filter (`EACCES`, as Android's sandbox
//! answers), which reproduces Selkie #14 without a phone.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

/// The built shim. `cargo test` does not build a cdylib for an integration test, so build it here.
fn shim() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let mut build =
                Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
            build.args([
                "build",
                "-p",
                "ifshim",
                "--manifest-path",
                concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            ]);
            if !cfg!(debug_assertions) {
                build.arg("--release");
            }
            assert!(
                build.status().expect("run cargo").success(),
                "cargo build -p ifshim failed"
            );
            let exe = std::env::current_exe().unwrap();
            exe.ancestors()
                .map(|dir| dir.join("libifshim.so"))
                .find(|so| so.is_file())
                .unwrap_or_else(|| panic!("libifshim.so not found above {}", exe.display()))
        })
        .clone()
}

/// Child half: list what `getifaddrs` and `if_nameindex` return, one line each, then free them with
/// glibc's functions. Does nothing when run as an ordinary test.
#[test]
fn helper() {
    if std::env::var_os("IFSHIM_HELPER").is_none() {
        return;
    }
    // The harness has already printed "test helper ... " on the current line.
    println!();
    if std::env::var_os("IFSHIM_DENY_NETLINK").is_some() {
        deny_netlink();
    }
    let mut out = String::new();
    // SAFETY: the C API as documented; each list is read before it is freed.
    unsafe {
        let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifa) != 0 {
            println!("error getifaddrs {}", std::io::Error::last_os_error());
            return;
        }
        let mut p = ifa;
        while !p.is_null() {
            let e = &*p;
            let name = std::ffi::CStr::from_ptr(e.ifa_name).to_string_lossy();
            let addr = describe(e.ifa_addr);
            // Flags that come and go with traffic or promiscuous mode are left out of the comparison.
            let flags =
                e.ifa_flags & (libc::IFF_UP | libc::IFF_LOOPBACK | libc::IFF_BROADCAST) as u32;
            let mask = describe(e.ifa_netmask);
            out += &format!("ifa {name} {flags:#x} {addr} {mask}\n");
            p = e.ifa_next;
        }
        libc::freeifaddrs(ifa);

        let ni = libc::if_nameindex();
        if ni.is_null() {
            println!("error if_nameindex {}", std::io::Error::last_os_error());
            return;
        }
        let mut i = 0;
        while (*ni.add(i)).if_index != 0 {
            let e = &*ni.add(i);
            let name = std::ffi::CStr::from_ptr(e.if_name).to_string_lossy();
            out += &format!("idx {} {name}\n", e.if_index);
            i += 1;
        }
        libc::if_freenameindex(ni);
    }
    print!("{out}");
}

/// Makes `socket(AF_NETLINK, ...)` fail with `EACCES` in this process and its children.
fn deny_netlink() {
    // Classic BPF over struct seccomp_data: nr at offset 0, the low half of args[0] at 16.
    let f = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
    const LD_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JEQ: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const RET: u16 = 0x06; // BPF_RET | BPF_K
    let mut prog = [
        f(LD_ABS, 0, 0, 0),
        f(JEQ, 0, 3, libc::SYS_socket as u32),
        f(LD_ABS, 0, 0, 16),
        f(JEQ, 0, 1, libc::AF_NETLINK as u32),
        f(RET, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EACCES as u32),
        f(RET, 0, 0, libc::SECCOMP_RET_ALLOW),
    ];
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_mut_ptr(),
    };
    // SAFETY: a valid filter program that outlives the call.
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        assert_eq!(
            libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &fprog),
            0,
            "seccomp: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// SAFETY: `a` is null or a sockaddr of the family it says.
unsafe fn describe(a: *const libc::sockaddr) -> String {
    if a.is_null() {
        return "-".into();
    }
    match (*a).sa_family as i32 {
        libc::AF_INET => {
            let s = &*(a as *const libc::sockaddr_in);
            std::net::Ipv4Addr::from(s.sin_addr.s_addr.to_ne_bytes()).to_string()
        }
        libc::AF_INET6 => {
            let s = &*(a as *const libc::sockaddr_in6);
            format!(
                "{}%{}",
                std::net::Ipv6Addr::from(s.sin6_addr.s6_addr),
                s.sin6_scope_id
            )
        }
        libc::AF_PACKET => {
            let s = &*(a as *const libc::sockaddr_ll);
            format!("packet#{}", s.sll_ifindex)
        }
        f => format!("family{f}"),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// No shim: glibc's netlink answer.
    Real,
    /// The shim, told to skip the real functions.
    Forced,
    /// The shim, with netlink denied, as on Android.
    Denied,
    /// No shim, with netlink denied: the bug.
    DeniedNoShim,
}

fn run(mode: Mode) -> BTreeSet<String> {
    let preload = matches!(mode, Mode::Forced | Mode::Denied);
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", "helper", "--nocapture", "--test-threads=1"])
        .env("IFSHIM_HELPER", "1");
    if preload {
        c.env("LD_PRELOAD", shim()).env("IFSHIM_TRACE", "1");
    }
    if mode == Mode::Forced {
        c.env("IFSHIM_FORCE", "1");
    }
    if matches!(mode, Mode::Denied | Mode::DeniedNoShim) {
        c.env("IFSHIM_DENY_NETLINK", "1");
    }
    let out = c.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "helper failed (preload {preload}): {}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    if preload {
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("ifshim: fallback found"),
            "the fallback did not run"
        );
    }
    stdout
        .lines()
        .filter(|l| l.starts_with("ifa ") || l.starts_with("idx ") || l.starts_with("error "))
        .map(str::to_string)
        .collect()
}

#[test]
fn fallback_matches_netlink() {
    check(&run(Mode::Real), &run(Mode::Forced));
}

#[test]
fn denied_netlink_falls_back() {
    let bug = run(Mode::DeniedNoShim);
    assert!(
        bug.iter().any(|l| l.starts_with("error getifaddrs")),
        "netlink was not denied, so this test proves nothing: {bug:#?}"
    );
    check(&run(Mode::Real), &run(Mode::Denied));
}

fn check(real: &BTreeSet<String>, shim: &BTreeSet<String>) {
    assert!(!real.iter().any(|l| l.starts_with("error ")), "{real:#?}");
    assert!(!shim.iter().any(|l| l.starts_with("error ")), "{shim:#?}");
    assert!(
        shim.iter()
            .any(|l| l.starts_with("ifa lo ") && l.contains(" 127.0.0.1 255.0.0.0")),
        "no loopback address: {shim:#?}"
    );
    assert!(
        shim.iter()
            .any(|l| l.starts_with("idx ") && l.ends_with(" lo")),
        "no loopback index: {shim:#?}"
    );
    // The fallback can only miss things (interfaces that are in no file and have no IPv4 address);
    // whatever it says must be what netlink says.
    let extra: Vec<&String> = shim.difference(real).collect();
    assert!(
        extra.is_empty(),
        "fallback said what netlink didn't: {extra:#?}\nnetlink: {real:#?}"
    );
    // Every IPv4 address netlink has, the fallback has (SIOCGIFCONF lists them all).
    let missing: Vec<&String> = real
        .iter()
        .filter(|l| l.starts_with("ifa ") && l.split(' ').nth(3).is_some_and(|a| a.contains('.')))
        .filter(|l| !shim.contains(*l))
        .collect();
    assert!(
        missing.is_empty(),
        "fallback missed IPv4 addresses: {missing:#?}"
    );
}

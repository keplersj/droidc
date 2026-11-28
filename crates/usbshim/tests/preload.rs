//! Loads the built shim into a child process with `LD_PRELOAD` and opens a placeholder usbfs node
//! while a fake host listens on an abstract socket, as Selkie does. The child is this test binary
//! itself (the `helper` test); `USBSHIM_USBFS` moves the usbfs directory into a temp dir.

use std::io::{BufRead, BufReader};
use std::os::fd::{AsFd, AsRawFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
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
                "usbshim",
                "--manifest-path",
                concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            ]);
            if !cfg!(debug_assertions) {
                build.arg("--release");
            }
            assert!(
                build.status().expect("run cargo").success(),
                "cargo build -p usbshim failed"
            );
            let exe = std::env::current_exe().unwrap();
            exe.ancestors()
                .map(|dir| dir.join("libusbshim.so"))
                .find(|so| so.is_file())
                .unwrap_or_else(|| panic!("libusbshim.so not found above {}", exe.display()))
        })
        .clone()
}

/// Child half: open `USBSHIM_OPEN` (with `openat` on its directory when `USBSHIM_AT` is set) and
/// print `ok <contents> cloexec=<0|1>` or `err <errno>`. Does nothing when run as an ordinary test.
#[test]
fn helper() {
    let Some(path) = std::env::var_os("USBSHIM_OPEN") else {
        return;
    };
    println!();
    let path = PathBuf::from(path);
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: plain libc calls on strings and fds owned here.
    unsafe {
        let fd = if std::env::var_os("USBSHIM_AT").is_some() {
            let dir = libc::open(
                c(path.parent().unwrap()).as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY,
            );
            assert!(dir >= 0);
            libc::openat(
                dir,
                c(Path::new(path.file_name().unwrap())).as_ptr(),
                libc::O_RDWR,
            )
        } else {
            libc::open(c(&path).as_ptr(), libc::O_RDWR)
        };
        if fd < 0 {
            println!(
                "err {}",
                std::io::Error::last_os_error().raw_os_error().unwrap()
            );
            return;
        }
        let mut buf = [0u8; 64];
        let n = libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
        let cloexec = libc::fcntl(fd, libc::F_GETFD) & libc::FD_CLOEXEC;
        println!(
            "ok {} cloexec={cloexec}",
            String::from_utf8_lossy(&buf[..n.max(0) as usize])
        );
    }
}

/// What the fake host answers.
#[derive(Clone, Copy)]
enum Host {
    Grant,
    Refuse,
}

fn send_with_fd(sock: &UnixStream, line: &[u8], fd: Option<std::os::fd::BorrowedFd>) {
    let mut control = [0u64; 8];
    let mut iov = libc::iovec {
        iov_base: line.as_ptr() as *mut _,
        iov_len: line.len(),
    };
    // SAFETY: test-only sendmsg with buffers that outlive the call.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if let Some(fd) = fd {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(4) as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(4) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>(), fd.as_raw_fd());
        }
        assert_eq!(
            libc::sendmsg(sock.as_raw_fd(), &msg, 0),
            line.len() as isize
        );
    }
}

/// A temp dir with a placeholder node `usb/001/005` and a "device" file the host hands out.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("usbshim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("usb/001")).unwrap();
        std::fs::write(dir.join("usb/001/005"), b"").unwrap();
        std::fs::write(dir.join("device"), b"descriptors").unwrap();
        Fixture { dir }
    }

    /// Serves one request on the abstract socket `name`; returns the request line.
    fn host(&self, name: &str, how: Host) -> std::thread::JoinHandle<String> {
        let l = UnixListener::bind_addr(&SocketAddr::from_abstract_name(name).unwrap()).unwrap();
        let device = self.dir.join("device");
        std::thread::spawn(move || {
            let (c, _) = l.accept().unwrap();
            let mut req = String::new();
            BufReader::new(&c).read_line(&mut req).unwrap();
            let v: serde_json::Value = serde_json::from_str(&req).unwrap();
            match how {
                Host::Grant => {
                    let f = std::fs::File::open(&device).unwrap();
                    let line = format!("{{\"id\":{},\"result\":{{}}}}\n", v["id"]);
                    send_with_fd(&c, line.as_bytes(), Some(f.as_fd()));
                }
                Host::Refuse => {
                    let line = format!(
                        "{{\"id\":{},\"error\":{{\"kind\":\"not-allowed\",\"message\":\"no\"}}}}\n",
                        v["id"]
                    );
                    send_with_fd(&c, line.as_bytes(), None);
                }
            }
            req
        })
    }

    fn run(&self, name: &str, open: &Path, at: bool) -> String {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "helper", "--nocapture", "--test-threads=1"])
            .env("LD_PRELOAD", shim())
            .env("USBSHIM_USBFS", self.dir.join("usb"))
            .env("DROIDC_USB_SOCKET", name)
            .env("USBSHIM_OPEN", open);
        if at {
            cmd.env("USBSHIM_AT", "1");
        }
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        stdout
            .lines()
            .find(|l| l.starts_with("ok ") || l.starts_with("err "))
            .unwrap_or_else(|| {
                panic!(
                    "no answer: {stdout} {}",
                    String::from_utf8_lossy(&out.stderr)
                )
            })
            .to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn placeholder_becomes_the_hosts_fd() {
    let f = Fixture::new("grant");
    let name = format!("usbshim-preload-grant-{}", std::process::id());
    let host = f.host(&name, Host::Grant);
    assert_eq!(
        f.run(&name, &f.dir.join("usb/001/005"), false),
        "ok descriptors cloexec=0"
    );
    let req = host.join().unwrap();
    assert!(req.contains("\"bus\":1,\"dev\":5"), "{req}");
}

#[test]
fn openat_on_the_bus_directory() {
    let f = Fixture::new("at");
    let name = format!("usbshim-preload-at-{}", std::process::id());
    let host = f.host(&name, Host::Grant);
    assert_eq!(
        f.run(&name, &f.dir.join("usb/001/005"), true),
        "ok descriptors cloexec=0"
    );
    host.join().unwrap();
}

#[test]
fn refusal_is_eacces() {
    let f = Fixture::new("refuse");
    let name = format!("usbshim-preload-refuse-{}", std::process::id());
    let host = f.host(&name, Host::Refuse);
    assert_eq!(
        f.run(&name, &f.dir.join("usb/001/005"), false),
        format!("err {}", libc::EACCES)
    );
    host.join().unwrap();
}

#[test]
fn no_host_is_enodev() {
    let f = Fixture::new("nohost");
    let name = format!("usbshim-preload-nohost-{}", std::process::id());
    assert_eq!(
        f.run(&name, &f.dir.join("usb/001/005"), false),
        format!("err {}", libc::ENODEV)
    );
}

#[test]
fn other_files_are_untouched() {
    let f = Fixture::new("other");
    let name = format!("usbshim-preload-other-{}", std::process::id());
    // No host listens: an intercepted open would fail with ENODEV.
    assert_eq!(
        f.run(&name, &f.dir.join("device"), false),
        "ok descriptors cloexec=0"
    );
}

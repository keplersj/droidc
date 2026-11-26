//! `sudo`, `su` and `pkexec` in the guest, by asking the host to start a second launcher as root
//! (Selkie #9, droidc #10/#11).
//!
//! A shim inside the guest cannot start proot itself: the guest is already traced by proot, and proot
//! is not in the image. So the host side, `droidc spawn-server`, listens on an abstract socket and
//! starts the launcher the host gave it (proot with `-0`, i.e. fake root in the same container). The
//! guest shim, `droidc spawn`, passes its own stdin, stdout and stderr over the socket, so the root
//! command shares the terminal, and exits with the child's status.
//!
//! Only the host decides what the launcher is: a request carries a command, a working directory and an
//! environment, never a rootfs, binds or launcher flags. The server checks the peer's uid, because any
//! app on the device can connect to an abstract socket.
//!
//! What this is not: real root. The second proot fakes uid 0 the same way DroidDeck does, so
//! file ownership and capabilities are proot's illusion, and nothing escapes the app's own uid.

use crate::error::{Context, Error, Result};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketType,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};

/// The guest environment variable naming the server's abstract socket (without the leading NUL).
pub const SOCKET_ENV: &str = "DROIDC_SPAWNER";
/// Where the shims are bound in the guest (`shell-spec`), all of them the guest droidc binary.
pub const SHIMS: [&str; 3] = ["/usr/bin/sudo", "/usr/bin/su", "/usr/bin/pkexec"];
/// Variables a request never carries, as `sudo` also refuses to pass them on.
const UNSAFE_ENV: [&str; 6] = [
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "BASH_ENV",
    "ENV",
    "PYTHONPATH",
];

/// What the guest shim asks for. The launcher, the container and the rootfs are the server's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum Message {
    /// Forward a signal to the running child.
    Signal {
        signal: i32,
    },
    /// The child's exit code (128 + signal when killed), or why it could not run.
    Done {
        status: i32,
    },
    Failed {
        error: String,
    },
}

fn abstract_addr(name: &str) -> Result<SocketAddrUnix> {
    SocketAddrUnix::new_abstract_name(name.as_bytes())
        .map_err(|e| Error::invalid(format!("socket name {name:?}: {e}")))
}

fn send_fds(sock: &UnixStream, line: &str, fds: &[BorrowedFd]) -> Result<()> {
    let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(3))];
    let mut buf = SendAncillaryBuffer::new(&mut space);
    buf.push(SendAncillaryMessage::ScmRights(fds));
    let bytes = format!("{line}\n");
    let mut sent = 0;
    while sent < bytes.len() {
        // The fds ride the first byte; later writes carry the rest of the line.
        let n = rustix::net::sendmsg(
            sock.as_fd(),
            &[std::io::IoSlice::new(&bytes.as_bytes()[sent..])],
            &mut buf,
            SendFlags::empty(),
        )
        .map_err(|e| Error::internal(format!("sendmsg: {e}")))?;
        sent += n;
        buf.clear();
    }
    Ok(())
}

/// Reads one line and the fds that came with it.
fn recv_with_fds(sock: &UnixStream) -> Result<(String, Vec<OwnedFd>)> {
    let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(3))];
    let mut line = Vec::new();
    let mut fds = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let mut anc = RecvAncillaryBuffer::new(&mut space);
        let got = rustix::net::recvmsg(
            sock.as_fd(),
            &mut [std::io::IoSliceMut::new(&mut byte)],
            &mut anc,
            RecvFlags::empty(),
        )
        .map_err(|e| Error::internal(format!("recvmsg: {e}")))?;
        for m in anc.drain() {
            if let RecvAncillaryMessage::ScmRights(r) = m {
                fds.extend(r);
            }
        }
        if got.bytes == 0 {
            break; // the peer went
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > 1 << 20 {
            return Err(Error::invalid("request too long"));
        }
    }
    Ok((String::from_utf8_lossy(&line).into_owned(), fds))
}

fn reply(sock: &UnixStream, msg: &Message) {
    let mut w = sock;
    let line = serde_json::to_string(msg).unwrap_or_default();
    let _ = writeln!(w, "{line}");
}

/// The command to run for a request: the host's launcher, then the working directory, then the
/// environment, then the request's command. `/usr/bin/env -i` makes the request's environment the
/// child's exactly, while the launcher itself keeps the server's (`PROOT_LOADER` and the like).
fn child_argv(launcher: &[String], req: &Request) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(launcher.len() + req.env.len() + 6);
    // A `-w` in the launcher is the deployment's default; the request's working directory replaces it.
    let mut skip = false;
    for a in launcher {
        if skip {
            skip = false;
            continue;
        }
        if a == "-w" || a == "--pwd" || a == "--cwd" {
            skip = true;
            continue;
        }
        argv.push(a.clone());
    }
    argv.push("-w".into());
    argv.push(req.cwd.clone());
    // `env -i` makes the request's environment exact (the launcher keeps its own, e.g. PROOT_LOADER).
    // No `--`: env takes the first argument without `=` as the command, and a `--` after the
    // assignments would itself be taken as the command.
    argv.push("/usr/bin/env".into());
    argv.push("-i".into());
    for (k, v) in &req.env {
        argv.push(format!("{k}={v}"));
    }
    argv.extend(req.argv.iter().cloned());
    argv
}

/// Reads the connecting process's uid, or `None` where the platform will not say.
fn peer_uid(sock: &UnixStream) -> Option<u32> {
    rustix::net::sockopt::socket_peercred(sock.as_fd())
        .ok()
        .map(|c| c.uid.as_raw())
}

fn run_request(
    launcher: &[String],
    sock: &UnixStream,
    req: &Request,
    fds: &[OwnedFd],
) -> Result<i32> {
    if req.argv.is_empty() {
        return Err(Error::invalid("no command"));
    }
    if fds.len() != 3 {
        return Err(Error::invalid(format!(
            "expected stdin, stdout and stderr, got {} descriptor(s)",
            fds.len()
        )));
    }
    let argv = child_argv(launcher, req);
    let (stdin, stdout, stderr) = (fds[0].as_raw_fd(), fds[1].as_raw_fd(), fds[2].as_raw_fd());
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // SAFETY: the child's own descriptors; `Stdio::from` would take ownership of ours.
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(libc::dup(stdin)))
            .stdout(Stdio::from_raw_fd(libc::dup(stdout)))
            .stderr(Stdio::from_raw_fd(libc::dup(stderr)));
        // Its own process group, so a forwarded signal reaches the whole root command and not us.
        cmd.pre_exec(|| {
            let _ = rustix::process::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().context(&argv[0])?;
    let pid = child.id();
    // While it runs, the shim may send signals (it has the terminal, we do not).
    let signals = sock.try_clone().context("socket")?;
    std::thread::spawn(move || {
        for line in BufReader::new(signals)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if let Ok(Message::Signal { signal }) = serde_json::from_str::<Message>(&line) {
                if (1..=31).contains(&signal) {
                    // The child is a session leader: signal its group.
                    unsafe { libc::kill(-(pid as i32), signal) };
                }
            }
        }
    });
    let st = child.wait().context("wait")?;
    Ok(st
        .code()
        .or_else(|| st.signal().map(|s| 128 + s))
        .unwrap_or(1))
}

/// `droidc spawn-server --socket NAME [--uid N] -- launcher...`: the host side. Serves one request per
/// connection, for as long as it runs.
pub fn serve(name: &str, allow_uid: Option<u32>, launcher: &[String]) -> Result<i32> {
    if launcher.is_empty() {
        return Err(Error::invalid(
            "spawn-server needs the launcher command after --",
        ));
    }
    let sock = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
        .map_err(|e| Error::internal(format!("socket: {e}")))?;
    rustix::net::bind(&sock, &abstract_addr(name)?)
        .map_err(|e| Error::internal(format!("bind @{name}: {e}")))?;
    rustix::net::listen(&sock, 16).map_err(|e| Error::internal(format!("listen: {e}")))?;
    let listener = UnixListener::from(sock);
    let allow = allow_uid.unwrap_or_else(|| rustix::process::getuid().as_raw());
    eprintln!("droidc spawn-server: listening on @{name} for uid {allow}");
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warn: accept: {e}");
                continue;
            }
        };
        let launcher = launcher.to_vec();
        std::thread::spawn(move || {
            if let Some(uid) = peer_uid(&conn) {
                if uid != allow {
                    eprintln!("warn: refused a request from uid {uid}");
                    reply(
                        &conn,
                        &Message::Failed {
                            error: "not allowed".into(),
                        },
                    );
                    return;
                }
            }
            let (line, fds) = match recv_with_fds(&conn) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("warn: {e}");
                    return;
                }
            };
            let parsed = serde_json::from_str::<Request>(&line)
                .map_err(|e| Error::invalid(format!("bad request: {e}")));
            let done = parsed.and_then(|req| run_request(&launcher, &conn, &req, &fds));
            match done {
                Ok(status) => reply(&conn, &Message::Done { status }),
                Err(e) => {
                    eprintln!("warn: {e}");
                    reply(
                        &conn,
                        &Message::Failed {
                            error: e.to_string(),
                        },
                    );
                }
            }
        });
    }
    Ok(0)
}

/// The environment a root command gets: the caller's, without what `sudo` drops, and with root's own
/// `HOME`, `USER` and `LOGNAME`.
fn root_env() -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| !UNSAFE_ENV.contains(&k.as_str()) && k != SOCKET_ENV)
        .collect();
    if let Some(user) = env.get("USER").cloned() {
        env.insert("SUDO_USER".into(), user);
    }
    env.insert("HOME".into(), "/root".into());
    env.insert("USER".into(), "root".into());
    env.insert("LOGNAME".into(), "root".into());
    env.insert("PATH".into(), crate::status::DEFAULT_PATH.into());
    env
}

/// What the shim's own argv means. `sudo`'s options are accepted where they make sense here and
/// refused where they would quietly do something else.
fn parse_shim(name: &str, args: &[String]) -> Result<Vec<String>> {
    let mut rest = args.iter();
    let mut cmd: Vec<String> = Vec::new();
    let mut login = false;
    while let Some(a) = rest.next() {
        match a.as_str() {
            "-" | "-l" | "--login" if name != "pkexec" => login = true,
            "-i" if name == "sudo" => login = true,
            "-E" | "--preserve-env" | "-H" | "--set-home" | "-k" | "-n" | "--non-interactive"
            | "-p" | "--" => {
                if a == "-p" {
                    rest.next();
                }
                if a == "--" {
                    cmd.extend(rest.cloned());
                    break;
                }
            }
            "-c" if name == "su" => {
                cmd = vec!["/bin/sh".into(), "-c".into()];
                cmd.extend(rest.cloned());
                break;
            }
            "-u" | "--user" => {
                let who = rest.next().map(String::as_str).unwrap_or("");
                if who != "root" && who != "0" {
                    return Err(Error::invalid(format!(
                        "{name}: only root is available here, not {who:?}"
                    )));
                }
            }
            "root" | "0" if name == "su" && cmd.is_empty() => {}
            "-v" | "--validate" => return Ok(Vec::new()),
            a if a.starts_with('-') => {
                return Err(Error::invalid(format!("{name}: {a} is not supported here")))
            }
            _ => {
                cmd.push(a.clone());
                cmd.extend(rest.cloned());
                break;
            }
        }
    }
    if cmd.is_empty() {
        // Interactive root shell, as `sudo -i` and `su` give.
        return Ok(vec!["/bin/bash".into(), "-l".into()]);
    }
    if login && !cmd.is_empty() {
        let joined = cmd.join(" ");
        return Ok(vec!["/bin/bash".into(), "-lc".into(), joined]);
    }
    Ok(cmd)
}

/// `droidc spawn -- cmd...`, and the `sudo`/`su`/`pkexec` shims (same binary, named by `argv[0]`):
/// asks the host to run `cmd` as root in this container and exits with its status.
pub fn client(name: &str, args: &[String]) -> Result<i32> {
    let argv = parse_shim(name, args)?;
    if argv.is_empty() {
        return Ok(0); // `sudo -v`
    }
    let Some(socket) = std::env::var_os(SOCKET_ENV).filter(|v| !v.is_empty()) else {
        return Err(Error::not_found(format!(
            "{name} needs droidc's spawner, which this session does not have ({SOCKET_ENV} is unset)"
        )));
    };
    let req = Request {
        argv,
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/".into()),
        env: root_env(),
    };
    let name_str = socket.to_string_lossy().into_owned();
    let sock = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
        .map_err(|e| Error::internal(format!("socket: {e}")))?;
    rustix::net::connect(&sock, &abstract_addr(&name_str)?)
        .map_err(|e| Error::internal(format!("connect @{name_str}: {e}")))?;
    let stream = UnixStream::from(sock);
    let line = serde_json::to_string(&req).map_err(|e| Error::internal(e.to_string()))?;
    // SAFETY: borrowing our own standard descriptors for the duration of the send.
    let (i, o, e) = unsafe {
        (
            BorrowedFd::borrow_raw(0),
            BorrowedFd::borrow_raw(1),
            BorrowedFd::borrow_raw(2),
        )
    };
    send_fds(&stream, &line, &[i, o, e])?;
    forward_signals(&stream);
    for line in BufReader::new(&stream)
        .lines()
        .map_while(std::result::Result::ok)
    {
        match serde_json::from_str::<Message>(&line) {
            Ok(Message::Done { status }) => return Ok(status),
            Ok(Message::Failed { error }) => {
                return Err(Error::internal(format!("{name}: {error}")))
            }
            _ => {}
        }
    }
    Err(Error::internal(format!(
        "{name}: the spawner closed the connection"
    )))
}

/// Terminal signals reach the shim, not the root command (the host started it). Forward them.
fn forward_signals(stream: &UnixStream) {
    use std::sync::atomic::{AtomicI32, Ordering};
    static CAUGHT: AtomicI32 = AtomicI32::new(0);
    extern "C" fn catch(sig: libc::c_int) {
        CAUGHT.store(sig, Ordering::SeqCst);
    }
    let handler: extern "C" fn(libc::c_int) = catch;
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: the handler only stores an integer.
        unsafe { libc::signal(sig, handler as usize as libc::sighandler_t) };
    }
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let sig = CAUGHT.swap(0, Ordering::SeqCst);
        if sig != 0 {
            let line = serde_json::to_string(&Message::Signal { signal: sig }).unwrap_or_default();
            if writeln!(out, "{line}").is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn req(argv: &[&str]) -> Request {
        Request {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: "/tmp".into(),
            env: BTreeMap::from([("A".to_string(), "b".to_string())]),
        }
    }

    #[test]
    fn the_launchers_working_directory_is_replaced_and_the_environment_is_exact() {
        let launcher: Vec<String> = ["proot", "-0", "-r", "/rootfs", "-w", "/root"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            child_argv(&launcher, &req(&["id", "-u"])),
            [
                "proot",
                "-0",
                "-r",
                "/rootfs",
                "-w",
                "/tmp",
                "/usr/bin/env",
                "-i",
                "A=b",
                "id",
                "-u"
            ]
        );
    }

    #[test]
    fn shim_arguments_become_a_command() {
        let p = |name: &str, args: &[&str]| {
            parse_shim(
                name,
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            p("sudo", &["dnf", "install", "-y", "git"]).unwrap(),
            ["dnf", "install", "-y", "git"]
        );
        assert_eq!(p("sudo", &["--", "-weird"]).unwrap(), ["-weird"]);
        assert_eq!(p("sudo", &["-u", "root", "id"]).unwrap(), ["id"]);
        assert_eq!(p("sudo", &[]).unwrap(), ["/bin/bash", "-l"]);
        assert_eq!(p("su", &["-"]).unwrap(), ["/bin/bash", "-l"]);
        assert_eq!(
            p("su", &["root", "-c", "id"]).unwrap(),
            ["/bin/sh", "-c", "id"]
        );
        assert_eq!(
            p("sudo", &["-i", "id"]).unwrap(),
            ["/bin/bash", "-lc", "id"]
        );
        assert_eq!(p("sudo", &["-v"]).unwrap(), Vec::<String>::new());
        assert_eq!(p("sudo", &["-E", "id"]).unwrap(), ["id"]);
        assert!(p("sudo", &["-u", "alice", "id"]).is_err());
        assert!(p("sudo", &["--askpass", "id"]).is_err());
    }

    #[test]
    fn the_root_environment_drops_what_sudo_drops() {
        std::env::set_var("LD_PRELOAD", "/x.so");
        std::env::set_var("USER", "selkie");
        std::env::set_var(SOCKET_ENV, "droidc-test");
        let env = root_env();
        assert!(!env.contains_key("LD_PRELOAD"));
        assert!(!env.contains_key(SOCKET_ENV));
        assert_eq!(env["USER"], "root");
        assert_eq!(env["SUDO_USER"], "selkie");
        assert_eq!(env["HOME"], "/root");
        std::env::remove_var("LD_PRELOAD");
        std::env::remove_var(SOCKET_ENV);
    }

    /// A server with `/bin/sh -c 'exec "$@"' --` for a launcher: it runs the command where proot would
    /// enter the container, so the whole round trip (fds, environment, status) can be tested here.
    #[test]
    fn a_request_runs_on_the_shims_own_descriptors() {
        let name = format!("droidc-spawn-test-{}", std::process::id());
        // Stands in for proot: consumes the `-w DIR` child_argv inserts, then runs the rest.
        let launcher: Vec<String> = [
            "/bin/sh",
            "-c",
            "d=; while [ \"$1\" = -w ]; do d=$2; shift 2; done; [ -n \"$d\" ] && cd \"$d\"; exec \"$@\"",
            "--",
        ]
        .map(String::from)
        .to_vec();
        let serving = {
            let (name, launcher) = (name.clone(), launcher.clone());
            std::thread::spawn(move || serve(&name, None, &launcher))
        };
        // The socket is bound by the time a connection succeeds.
        let connect = || -> Option<UnixStream> {
            let sock = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
            rustix::net::connect(&sock, &abstract_addr(&name).unwrap())
                .ok()
                .map(|()| UnixStream::from(sock))
        };
        let mut stream = None;
        for _ in 0..100 {
            if let Some(s) = connect() {
                stream = Some(s);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let stream = stream.expect("the server bound its socket");

        let out = tempfile::NamedTempFile::new().unwrap();
        let r = Request {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo \"$A $PWD $(id -u 2>/dev/null)\"; exit 3".into(),
            ],
            cwd: "/tmp".into(),
            env: BTreeMap::from([("A".to_string(), "hello".to_string())]),
        };
        let line = serde_json::to_string(&r).unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        send_fds(
            &stream,
            &line,
            &[null.as_fd(), out.as_file().as_fd(), out.as_file().as_fd()],
        )
        .unwrap();
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).unwrap()["status"],
            3
        );
        let mut written = String::new();
        std::fs::File::open(out.path())
            .unwrap()
            .read_to_string(&mut written)
            .unwrap();
        assert!(
            written.starts_with("hello /tmp"),
            "the request's environment and directory, got {written:?}"
        );
        drop(stream);
        assert!(!serving.is_finished(), "the server serves more requests");
    }
}

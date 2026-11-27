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
//!
//! Asking first. With `DROIDC_SPAWNER_ASK=1` in the server's environment, every root command waits
//! for the host's answer before it starts (a `bwrap` sandbox runs as the caller, so it never asks).
//! The question goes to the server's stdout and the answer comes back on its stdin, one JSON object
//! per line, so only the process that started the server can answer: nothing in the guest can reach
//! those pipes. The host gets
//!
//! ```text
//! {"ask":{"id":1,"via":"sudo","argv":["dnf","install","-y","git"],"cwd":"/root","from":"bash"}}
//! {"settled":{"id":1,"allowed":true,"by":"host"}}
//! ```
//!
//! `via` is the shim that asked, `from` the name of the process that ran it (the guest's own word:
//! a hint, not something the host can verify), and `settled` says how a question ended: answered by
//! the host, given up by the caller (Ctrl-C, or the shim went away), or refused because nobody is
//! listening any more (`by` is `host`, `caller` or `nobody`). The host answers with
//!
//! ```text
//! {"answer":{"id":1,"allow":true,"remember":false}}
//! {"forget":true}
//! ```
//!
//! `remember` allows every later root command of this server without asking, until `forget` or the
//! server ends (Selkie runs one per container, for as long as the container runs). While it waits,
//! the shim says so on its stderr; a refusal ends it with status 1, as a refused `sudo` does. Asked
//! commands also get a fixed `PATH` and none of the variables `sudo` drops, whatever the shim sent,
//! so what the host approved is what runs.

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
/// Set (to anything but `0`) in the server's environment: ask the host before each root command.
pub const ASK_ENV: &str = "DROIDC_SPAWNER_ASK";
/// Where the shims are bound in the guest (`shell-spec`), all of them the guest droidc binary.
/// `bwrap` is `crate::bwrap`, the rest are root commands.
pub const SHIMS: [&str; 4] = [
    "/usr/bin/sudo",
    "/usr/bin/su",
    "/usr/bin/pkexec",
    crate::bwrap::SHIM,
];
/// Descriptors one request may carry (stdin, stdout, stderr and a sandbox's).
const MAX_FDS: usize = 3 + crate::bwrap::MAX_FDS;
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
    /// A `bwrap` request: run in a sandbox of its own, as the caller, instead of in the container as
    /// root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<crate::bwrap::Sandbox>,
    /// The shim that asked (`sudo`, `su`, `pkexec`, `spawn`), shown to the host when it is asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
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
    /// A sandbox started (its host pid, for `bwrap --info-fd`).
    Started {
        pid: u32,
    },
    /// The command waits for the host's answer (what to tell the user meanwhile).
    Waiting {
        waiting: String,
    },
    /// The host said no (why, for the user).
    Refused {
        refused: String,
    },
}

/// Signals that mean the caller gave up on a root command it is still waiting for.
const GIVE_UP: [i32; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// A line the server writes for the host when it asks (see the module docs).
#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Ask<'a> {
    Ask {
        id: u64,
        via: &'a str,
        argv: &'a [String],
        cwd: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<&'a str>,
    },
    Settled {
        id: u64,
        allowed: bool,
        by: &'static str,
    },
}

/// A line the host writes back.
#[derive(Debug, Default, Deserialize)]
struct HostLine {
    #[serde(default)]
    answer: Option<Answer>,
    #[serde(default)]
    forget: bool,
}

#[derive(Debug, Deserialize)]
struct Answer {
    id: u64,
    allow: bool,
    #[serde(default)]
    remember: bool,
}

enum Verdict {
    Allow {
        remember: bool,
    },
    Deny,
    /// Nobody will answer: the host closed the server's stdin.
    Nobody,
}

/// Asks the host before root commands: questions out on one stream, answers in on another.
pub struct Asker {
    out: std::sync::Mutex<Box<dyn Write + Send>>,
    waiting: std::sync::Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<Verdict>>>,
    next: std::sync::atomic::AtomicU64,
    remembered: std::sync::atomic::AtomicBool,
    gone: std::sync::atomic::AtomicBool,
}

impl Asker {
    /// Questions go to `out`; answers are read from `input` on a thread of their own.
    pub fn new(
        out: Box<dyn Write + Send>,
        input: impl BufRead + Send + 'static,
    ) -> std::sync::Arc<Self> {
        let asker = std::sync::Arc::new(Asker {
            out: std::sync::Mutex::new(out),
            waiting: Default::default(),
            next: Default::default(),
            remembered: Default::default(),
            gone: Default::default(),
        });
        let reader = asker.clone();
        std::thread::spawn(move || reader.read_answers(input));
        asker
    }

    /// The server's own stdout and stdin, when `DROIDC_SPAWNER_ASK` asks for it.
    pub fn from_env() -> Option<std::sync::Arc<Self>> {
        std::env::var_os(ASK_ENV)
            .filter(|v| !v.is_empty() && v != "0")
            .map(|_| {
                Asker::new(
                    Box::new(std::io::stdout()),
                    BufReader::new(std::io::stdin()),
                )
            })
    }

    fn read_answers(&self, input: impl BufRead) {
        use std::sync::atomic::Ordering;
        for line in input.lines().map_while(std::result::Result::ok) {
            let Ok(host) = serde_json::from_str::<HostLine>(&line) else {
                eprintln!("warn: spawn-server: not an answer: {line:?}");
                continue;
            };
            if host.forget {
                self.remembered.store(false, Ordering::SeqCst);
            }
            if let Some(a) = host.answer {
                let tx = self.waiting.lock().unwrap().remove(&a.id);
                if let Some(tx) = tx {
                    let _ = tx.send(if a.allow {
                        Verdict::Allow {
                            remember: a.remember,
                        }
                    } else {
                        Verdict::Deny
                    });
                }
            }
        }
        // The host stopped listening: nothing waiting now or later can be allowed.
        self.gone.store(true, Ordering::SeqCst);
        for (_, tx) in self.waiting.lock().unwrap().drain() {
            let _ = tx.send(Verdict::Nobody);
        }
    }

    fn emit(&self, line: &Ask) -> bool {
        let Ok(text) = serde_json::to_string(line) else {
            return false;
        };
        let mut out = self.out.lock().unwrap();
        writeln!(out, "{text}").and_then(|()| out.flush()).is_ok()
    }

    /// Waits for the host's answer to `req`: `Ok` to run it, `Err` with what to tell the caller.
    fn ask(
        &self,
        conn: &UnixStream,
        req: &Request,
        from: Option<&str>,
    ) -> std::result::Result<(), String> {
        use std::sync::atomic::Ordering;
        const NOBODY: &str = "nobody is there to allow it (Selkie is not listening)";
        if self.remembered.load(Ordering::SeqCst) {
            return Ok(());
        }
        if self.gone.load(Ordering::SeqCst) {
            return Err(NOBODY.into());
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = std::sync::mpsc::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        let asked = self.emit(&Ask::Ask {
            id,
            via: req.via.as_deref().unwrap_or("spawn"),
            argv: &req.argv,
            cwd: &req.cwd,
            from,
        });
        if !asked {
            self.waiting.lock().unwrap().remove(&id);
            return Err(NOBODY.into());
        }
        reply(
            conn,
            &Message::Waiting {
                waiting: "waiting for you to allow this in Selkie".into(),
            },
        );
        let mut partial = Vec::new();
        loop {
            let verdict = match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(v) => v,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if !caller_gave_up(conn, &mut partial) {
                        continue;
                    }
                    self.waiting.lock().unwrap().remove(&id);
                    self.emit(&Ask::Settled {
                        id,
                        allowed: false,
                        by: "caller",
                    });
                    return Err("cancelled".into());
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Verdict::Nobody,
            };
            let (allowed, by) = match verdict {
                Verdict::Allow { remember } => {
                    if remember {
                        self.remembered.store(true, Ordering::SeqCst);
                    }
                    (true, "host")
                }
                Verdict::Deny => (false, "host"),
                Verdict::Nobody => (false, "nobody"),
            };
            self.emit(&Ask::Settled { id, allowed, by });
            return match (allowed, by) {
                (true, _) => Ok(()),
                (false, "nobody") => Err(NOBODY.into()),
                _ => Err("you did not allow this in Selkie".into()),
            };
        }
    }
}

/// Whether the caller hung up or sent a signal that gives up, while its command waits to be allowed.
/// Reads only what is already there; a partial line is kept in `partial` for the next call.
fn caller_gave_up(conn: &UnixStream, partial: &mut Vec<u8>) -> bool {
    loop {
        let mut b = 0u8;
        // SAFETY: one byte into a local, without blocking.
        let n = unsafe {
            libc::recv(
                conn.as_raw_fd(),
                (&mut b as *mut u8).cast(),
                1,
                libc::MSG_DONTWAIT,
            )
        };
        if n == 0 {
            return true; // the shim went
        }
        if n < 0 {
            let e = std::io::Error::last_os_error();
            match e.kind() {
                std::io::ErrorKind::WouldBlock => return false,
                std::io::ErrorKind::Interrupted => continue,
                _ => return true,
            }
        }
        if b != b'\n' {
            partial.push(b);
            if partial.len() > 4096 {
                partial.clear();
            }
            continue;
        }
        let line = std::mem::take(partial);
        if let Ok(Message::Signal { signal }) = serde_json::from_slice::<Message>(&line) {
            if GIVE_UP.contains(&signal) {
                return true;
            }
        }
    }
}

/// The name of the process that ran the shim (its parent's `comm`), for the host's question. The
/// guest controls it, so it is only ever shown as the guest's own word.
fn caller_name(sock: &UnixStream) -> Option<String> {
    let pid = rustix::net::sockopt::socket_peercred(sock.as_fd())
        .ok()?
        .pid
        .as_raw_nonzero()
        .get();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid ...`: comm may hold spaces and parentheses, so read after the last ')'.
    let ppid: u32 = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let comm = std::fs::read_to_string(format!("/proc/{ppid}/comm")).ok()?;
    let comm: String = comm
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    (!comm.is_empty()).then_some(comm)
}

/// What an asked root command runs with: no variable `sudo` would drop and the fixed `PATH`, whatever
/// the shim sent, so the command the host saw is the one that runs.
fn harden(env: &mut BTreeMap<String, String>) {
    env.retain(|k, _| !UNSAFE_ENV.contains(&k.as_str()) && k != SOCKET_ENV);
    env.insert("PATH".into(), crate::status::DEFAULT_PATH.into());
}

fn abstract_addr(name: &str) -> Result<SocketAddrUnix> {
    SocketAddrUnix::new_abstract_name(name.as_bytes())
        .map_err(|e| Error::invalid(format!("socket name {name:?}: {e}")))
}

fn send_fds(sock: &UnixStream, line: &str, fds: &[BorrowedFd]) -> Result<()> {
    let mut space =
        [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
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
    let mut space =
        [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut line = Vec::new();
    let mut fds = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let mut anc = RecvAncillaryBuffer::new(&mut space);
        let got = rustix::net::recvmsg(
            sock.as_fd(),
            &mut [std::io::IoSliceMut::new(&mut byte)],
            &mut anc,
            // Not inherited by other requests' children; a sandbox's are placed on purpose.
            RecvFlags::CMSG_CLOEXEC,
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
        if line.len() > 16 << 20 {
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

/// Removes a sandbox's staging directory when the request ends.
struct Staging(std::path::PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn staging_dir() -> Result<Staging> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let base = std::env::var_os("DROIDC_SANDBOX_DIR")
        .or_else(|| std::env::var_os("PROOT_TMP_DIR"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!(
        "droidc-sandbox-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).context(dir.display().to_string())?;
    Ok(Staging(dir))
}

/// The command for a `bwrap` request (a sibling proot over the sandbox's own root and binds), the
/// environment to start it with, and its staging directory.
fn sandbox_argv(
    launcher: &[String],
    req: &Request,
    sb: &crate::bwrap::Sandbox,
) -> Result<(Vec<String>, BTreeMap<String, String>, Staging)> {
    let l = crate::bwrap::Launcher::parse(launcher)?;
    let staging = staging_dir()?;
    // A bind may only resolve to what the guest itself sees.
    let mut allowed = vec![l.rootfs.clone()];
    allowed.extend(l.binds.iter().map(|(h, _)| h.clone()));
    let plan = crate::bwrap::plan(&l, &sb.ops, &staging.0, &allowed)?;
    let mut argv = plan.argv(&l, &req.cwd, sb.root);
    argv.extend(req.argv.iter().cloned());
    // proot passes its environment to the command: the request's, plus proot's own settings (which
    // the server has, as every proot it starts does). No `env -i` as for root commands: the sandbox
    // need not have `/usr/bin/env`.
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("PROOT_"))
        .collect();
    env.extend(req.env.clone());
    Ok((argv, env, staging))
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
    let extra = req.sandbox.as_ref().map_or(0, |sb| sb.fds.len());
    if fds.len() != 3 + extra {
        return Err(Error::invalid(format!(
            "expected stdin, stdout, stderr and {extra} more, got {} descriptor(s)",
            fds.len()
        )));
    }
    let (argv, env, _staging) = match &req.sandbox {
        None => (child_argv(launcher, req), None, None),
        Some(sb) => {
            if sb.fds.iter().any(|fd| *fd < 3) {
                return Err(Error::invalid("a sandbox descriptor below 3"));
            }
            let (argv, env, staging) = sandbox_argv(launcher, req, sb)?;
            // The spawner's own log: what a sandbox was started as, for when it fails.
            eprintln!("droidc spawn-server: bwrap: {}", argv.join(" "));
            (argv, Some(env), Some(staging))
        }
    };
    let (stdin, stdout, stderr) = (fds[0].as_raw_fd(), fds[1].as_raw_fd(), fds[2].as_raw_fd());
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(env) = &env {
        cmd.env_clear().envs(env);
    }
    // A sandbox's descriptors, at the numbers the shim had them.
    let placed: Vec<(i32, i32)> = req.sandbox.as_ref().map_or_else(Vec::new, |sb| {
        fds[3..]
            .iter()
            .map(|f| f.as_raw_fd())
            .zip(sb.fds.iter().copied())
            .collect()
    });
    let mut high = vec![-1; placed.len()];
    // SAFETY: the child's own descriptors; `Stdio::from` would take ownership of ours.
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(libc::dup(stdin)))
            .stdout(Stdio::from_raw_fd(libc::dup(stdout)))
            .stderr(Stdio::from_raw_fd(libc::dup(stderr)));
        // Its own process group, so a forwarded signal reaches the whole root command and not us.
        cmd.pre_exec(move || {
            let _ = rustix::process::setsid();
            // Out of the way first (a target may be another's source), then into place; dup2 leaves
            // the target open across exec. Only async-signal-safe calls, no allocation.
            for (i, (src, _)) in placed.iter().enumerate() {
                high[i] = libc::fcntl(*src, libc::F_DUPFD_CLOEXEC, 1000);
                if high[i] < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            for (i, (_, to)) in placed.iter().enumerate() {
                if libc::dup2(high[i], *to) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context(&argv[0])?;
    let pid = child.id();
    let sandboxed = req.sandbox.is_some();
    if sandboxed {
        reply(sock, &Message::Started { pid });
    }
    let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // While it runs, the shim may send signals (it has the terminal, we do not).
    let signals = sock.try_clone().context("socket")?;
    let gone = exited.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(signals)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if let Ok(Message::Signal { signal }) = serde_json::from_str::<Message>(&line) {
                if (1..=31).contains(&signal) && !gone.load(std::sync::atomic::Ordering::SeqCst) {
                    // The child is a session leader: signal its group.
                    unsafe { libc::kill(-(pid as i32), signal) };
                }
            }
        }
        // The shim went. A sandbox goes with its bwrap, as it would with --die-with-parent.
        if sandboxed && !gone.load(std::sync::atomic::Ordering::SeqCst) {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    });
    let st = child.wait().context("wait")?;
    exited.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(st
        .code()
        .or_else(|| st.signal().map(|s| 128 + s))
        .unwrap_or(1))
}

/// `droidc spawn-server --socket NAME [--uid N] -- launcher...`: the host side. Serves one request per
/// connection, for as long as it runs. With `DROIDC_SPAWNER_ASK` set, it asks the host first (see
/// the module docs).
pub fn serve(name: &str, allow_uid: Option<u32>, launcher: &[String]) -> Result<i32> {
    serve_asking(name, allow_uid, launcher, Asker::from_env())
}

/// [`serve`], asking `asker` before each root command when there is one.
pub fn serve_asking(
    name: &str,
    allow_uid: Option<u32>,
    launcher: &[String],
    asker: Option<std::sync::Arc<Asker>>,
) -> Result<i32> {
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
    eprintln!(
        "droidc spawn-server: listening on @{name} for uid {allow}{}",
        if asker.is_some() {
            ", asking first"
        } else {
            ""
        }
    );
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warn: accept: {e}");
                continue;
            }
        };
        let launcher = launcher.to_vec();
        let asker = asker.clone();
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
            let mut refused = None;
            let done = parsed.and_then(|mut req| {
                // A sandbox runs as the caller: only root commands are asked about.
                if let (Some(asker), None) = (&asker, &req.sandbox) {
                    harden(&mut req.env);
                    if let Err(why) = asker.ask(&conn, &req, caller_name(&conn).as_deref()) {
                        refused = Some(why);
                        return Ok(0);
                    }
                }
                run_request(&launcher, &conn, &req, &fds)
            });
            if let Some(why) = refused {
                eprintln!("droidc spawn-server: not run: {why}");
                reply(&conn, &Message::Refused { refused: why });
                return;
            }
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
    let req = Request {
        argv,
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/".into()),
        env: root_env(),
        sandbox: None,
        via: Some(name.into()),
    };
    request(name, &req, &[], |_| {})
}

/// Sends `req` with our stdin, stdout, stderr and `extra` descriptors, forwards terminal signals, and
/// returns the command's status. `started` gets a sandbox's pid when the server reports it.
pub fn request(
    name: &str,
    req: &Request,
    extra: &[i32],
    mut started: impl FnMut(u32),
) -> Result<i32> {
    let Some(socket) = std::env::var_os(SOCKET_ENV).filter(|v| !v.is_empty()) else {
        return Err(Error::not_found(format!(
            "{name} needs droidc's spawner, which this session does not have ({SOCKET_ENV} is unset)"
        )));
    };
    let name_str = socket.to_string_lossy().into_owned();
    let sock = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
        .map_err(|e| Error::internal(format!("socket: {e}")))?;
    rustix::net::connect(&sock, &abstract_addr(&name_str)?)
        .map_err(|e| Error::internal(format!("connect @{name_str}: {e}")))?;
    let stream = UnixStream::from(sock);
    let line = serde_json::to_string(req).map_err(|e| Error::internal(e.to_string()))?;
    // SAFETY: borrowing our own descriptors for the duration of the send.
    let fds: Vec<BorrowedFd> = [0, 1, 2]
        .iter()
        .chain(extra)
        .map(|fd| unsafe { BorrowedFd::borrow_raw(*fd) })
        .collect();
    send_fds(&stream, &line, &fds)?;
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
            Ok(Message::Started { pid }) => started(pid),
            Ok(Message::Waiting { waiting }) => eprintln!("{name}: {waiting}"),
            // Refused, as `sudo` refuses: status 1 and why.
            Ok(Message::Refused { refused }) => {
                eprintln!("{name}: {refused}");
                return Ok(1);
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
            sandbox: None,
            via: None,
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
            sandbox: None,
            via: None,
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

    /// With an asker, a root command waits for the host: allowed, refused, given up by the caller,
    /// remembered, forgotten, and refused once nobody listens. Asked commands get the fixed `PATH`.
    #[test]
    fn root_commands_wait_for_the_hosts_answer() {
        let name = format!("droidc-spawn-ask-test-{}", std::process::id());
        let launcher: Vec<String> = [
            "/bin/sh",
            "-c",
            "d=; while [ \"$1\" = -w ]; do d=$2; shift 2; done; [ -n \"$d\" ] && cd \"$d\"; exec \"$@\"",
            "--",
        ]
        .map(String::from)
        .to_vec();
        let (q_w, q_r) = UnixStream::pair().unwrap();
        let (a_w, a_r) = UnixStream::pair().unwrap();
        let asker = Asker::new(Box::new(q_w), BufReader::new(a_r));
        {
            let (name, launcher) = (name.clone(), launcher.clone());
            std::thread::spawn(move || serve_asking(&name, None, &launcher, Some(asker)));
        }
        let mut questions = BufReader::new(q_r);
        let mut answers = a_w;
        let next_question = |q: &mut BufReader<UnixStream>| -> serde_json::Value {
            let mut l = String::new();
            q.read_line(&mut l).unwrap();
            serde_json::from_str(&l).unwrap()
        };
        let connect = || -> UnixStream {
            for _ in 0..100 {
                let sock =
                    rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
                if rustix::net::connect(&sock, &abstract_addr(&name).unwrap()).is_ok() {
                    return UnixStream::from(sock);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("the server never bound its socket");
        };
        // Sends a sudo request that prints its PATH and LD_PRELOAD; returns the stream and its output.
        let send = || -> (UnixStream, tempfile::NamedTempFile) {
            let stream = connect();
            let out = tempfile::NamedTempFile::new().unwrap();
            let r = Request {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo \"ran $PATH [$LD_PRELOAD]\"; exit 5".into(),
                ],
                cwd: "/tmp".into(),
                env: BTreeMap::from([
                    ("PATH".to_string(), "/tmp/evil".to_string()),
                    ("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string()),
                ]),
                sandbox: None,
                via: Some("sudo".into()),
            };
            let null = std::fs::File::open("/dev/null").unwrap();
            send_fds(
                &stream,
                &serde_json::to_string(&r).unwrap(),
                &[null.as_fd(), out.as_file().as_fd(), out.as_file().as_fd()],
            )
            .unwrap();
            (stream, out)
        };
        let reply_of = |s: &UnixStream| -> serde_json::Value {
            let mut l = String::new();
            BufReader::new(s).read_line(&mut l).unwrap();
            serde_json::from_str(&l).unwrap()
        };
        let output = |f: &tempfile::NamedTempFile| std::fs::read_to_string(f.path()).unwrap();
        let mut answer = |line: String| writeln!(answers, "{line}").unwrap();

        // Allowed: the shim hears it waits, the host gets the question, the command runs hardened.
        let (s1, o1) = send();
        assert!(reply_of(&s1)["waiting"].is_string());
        let q = next_question(&mut questions);
        assert_eq!(q["ask"]["id"], 1);
        assert_eq!(q["ask"]["via"], "sudo");
        assert_eq!(q["ask"]["cwd"], "/tmp");
        assert_eq!(q["ask"]["argv"][0], "/bin/sh");
        assert_eq!(output(&o1), "", "nothing runs before the answer");
        answer(r#"{"answer":{"id":1,"allow":true}}"#.into());
        assert_eq!(reply_of(&s1)["status"], 5);
        assert_eq!(
            next_question(&mut questions)["settled"],
            serde_json::json!({"id": 1, "allowed": true, "by": "host"})
        );
        assert_eq!(
            output(&o1).trim(),
            format!("ran {} []", crate::status::DEFAULT_PATH)
        );

        // Refused: nothing runs, the shim is told why.
        let (s2, o2) = send();
        assert!(reply_of(&s2)["waiting"].is_string());
        assert_eq!(next_question(&mut questions)["ask"]["id"], 2);
        answer(r#"{"answer":{"id":2,"allow":false}}"#.into());
        assert!(reply_of(&s2)["refused"].is_string());
        assert_eq!(next_question(&mut questions)["settled"]["by"], "host");
        assert_eq!(output(&o2), "");

        // Ctrl-C while it waits: the question is withdrawn.
        let (s3, _o3) = send();
        assert!(reply_of(&s3)["waiting"].is_string());
        assert_eq!(next_question(&mut questions)["ask"]["id"], 3);
        writeln!(&s3, r#"{{"signal":{}}}"#, libc::SIGINT).unwrap();
        assert_eq!(
            next_question(&mut questions)["settled"],
            serde_json::json!({"id": 3, "allowed": false, "by": "caller"})
        );

        // The shim going away withdraws it too.
        let (s4, _o4) = send();
        assert!(reply_of(&s4)["waiting"].is_string());
        assert_eq!(next_question(&mut questions)["ask"]["id"], 4);
        drop(s4);
        assert_eq!(next_question(&mut questions)["settled"]["by"], "caller");

        // Remembered: the next one runs without a question.
        let (s5, _o5) = send();
        assert!(reply_of(&s5)["waiting"].is_string());
        assert_eq!(next_question(&mut questions)["ask"]["id"], 5);
        answer(r#"{"answer":{"id":5,"allow":true,"remember":true}}"#.into());
        assert_eq!(reply_of(&s5)["status"], 5);
        assert_eq!(next_question(&mut questions)["settled"]["allowed"], true);
        let (s6, o6) = send();
        assert_eq!(reply_of(&s6)["status"], 5);
        assert!(output(&o6).starts_with("ran "));

        // Forgotten: it asks again.
        answer(r#"{"forget":true}"#.into());
        std::thread::sleep(std::time::Duration::from_millis(100));
        let (s7, _o7) = send();
        assert!(reply_of(&s7)["waiting"].is_string());
        assert_eq!(next_question(&mut questions)["ask"]["id"], 6);

        // The host stops listening: what waits is refused, and so is what comes later.
        drop(answers);
        assert!(reply_of(&s7)["refused"].is_string());
        assert_eq!(next_question(&mut questions)["settled"]["by"], "nobody");
        let (s8, o8) = send();
        assert!(reply_of(&s8)["refused"].is_string());
        assert_eq!(output(&o8), "");
    }

    /// A `bwrap` request through a real proot, where one is installed: the sandbox's root, binds, data
    /// files and descriptors, and its staging directory is gone afterwards.
    #[test]
    fn a_sandbox_request_runs_under_proot() {
        use crate::bwrap::{Op, Sandbox};
        let Some(proot) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|d| d.join("proot"))
            .find(|p| p.is_file())
        else {
            eprintln!("skipping: no proot");
            return;
        };
        let staging = tempfile::tempdir().unwrap();
        std::env::set_var("DROIDC_SANDBOX_DIR", staging.path());
        // proot 5.4's seccomp acceleration misses path translation on recent kernels (Ubuntu 26.04
        // runners): the tracee then sees the host's root, not the sandbox's. The server passes
        // PROOT_* on to the proot it starts, as Selkie's own settings are passed.
        std::env::set_var("PROOT_NO_SECCOMP", "1");
        let name = format!("droidc-sandbox-test-{}", std::process::id());
        let launcher: Vec<String> = [
            proot.to_str().unwrap(),
            "-0",
            "-r",
            "/",
            "-b",
            "/proc",
            "-b",
            "/dev",
            "-w",
            "/",
        ]
        .map(String::from)
        .to_vec();
        {
            let (name, launcher) = (name.clone(), launcher.clone());
            std::thread::spawn(move || serve(&name, None, &launcher));
        }
        let mut stream = None;
        for _ in 0..100 {
            let sock = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
            if rustix::net::connect(&sock, &abstract_addr(&name).unwrap()).is_ok() {
                stream = Some(UnixStream::from(sock));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let stream = stream.expect("the server bound its socket");
        let bind = |p: &str| Op::Bind {
            src: p.into(),
            dest: p.into(),
        };
        let mut ops: Vec<Op> = ["/usr", "/etc"].iter().map(|p| bind(p)).collect();
        for l in ["bin", "lib", "lib64", "sbin"] {
            if std::path::Path::new(&format!("/usr/{l}")).exists() {
                ops.push(Op::Symlink {
                    target: format!("usr/{l}"),
                    dest: format!("/{l}"),
                });
            }
        }
        ops.extend([
            bind("/proc"),
            Op::Tmpfs {
                dest: "/tmp".into(),
            },
            Op::Data {
                dest: "/hello".into(),
                data: b"data\n".to_vec(),
            },
        ]);
        let r = Request {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                // Shell builtins only: some hosts' coreutils (uutils, one multicall binary) refuse
                // to run under proot with the host as the rootfs.
                "read -r a </hello; read -r b <&9; set -- /tmp/*; [ -e \"$1\" ] && n=1 || n=0; \
                 [ -e /home ] && h=home || h=no-home; echo \"$a $b $n $h $PATH\"; exit 4"
                    .into(),
            ],
            cwd: "/".into(),
            env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
            sandbox: Some(Sandbox {
                ops,
                fds: vec![9],
                root: false,
            }),
            via: None,
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        let extra = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(extra.path(), "fd nine\n").unwrap();
        let extra_r = std::fs::File::open(extra.path()).unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        send_fds(
            &stream,
            &serde_json::to_string(&r).unwrap(),
            &[
                null.as_fd(),
                out.as_file().as_fd(),
                out.as_file().as_fd(),
                extra_r.as_fd(),
            ],
        )
        .unwrap();
        let mut lines = BufReader::new(&stream).lines();
        let started: serde_json::Value =
            serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert!(started["pid"].as_u64().unwrap() > 0, "{started}");
        let done: serde_json::Value =
            serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        let written = std::fs::read_to_string(out.path()).unwrap();
        assert_eq!(done["status"], 4, "{done} {written}");
        assert_eq!(written, "data fd nine 0 no-home /usr/bin:/bin\n");
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
    }
}

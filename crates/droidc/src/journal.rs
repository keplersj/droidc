//! Stand-ins for journald's sockets, so the guest has somewhere to log to.
//!
//! Without systemd as PID 1 nothing listens on `/run/systemd/journal/`. `dbus-broker-launch` exits at
//! startup when the datagram socket is missing, and `systemd --user` logs "Failed to connect stdout to
//! the journal socket" for every unit and drops that output. This binds:
//!
//! * `/run/systemd/journal/socket`: native-protocol datagrams (`sd_journal_send`, the manager's own log);
//! * `/run/systemd/journal/stdout`: the stream socket units' stdout/stderr go to (`StandardOutput=journal`).
//!
//! What arrives is dropped, unless asked for: `DROIDC_JOURNAL=stderr` prints it, and `DROIDC_LOGCAT`
//! forwards it to Android's log (see [`crate::logcat`]).

use crate::logcat::{Level, Logcat};
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;

pub const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";
pub const STDOUT_SOCKET: &str = "/run/systemd/journal/stdout";

/// Where received records go.
#[derive(Debug, Default)]
pub struct Config {
    pub print: bool,
    pub logcat: Option<Logcat>,
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            print: print_enabled(),
            logcat: Logcat::from_env(),
        }
    }

    fn emit(&self, r: &Record) {
        if self.print {
            eprintln!("journal: {}", r.display());
        }
        if let Some(l) = &self.logcat {
            l.log(Level::from_syslog(r.priority), r.tag(), &r.message);
        }
    }
}

/// `DROIDC_JOURNAL=stderr`.
pub fn print_enabled() -> bool {
    std::env::var("DROIDC_JOURNAL").as_deref() == Ok("stderr")
}

/// Bind the datagram socket and, with `stream`, the stdout stream socket. Each is served by detached
/// threads for the life of the process. Best effort: a socket someone else already listens on is
/// left alone, and an unwritable `/run` just means there are no stand-ins.
pub fn start(stream: bool) {
    let cfg = Arc::new(Config::from_env());
    start_datagram_at(Path::new(JOURNAL_SOCKET), cfg.clone());
    if stream {
        start_stream_at(Path::new(STDOUT_SOCKET), cfg);
    }
}

/// A socket file left by a previous session refuses connections; only a live listener is kept.
fn prepare(path: &Path, live: impl Fn(&Path) -> bool) -> bool {
    if path.exists() && (live(path) || fs::remove_file(path).is_err()) {
        return false;
    }
    path.parent().is_none_or(|p| fs::create_dir_all(p).is_ok())
}

pub fn start_datagram_at(path: &Path, cfg: Arc<Config>) {
    let live = |p: &Path| UnixDatagram::unbound().and_then(|c| c.connect(p)).is_ok();
    if !prepare(path, live) {
        return;
    }
    let Ok(sock) = UnixDatagram::bind(path) else {
        return;
    };
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        while let Ok(n) = sock.recv(&mut buf) {
            if let Some(r) = Record::parse(&buf[..n]) {
                cfg.emit(&r);
            }
        }
    });
}

pub fn start_stream_at(path: &Path, cfg: Arc<Config>) {
    if !prepare(path, |p| UnixStream::connect(p).is_ok()) {
        return;
    }
    let Ok(listener) = UnixListener::bind(path) else {
        return;
    };
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let cfg = cfg.clone();
            std::thread::spawn(move || serve_stream(conn, &cfg));
        }
    });
}

/// journald's stdout stream protocol: seven header lines (identifier, unit, default priority,
/// level prefix, three forwarding flags), then log lines until EOF. With the level prefix on, a line
/// may start with `<N>` to set its own priority.
fn serve_stream(conn: UnixStream, cfg: &Config) {
    let mut r = BufReader::new(conn);
    let mut header = Vec::with_capacity(7);
    for _ in 0..7 {
        let mut l = String::new();
        if r.read_line(&mut l).unwrap_or(0) == 0 {
            return;
        }
        header.push(l.trim_end().to_string());
    }
    let ident = if header[0].is_empty() {
        header[1].clone()
    } else {
        header[0].clone()
    };
    // Units pass facility|level (a daemon's info is 3<<3|6 = 30); only the level matters here.
    let default_priority = header[2].parse::<u8>().map_or(6, |p| p & 7);
    let prefixed = header[3] == "1";
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match r.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\n', '\r']);
        let (priority, message) = match line.as_bytes() {
            [b'<', d @ b'0'..=b'7', b'>', ..] if prefixed => (d - b'0', &line[3..]),
            _ => (default_priority, line),
        };
        if !message.is_empty() {
            cfg.emit(&Record {
                ident: ident.clone(),
                priority,
                message: message.to_string(),
            });
        }
    }
}

/// One journal entry, reduced to what a log line needs.
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    pub ident: String,
    pub priority: u8,
    pub message: String,
}

impl Record {
    pub fn tag(&self) -> &str {
        if self.ident.is_empty() {
            "journal"
        } else {
            &self.ident
        }
    }

    pub fn display(&self) -> String {
        format!("{}: {}", self.ident, self.message)
    }

    /// A native-protocol datagram: `KEY=value\n` fields, with binary fields framed as
    /// `KEY\n<u64 LE length><data>\n`. `None` without a `MESSAGE`.
    pub fn parse(mut d: &[u8]) -> Option<Record> {
        let (mut ident, mut msg, mut priority) = (None, None, 6u8);
        while !d.is_empty() {
            let nl = d.iter().position(|&b| b == b'\n')?;
            let line = &d[..nl];
            let (key, val, used) = if let Some(eq) = line.iter().position(|&b| b == b'=') {
                (&line[..eq], &line[eq + 1..], nl + 1)
            } else {
                let len = u64::from_le_bytes(d.get(nl + 1..nl + 9)?.try_into().ok()?) as usize;
                (line, d.get(nl + 9..nl + 9 + len)?, nl + 9 + len + 1)
            };
            match key {
                b"MESSAGE" => msg = Some(String::from_utf8_lossy(val).into_owned()),
                b"PRIORITY" => {
                    priority = String::from_utf8_lossy(val)
                        .trim()
                        .parse()
                        .unwrap_or(priority)
                }
                b"SYSLOG_IDENTIFIER" | b"_COMM" if ident.is_none() => {
                    ident = Some(String::from_utf8_lossy(val).into_owned())
                }
                _ => {}
            }
            d = d.get(used..)?;
        }
        Some(Record {
            ident: ident.unwrap_or_default(),
            priority,
            message: msg?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    #[test]
    fn datagrams_are_decoded() {
        let mut d = b"PRIORITY=4\nSYSLOG_IDENTIFIER=systemd\nMESSAGE=Started x.\n".to_vec();
        let r = Record::parse(&d).unwrap();
        assert_eq!(r.display(), "systemd: Started x.");
        assert_eq!(r.priority, 4);
        // a binary-framed field: KEY\n<len le64>data\n
        d.extend(b"MULTI\n");
        d.extend(5u64.to_le_bytes());
        d.extend(b"a\nb\nc\n");
        d.extend(b"MESSAGE=after\n");
        assert_eq!(Record::parse(&d).unwrap().message, "after");
        assert_eq!(Record::parse(b"no newline"), None);
        assert_eq!(Record::parse(b"PRIORITY=3\n"), None, "no MESSAGE");
    }

    #[test]
    fn sink_replaces_stale_socket_and_keeps_live_one() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("journal/socket");
        // stale: bound then dropped, leaving the file behind with no listener
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        drop(UnixDatagram::bind(&p).unwrap());
        // A test forking concurrently holds a copy of the fd until its child execs: wait for the
        // socket to really be gone instead of asserting it at once.
        let stale = (0..200).any(|_| {
            let gone = UnixDatagram::unbound().unwrap().connect(&p).is_err();
            if !gone {
                std::thread::sleep(Duration::from_millis(10));
            }
            gone
        });
        assert!(stale, "stale socket still accepts connections");
        start_datagram_at(&p, Arc::default());
        let c = UnixDatagram::unbound().unwrap();
        c.connect(&p).expect("sink must accept connections");
        c.send(b"hello").unwrap();
        // live: a second call leaves the working socket alone
        start_datagram_at(&p, Arc::default());
        c.send(b"again").unwrap();
    }

    /// A fake logd, and a config forwarding to it.
    fn fake_logd(dir: &Path) -> (UnixDatagram, Arc<Config>) {
        let p = dir.join("logdw");
        let server = UnixDatagram::bind(&p).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let cfg = Config {
            print: false,
            logcat: Some(Logcat::open(&p, Level::Debug).unwrap()),
        };
        (server, Arc::new(cfg))
    }

    /// `(priority, tag, message)` of a logd datagram.
    fn entry(server: &UnixDatagram) -> (u8, String, String) {
        let mut buf = [0u8; 4096];
        let n = server.recv(&mut buf).unwrap();
        let body = &buf[11..n - 1];
        let mut parts = body[1..].splitn(2, |&b| b == 0);
        let tag = String::from_utf8_lossy(parts.next().unwrap()).into_owned();
        let msg = String::from_utf8_lossy(parts.next().unwrap()).into_owned();
        (body[0], tag, msg)
    }

    #[test]
    fn datagrams_reach_logcat() {
        let d = tempfile::tempdir().unwrap();
        let (logd, cfg) = fake_logd(d.path());
        let p = d.path().join("socket");
        start_datagram_at(&p, cfg);
        let c = UnixDatagram::unbound().unwrap();
        c.connect(&p).unwrap();
        c.send(b"PRIORITY=3\nSYSLOG_IDENTIFIER=gsd-color\nMESSAGE=broken\n")
            .unwrap();
        assert_eq!(entry(&logd), (6, "gsd-color".into(), "broken".into()));
    }

    #[test]
    fn unit_output_reaches_logcat_with_its_priorities() {
        let d = tempfile::tempdir().unwrap();
        let (logd, cfg) = fake_logd(d.path());
        let p = d.path().join("stdout");
        start_stream_at(&p, cfg);
        let mut c = UnixStream::connect(&p).unwrap();
        // identifier, unit, default priority, level prefix, then three forwarding flags
        c.write_all(b"gnome-shell\norg.gnome.Shell@wayland.service\n30\n1\n0\n0\n0\n")
            .unwrap();
        c.write_all(b"plain line\n<3>an error\n<9>not a prefix\n\npartial")
            .unwrap();
        drop(c);
        assert_eq!(entry(&logd), (4, "gnome-shell".into(), "plain line".into()));
        assert_eq!(entry(&logd), (6, "gnome-shell".into(), "an error".into()));
        assert_eq!(
            entry(&logd),
            (4, "gnome-shell".into(), "<9>not a prefix".into())
        );
        assert_eq!(entry(&logd), (4, "gnome-shell".into(), "partial".into()));
    }
}

//! Android's log, written the way `liblog` does it.
//!
//! `liblog` is bionic-only, so the guest cannot link it, but it only sends datagrams to logd's
//! `/dev/socket/logdw`, which every app may write to. Doing the same from here puts a guest message
//! in `adb logcat` under the app's own pid and uid (logd reads them from the socket credentials).
//! Needs `/dev` bound in the guest, which Selkie does.
//!
//! The wire format is `android_log_header_t` (`id: u8`, `tid: u16`, `sec: u32`, `nsec: u32`, all little
//! endian) followed by `priority: u8`, `tag\0`, `message\0`. It is logd's ABI to liblog rather than a
//! documented API; it has been stable since Android 7. Not verified on a device yet.
//!
//! Opt-in through `DROIDC_LOGCAT` (see [`Logcat::from_env`]): guest logs can hold paths and user names,
//! and logcat ends up in bug reports.

use std::os::unix::net::UnixDatagram;
use std::path::Path;

pub const LOGDW: &str = "/dev/socket/logdw";
const LOG_ID_MAIN: u8 = 0;
/// `LOGGER_ENTRY_MAX_PAYLOAD`, less room for the priority byte, the tag and the terminators.
const MAX_MESSAGE: usize = 3900;
const MAX_TAG: usize = 100;

/// Android log priorities (`android/log.h`); only the ones that matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 3,
    Info = 4,
    Warn = 5,
    Error = 6,
    Fatal = 7,
}

impl Level {
    /// syslog/journal `PRIORITY` (0 emerg .. 7 debug) to an Android priority.
    pub fn from_syslog(p: u8) -> Level {
        match p {
            0..=2 => Level::Fatal,
            3 => Level::Error,
            4 => Level::Warn,
            5 | 6 => Level::Info,
            _ => Level::Debug,
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "debug" => Some(Level::Debug),
            "info" | "auto" | "on" | "1" | "true" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Logcat {
    sock: UnixDatagram,
    min: Level,
}

impl Logcat {
    /// `DROIDC_LOGCAT=off` (or unset): nothing. `auto`/`on`/`info`: info and above. `debug`, `warn`,
    /// `error`: that level and above. `DROIDC_LOGDW` overrides the socket path (tests). `None` when
    /// disabled or when there is no logd to talk to (not Android).
    pub fn from_env() -> Option<Logcat> {
        let level = Level::parse(std::env::var("DROIDC_LOGCAT").ok()?.trim())?;
        let path = std::env::var("DROIDC_LOGDW").unwrap_or_else(|_| LOGDW.to_string());
        Logcat::open(Path::new(&path), level).ok()
    }

    pub fn open(path: &Path, min: Level) -> std::io::Result<Logcat> {
        let sock = UnixDatagram::unbound()?;
        sock.connect(path)?;
        Ok(Logcat { sock, min })
    }

    pub fn try_clone(&self) -> std::io::Result<Logcat> {
        Ok(Logcat {
            sock: self.sock.try_clone()?,
            min: self.min,
        })
    }

    /// Send `message` (one entry per line, long lines split) below `tag`. Best effort: logd drops what
    /// it throttles and there is nobody to tell.
    pub fn log(&self, level: Level, tag: &str, message: &str) {
        let _ = self.try_log(level, tag, message);
    }

    /// [`Logcat::log`], reporting a failure to reach logd.
    pub fn try_log(&self, level: Level, tag: &str, message: &str) -> std::io::Result<()> {
        if level < self.min {
            return Ok(());
        }
        let tid = rustix::process::getpid().as_raw_nonzero().get() as u16;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        for line in message.lines().filter(|l| !l.trim().is_empty()) {
            for chunk in chunks(line, MAX_MESSAGE) {
                self.sock.send(&encode(
                    level,
                    tag,
                    chunk,
                    tid,
                    now.as_secs() as u32,
                    now.subsec_nanos(),
                ))?;
            }
        }
        Ok(())
    }
}

/// `droidc log`: one message at `priority` (a level name), whatever `DROIDC_LOGCAT` says.
pub fn write(priority: &str, tag: &str, message: &str) -> Result<(), String> {
    let level = Level::parse(priority).ok_or_else(|| format!("unknown priority '{priority}'"))?;
    let path = std::env::var("DROIDC_LOGDW").unwrap_or_else(|_| LOGDW.to_string());
    let l = Logcat::open(Path::new(&path), Level::Debug).map_err(|e| format!("{path}: {e}"))?;
    l.try_log(level, tag, message)
        .map_err(|e| format!("{path}: {e}"))
}

/// `s` in pieces of at most `max` bytes, cut on character boundaries.
fn chunks(s: &str, max: usize) -> Vec<&str> {
    let mut out = vec![];
    let mut rest = s;
    while rest.len() > max {
        let mut at = max;
        while !rest.is_char_boundary(at) {
            at -= 1;
        }
        out.push(&rest[..at]);
        rest = &rest[at..];
    }
    out.push(rest);
    out
}

fn encode(level: Level, tag: &str, message: &str, tid: u16, sec: u32, nsec: u32) -> Vec<u8> {
    let tag = chunks(tag, MAX_TAG)[0];
    let mut d = Vec::with_capacity(16 + tag.len() + message.len());
    d.push(LOG_ID_MAIN);
    d.extend(tid.to_le_bytes());
    d.extend(sec.to_le_bytes());
    d.extend(nsec.to_le_bytes());
    d.push(level as u8);
    d.extend(tag.bytes().filter(|&b| b != 0));
    d.push(0);
    d.extend(message.bytes().filter(|&b| b != 0));
    d.push(0);
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_matches_liblog() {
        let d = encode(Level::Warn, "gnome-shell", "hi", 0x1234, 0x0102_0304, 5);
        assert_eq!(d[0], 0, "main buffer");
        assert_eq!(&d[1..3], &[0x34, 0x12]);
        assert_eq!(&d[3..7], &[4, 3, 2, 1]);
        assert_eq!(&d[7..11], &[5, 0, 0, 0]);
        assert_eq!(&d[11..], b"\x05gnome-shell\0hi\0");
    }

    #[test]
    fn syslog_priorities_map_to_android_levels() {
        let m: Vec<Level> = (0..=7).map(Level::from_syslog).collect();
        assert_eq!(
            m,
            [
                Level::Fatal,
                Level::Fatal,
                Level::Fatal,
                Level::Error,
                Level::Warn,
                Level::Info,
                Level::Info,
                Level::Debug
            ]
        );
    }

    #[test]
    fn long_and_multibyte_messages_are_split_on_boundaries() {
        let s = "é".repeat(5000); // 10000 bytes
        let c = chunks(&s, 3900);
        assert!(c.iter().all(|p| p.len() <= 3900));
        assert_eq!(c.concat(), s);
        assert_eq!(chunks("short", 100), ["short"]);
    }

    #[test]
    fn sends_entries_above_the_minimum_level_one_per_line() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("logdw");
        let server = UnixDatagram::bind(&p).unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let l = Logcat::open(&p, Level::Info).unwrap();
        l.log(Level::Debug, "t", "dropped");
        l.log(Level::Error, "tag", "one\n\ntwo\n");
        let mut buf = [0u8; 4096];
        let mut got = vec![];
        for _ in 0..2 {
            let n = server.recv(&mut buf).unwrap();
            got.push(buf[11..n].to_vec());
        }
        assert_eq!(got[0], b"\x06tag\0one\0");
        assert_eq!(got[1], b"\x06tag\0two\0");
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        assert!(server.recv(&mut buf).is_err(), "nothing else was sent");
    }

    #[test]
    fn environment_selects_the_level() {
        assert_eq!(Level::parse("auto"), Some(Level::Info));
        assert_eq!(Level::parse("debug"), Some(Level::Debug));
        assert_eq!(Level::parse("error"), Some(Level::Error));
        assert_eq!(Level::parse("off"), None);
        assert_eq!(Level::parse("nonsense"), None);
    }
}

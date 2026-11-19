//! `droidc login1`: a stand-in for `systemd-logind` on the system bus.
//!
//! There is no logind in the guest (no systemd PID 1, no cgroups). GNOME still talks to
//! `org.freedesktop.login1`: gnome-shell/mutter and gnome-session look up their session, the power
//! menu asks what is allowed, and `Inhibit` hands out fds. This serves exactly one session, one seat
//! and one user, all permanently active, and refuses what cannot work here (`TakeDevice`, power
//! operations). It also writes the `/run/systemd/{sessions,seats,users}` state files that
//! libsystemd's `sd_session_*`/`sd_uid_*` read without asking the bus.
//!
//! The first cut covers what the supervisor's session needs; widen it from a `busctl monitor` trace of
//! a real login (see `docs/systemd-on-proot.md`), not by guessing.

use crate::error::{Context, Error, Result};
use std::fs;
use std::path::{Path, PathBuf};
use zbus::fdo;
use zbus::zvariant::{ObjectPath, OwnedFd, OwnedObjectPath};

pub const NAME: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const SEAT: &str = "seat0";
const SESSION_ID: &str = "1";

/// Who the single session belongs to.
#[derive(Debug, Clone)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    pub user: String,
    /// The supervisor: the session leader.
    pub leader: u32,
    pub desktop: String,
    pub started_usec: u64,
}

impl Identity {
    /// The calling process's identity, with the desktop name from `XDG_CURRENT_DESKTOP`.
    pub fn current() -> Identity {
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let user = user_name(uid, Path::new("/etc/passwd"))
            .or_else(|| std::env::var("USER").ok().filter(|u| !u.is_empty()))
            .unwrap_or_else(|| uid.to_string());
        let started_usec = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        Identity {
            uid,
            gid,
            user,
            leader: rustix::process::getppid().map_or(0, |p| p.as_raw_nonzero().get() as u32),
            desktop: std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
            started_usec,
        }
    }
}

fn user_name(uid: u32, passwd: &Path) -> Option<String> {
    let text = fs::read_to_string(passwd).ok()?;
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 2 && f[2].parse::<u32>().ok() == Some(uid)).then(|| f[0].to_string())
    })
}

// systemd's bus_label_escape: a leading digit is escaped, so session "1" lives at `session/_31`;
// user paths are `_<uid>`; seat0 needs no escaping.
fn session_path() -> OwnedObjectPath {
    path(&format!("/org/freedesktop/login1/session/_3{SESSION_ID}"))
}
fn seat_path() -> OwnedObjectPath {
    path(&format!("/org/freedesktop/login1/seat/{SEAT}"))
}
fn user_path(uid: u32) -> OwnedObjectPath {
    path(&format!("/org/freedesktop/login1/user/_{uid}"))
}
fn path(p: &str) -> OwnedObjectPath {
    ObjectPath::try_from(p.to_string()).unwrap().into()
}

fn not_supported(what: &str) -> fdo::Error {
    fdo::Error::NotSupported(format!("{what} is not available in this environment"))
}

struct Manager {
    id: Identity,
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    fn get_session(&self, id: &str) -> fdo::Result<OwnedObjectPath> {
        match id {
            SESSION_ID | "auto" | "self" => Ok(session_path()),
            _ => Err(fdo::Error::Failed(format!("No session '{id}' known"))),
        }
    }
    fn get_session_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        session_path()
    }
    fn get_seat(&self, id: &str) -> fdo::Result<OwnedObjectPath> {
        match id {
            SEAT | "auto" => Ok(seat_path()),
            _ => Err(fdo::Error::Failed(format!("No seat '{id}' known"))),
        }
    }
    fn get_user(&self, uid: u32) -> fdo::Result<OwnedObjectPath> {
        if uid == self.id.uid {
            Ok(user_path(uid))
        } else {
            Err(fdo::Error::Failed(format!("No user '{uid}' known")))
        }
    }
    fn get_user_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        user_path(self.id.uid)
    }
    /// `a(susso)`: id, uid, user, seat, object path.
    fn list_sessions(&self) -> Vec<(String, u32, String, String, OwnedObjectPath)> {
        vec![(
            SESSION_ID.into(),
            self.id.uid,
            self.id.user.clone(),
            SEAT.into(),
            session_path(),
        )]
    }
    fn list_seats(&self) -> Vec<(String, OwnedObjectPath)> {
        vec![(SEAT.into(), seat_path())]
    }
    fn list_users(&self) -> Vec<(u32, String, OwnedObjectPath)> {
        vec![(self.id.uid, self.id.user.clone(), user_path(self.id.uid))]
    }
    /// An fd whose lifetime is the lock. Nothing here ever suspends or shuts down, so the lock is
    /// vacuous: hand out the read end of a pipe whose write end is already closed.
    fn inhibit(&self, _what: &str, _who: &str, _why: &str, _mode: &str) -> fdo::Result<OwnedFd> {
        let (r, _w) = rustix::pipe::pipe().map_err(|e| fdo::Error::Failed(e.to_string()))?;
        Ok(OwnedFd::from(r))
    }
    fn activate_session(&self, _id: &str) {}
    fn lock_session(&self, _id: &str) {}
    fn unlock_session(&self, _id: &str) {}
    fn lock_sessions(&self) {}
    fn unlock_sessions(&self) {}
    fn power_off(&self, _interactive: bool) -> fdo::Result<()> {
        Err(not_supported("PowerOff"))
    }
    fn reboot(&self, _interactive: bool) -> fdo::Result<()> {
        Err(not_supported("Reboot"))
    }
    fn suspend(&self, _interactive: bool) -> fdo::Result<()> {
        Err(not_supported("Suspend"))
    }
    fn hibernate(&self, _interactive: bool) -> fdo::Result<()> {
        Err(not_supported("Hibernate"))
    }
    fn can_power_off(&self) -> &'static str {
        "na"
    }
    fn can_reboot(&self) -> &'static str {
        "na"
    }
    fn can_suspend(&self) -> &'static str {
        "na"
    }
    fn can_hibernate(&self) -> &'static str {
        "na"
    }
    fn can_hybrid_sleep(&self) -> &'static str {
        "na"
    }
    fn can_suspend_then_hibernate(&self) -> &'static str {
        "na"
    }
    #[zbus(property, name = "NAutoVTs")]
    fn n_auto_vts(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn idle_hint(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn block_inhibited(&self) -> &'static str {
        ""
    }
    #[zbus(property)]
    fn delay_inhibited(&self) -> &'static str {
        ""
    }
    #[zbus(property)]
    fn preparing_for_shutdown(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn preparing_for_sleep(&self) -> bool {
        false
    }
}

struct Session {
    id: Identity,
}

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl Session {
    fn activate(&self) {}
    fn lock(&self) {}
    fn unlock(&self) {}
    fn terminate(&self) -> fdo::Result<()> {
        Err(not_supported("Terminate"))
    }
    fn set_idle_hint(&self, _idle: bool) {}
    fn set_locked_hint(&self, _locked: bool) {}
    fn set_type(&self, _t: &str) {}
    fn set_display(&self, _d: &str) {}
    fn take_control(&self, _force: bool) {}
    fn release_control(&self) {}
    /// Device access needs the real logind (and a kernel DRM/input device): refused, so a native
    /// mutter backend fails cleanly. Headless and nested backends do not call this.
    fn take_device(&self, _major: u32, _minor: u32) -> fdo::Result<(OwnedFd, bool)> {
        Err(not_supported("TakeDevice"))
    }
    fn release_device(&self, _major: u32, _minor: u32) {}
    fn pause_device_complete(&self, _major: u32, _minor: u32) {}
    fn set_brightness(&self, _subsystem: &str, _name: &str, _brightness: u32) -> fdo::Result<()> {
        Err(not_supported("SetBrightness"))
    }
    #[zbus(property)]
    fn id(&self) -> &'static str {
        SESSION_ID
    }
    #[zbus(property)]
    fn user(&self) -> (u32, OwnedObjectPath) {
        (self.id.uid, user_path(self.id.uid))
    }
    #[zbus(property)]
    fn name(&self) -> String {
        self.id.user.clone()
    }
    #[zbus(property)]
    fn timestamp(&self) -> u64 {
        self.id.started_usec
    }
    #[zbus(property)]
    fn timestamp_monotonic(&self) -> u64 {
        0
    }
    #[zbus(property, name = "VTNr")]
    fn vtnr(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn seat(&self) -> (String, OwnedObjectPath) {
        (SEAT.into(), seat_path())
    }
    #[zbus(property)]
    fn display(&self) -> &'static str {
        ""
    }
    #[zbus(property)]
    fn remote(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn remote_host(&self) -> &'static str {
        ""
    }
    #[zbus(property)]
    fn remote_user(&self) -> &'static str {
        ""
    }
    #[zbus(property)]
    fn service(&self) -> &'static str {
        "droidc"
    }
    #[zbus(property)]
    fn desktop(&self) -> String {
        self.id.desktop.clone()
    }
    #[zbus(property)]
    fn scope(&self) -> String {
        format!("session-{SESSION_ID}.scope")
    }
    #[zbus(property)]
    fn leader(&self) -> u32 {
        self.id.leader
    }
    #[zbus(property)]
    fn audit(&self) -> u32 {
        0
    }
    #[zbus(property, name = "Type")]
    fn type_(&self) -> &'static str {
        "wayland"
    }
    #[zbus(property)]
    fn class(&self) -> &'static str {
        "user"
    }
    #[zbus(property)]
    fn active(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn state(&self) -> &'static str {
        "active"
    }
    #[zbus(property)]
    fn idle_hint(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn idle_since_hint(&self) -> u64 {
        0
    }
    #[zbus(property)]
    fn idle_since_hint_monotonic(&self) -> u64 {
        0
    }
    #[zbus(property)]
    fn locked_hint(&self) -> bool {
        false
    }
    #[zbus(property, name = "TTY")]
    fn tty(&self) -> &'static str {
        ""
    }
}

struct Seat {
    id: Identity,
}

#[zbus::interface(name = "org.freedesktop.login1.Seat")]
impl Seat {
    fn activate_session(&self, _id: &str) {}
    fn switch_to(&self, _vt: u32) -> fdo::Result<()> {
        Err(not_supported("SwitchTo"))
    }
    #[zbus(property)]
    fn id(&self) -> &'static str {
        SEAT
    }
    #[zbus(property)]
    fn active_session(&self) -> (String, OwnedObjectPath) {
        (SESSION_ID.into(), session_path())
    }
    #[zbus(property)]
    fn sessions(&self) -> Vec<(String, OwnedObjectPath)> {
        vec![(SESSION_ID.into(), session_path())]
    }
    #[zbus(property)]
    fn can_graphical(&self) -> bool {
        true
    }
    #[zbus(property, name = "CanTTY")]
    fn can_tty(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_multi_session(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn idle_hint(&self) -> bool {
        let _ = &self.id;
        false
    }
}

struct User {
    id: Identity,
}

#[zbus::interface(name = "org.freedesktop.login1.User")]
impl User {
    #[zbus(property, name = "UID")]
    fn uid(&self) -> u32 {
        self.id.uid
    }
    #[zbus(property, name = "GID")]
    fn gid(&self) -> u32 {
        self.id.gid
    }
    #[zbus(property)]
    fn name(&self) -> String {
        self.id.user.clone()
    }
    #[zbus(property)]
    fn state(&self) -> &'static str {
        "active"
    }
    #[zbus(property)]
    fn sessions(&self) -> Vec<(String, OwnedObjectPath)> {
        vec![(SESSION_ID.into(), session_path())]
    }
    #[zbus(property)]
    fn display(&self) -> (String, OwnedObjectPath) {
        (SESSION_ID.into(), session_path())
    }
    #[zbus(property)]
    fn service(&self) -> String {
        format!("user@{}.service", self.id.uid)
    }
    #[zbus(property)]
    fn slice(&self) -> String {
        format!("user-{}.slice", self.id.uid)
    }
    #[zbus(property)]
    fn runtime_path(&self) -> String {
        format!("/run/user/{}", self.id.uid)
    }
    #[zbus(property)]
    fn idle_hint(&self) -> bool {
        false
    }
}

/// The `/run/systemd/{sessions,seats,users}` files libsystemd reads directly.
pub fn write_state(run: &Path, id: &Identity) -> std::io::Result<()> {
    let put = |rel: &str, body: String| -> std::io::Result<()> {
        let p: PathBuf = run.join("systemd").join(rel);
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(p, body)
    };
    put(
        &format!("sessions/{SESSION_ID}"),
        format!(
            "# This is private data. Do not parse.\nUID={uid}\nUSER={user}\nACTIVE=1\nIS_DISPLAY=1\n\
             STATE=active\nREMOTE=0\nSERVICE=droidc\nDESKTOP={desk}\nTYPE=wayland\nCLASS=user\n\
             SCOPE=session-{SESSION_ID}.scope\nSEAT={SEAT}\nLEADER={leader}\n",
            uid = id.uid,
            user = id.user,
            desk = id.desktop,
            leader = id.leader
        ),
    )?;
    put(
        &format!("seats/{SEAT}"),
        format!(
            "# This is private data. Do not parse.\nIS_SEAT0=1\nCAN_MULTI_SESSION=0\nCAN_TTY=0\n\
             CAN_GRAPHICAL=1\nACTIVE={SESSION_ID}\nACTIVE_UID={}\nSESSIONS={SESSION_ID}\nUIDS={}\n",
            id.uid, id.uid
        ),
    )?;
    put(
        &format!("users/{}", id.uid),
        format!(
            "# This is private data. Do not parse.\nNAME={}\nSTATE=active\nRUNTIME=/run/user/{}\n\
             SLICE=user-{}.slice\nDISPLAY={SESSION_ID}\nSERVICE=user@{}.service\nREALTIME=0\n\
             MONOTONIC=0\nSESSIONS={SESSION_ID}\nSEATS={SEAT}\nONLINE_SESSIONS={SESSION_ID}\n\
             ACTIVE_SESSIONS={SESSION_ID}\n",
            id.user, id.uid, id.uid, id.uid
        ),
    )
}

/// Claims `org.freedesktop.login1` on the bus at `address` and returns the live connection.
/// Fails with [`Error`] kind `locked` when a login1 already owns the name.
pub fn serve(address: &str, id: Identity) -> Result<zbus::blocking::Connection> {
    let ctx = |e: zbus::Error| Error::internal(format!("login1: {e}"));
    zbus::blocking::connection::Builder::address(address)
        .map_err(ctx)?
        .serve_at(MANAGER_PATH, Manager { id: id.clone() })
        .map_err(ctx)?
        .serve_at(session_path(), Session { id: id.clone() })
        .map_err(ctx)?
        .serve_at(seat_path(), Seat { id: id.clone() })
        .map_err(ctx)?
        .serve_at(user_path(id.uid), User { id: id.clone() })
        .map_err(ctx)?
        .name(NAME)
        .map_err(ctx)?
        // Never take the name from a real logind (or another stub), and never let one take ours.
        .replace_existing_names(false)
        .allow_name_replacements(false)
        .build()
        .map_err(|e| match e {
            zbus::Error::NameTaken => Error::new(
                crate::error::ErrorKind::Locked,
                format!("{NAME} is already owned"),
            ),
            e => ctx(e),
        })
}

/// `droidc login1`: serve on `$DBUS_SYSTEM_BUS_ADDRESS` until killed.
pub fn run() -> Result<i32> {
    let address = std::env::var("DBUS_SYSTEM_BUS_ADDRESS")
        .ok()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| Error::invalid("DBUS_SYSTEM_BUS_ADDRESS is not set"))?;
    let id = Identity::current();
    if let Err(e) = write_state(Path::new("/run"), &id) {
        eprintln!("warn: login1: cannot write /run/systemd state: {e}");
    }
    let _conn = match serve(&address, id) {
        Ok(c) => c,
        Err(e) if e.kind == crate::error::ErrorKind::Locked => {
            eprintln!("droidc login1: {e}; leaving the existing service in charge");
            return Ok(0);
        }
        Err(e) => return Err(e).context("login1"),
    };
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn id() -> Identity {
        Identity {
            uid: 1000,
            gid: 1000,
            user: "kepler".into(),
            leader: 42,
            desktop: "GNOME".into(),
            started_usec: 1,
        }
    }

    #[test]
    fn paths_follow_systemd_escaping() {
        assert_eq!(
            session_path().as_str(),
            "/org/freedesktop/login1/session/_31"
        );
        assert_eq!(
            user_path(1000).as_str(),
            "/org/freedesktop/login1/user/_1000"
        );
        assert_eq!(seat_path().as_str(), "/org/freedesktop/login1/seat/seat0");
    }

    #[test]
    fn state_files_are_written() {
        let d = tempfile::tempdir().unwrap();
        write_state(d.path(), &id()).unwrap();
        let s = fs::read_to_string(d.path().join("systemd/sessions/1")).unwrap();
        assert!(s.contains("UID=1000") && s.contains("TYPE=wayland") && s.contains("SEAT=seat0"));
        assert!(fs::read_to_string(d.path().join("systemd/users/1000"))
            .unwrap()
            .contains("STATE=active"));
        assert!(fs::read_to_string(d.path().join("systemd/seats/seat0"))
            .unwrap()
            .contains("CAN_GRAPHICAL=1"));
    }

    #[test]
    fn user_name_comes_from_passwd() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("passwd");
        fs::write(
            &p,
            "root:x:0:0::/root:/bin/sh\nselkie:x:10123:10123::/root:/bin/sh\n",
        )
        .unwrap();
        assert_eq!(user_name(10123, &p).as_deref(), Some("selkie"));
        assert_eq!(user_name(7, &p), None);
    }

    /// Serve on a private dbus-daemon and call it the way gnome-session and mutter do.
    #[test]
    fn answers_the_calls_gnome_makes() {
        if which("dbus-daemon").is_none() {
            eprintln!("skipping: no dbus-daemon");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("bus");
        let mut bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!("--address=unix:path={}", sock.display()))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut addr = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(bus.stdout.take().unwrap()),
            &mut addr,
        )
        .unwrap();
        let addr = addr.trim().to_string();
        let _server = serve(&addr, id()).unwrap();
        // a second login1 does not take the name over
        assert_eq!(
            serve(&addr, id()).unwrap_err().kind,
            crate::error::ErrorKind::Locked
        );

        let c = zbus::blocking::connection::Builder::address(addr.as_str())
            .unwrap()
            .build()
            .unwrap();
        let m = "org.freedesktop.login1.Manager";
        let s: OwnedObjectPath = call(&c, MANAGER_PATH, m, "GetSession", &("auto",))
            .unwrap()
            .body()
            .deserialize()
            .unwrap();
        assert_eq!(s.as_str(), "/org/freedesktop/login1/session/_31");
        assert!(call(&c, MANAGER_PATH, m, "GetSession", &("nope",)).is_err());
        let (cs,): (String,) = call(&c, MANAGER_PATH, m, "CanSuspend", &())
            .unwrap()
            .body()
            .deserialize()
            .unwrap();
        let _ = cs;
        call(
            &c,
            MANAGER_PATH,
            m,
            "Inhibit",
            &("sleep", "gsd", "x", "delay"),
        )
        .unwrap();
        assert!(call(&c, MANAGER_PATH, m, "Suspend", &(true,)).is_err());
        let sess = "org.freedesktop.login1.Session";
        let get = |name: &str| -> zbus::zvariant::OwnedValue {
            let r = c
                .call_method(
                    Some(NAME),
                    "/org/freedesktop/login1/session/_31",
                    Some("org.freedesktop.DBus.Properties"),
                    "Get",
                    &(sess, name),
                )
                .unwrap();
            let v: zbus::zvariant::OwnedValue = r.body().deserialize().unwrap();
            v
        };
        assert_eq!(String::try_from(get("Type")).unwrap(), "wayland");
        assert!(bool::try_from(get("Active")).unwrap());
        assert!(call(
            &c,
            "/org/freedesktop/login1/session/_31",
            sess,
            "TakeDevice",
            &(226u32, 0u32)
        )
        .is_err());
        let _ = bus.kill();
        let _ = bus.wait();
    }

    fn call<B>(
        c: &zbus::blocking::Connection,
        path: &str,
        iface: &str,
        method: &str,
        body: &B,
    ) -> zbus::Result<zbus::Message>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        c.call_method(Some(NAME), path, Some(iface), method, body)
    }

    fn which(name: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    }
}

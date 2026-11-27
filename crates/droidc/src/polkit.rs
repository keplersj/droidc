//! `droidc polkit`: a stand-in for `polkitd` on the system bus.
//!
//! There is no polkitd in the guest (it is a system service, and nothing here runs those), so every
//! client that asks `org.freedesktop.PolicyKit1` whether the user may do something gets no answer.
//! The one that matters is `flatpak-system-helper`: Bazaar and `flatpak install` run as the user and
//! ask the helper to deploy into the system installation, and the helper asks polkit first.
//!
//! The guest has one user, the device's owner, who already gets root without a password through the
//! spawner (`sudo`, see `spawn.rs`), and everything in it runs as the app's uid. So this answers yes
//! for that uid (and for root) and no for anyone else, never asks for a password, and keeps no
//! temporary authorizations. It also accepts an authentication agent (gnome-shell registers one at
//! startup) and never calls it.

use crate::error::{Context, Error, Result};
use std::collections::HashMap;
use zbus::fdo;
use zbus::zvariant::OwnedValue;

pub const NAME: &str = "org.freedesktop.PolicyKit1";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";

/// polkit's `(sa{sv})` subject: a kind (`unix-process`, `system-bus-name`, `unix-session`) and its
/// details.
type Subject = (String, HashMap<String, OwnedValue>);

/// `(ssssssuuua{ss})`: action id, description, message, vendor, vendor URL, icon, the three implicit
/// authorizations, annotations.
type ActionDescription = (
    String,
    String,
    String,
    String,
    String,
    String,
    u32,
    u32,
    u32,
    HashMap<String, String>,
);

/// polkit's `(bba{ss})` authorization result: is_authorized, is_challenge, details. Returned wrapped
/// in a one-element tuple, so the reply is a single `(bba{ss})` argument as libpolkit reads it.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, zbus::zvariant::Type)]
struct AuthorizationResult(bool, bool, HashMap<String, String>);

struct Authority {
    /// The uid allowed besides root: the app's, which is the guest's only user.
    uid: u32,
}

/// The uid of a `/proc/<pid>/status` `Uid:` line (the real uid).
fn proc_uid(pid: u32) -> Option<u32> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn detail_u32(details: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    let v = details.get(key)?;
    u32::try_from(v)
        .ok()
        .or_else(|| i32::try_from(v).ok().and_then(|i| u32::try_from(i).ok()))
}

impl Authority {
    /// Whose request this is, or `None` when the subject does not say.
    async fn subject_uid(&self, conn: &zbus::Connection, subject: &Subject) -> Option<u32> {
        let (kind, details) = subject;
        match kind.as_str() {
            // The process's own uid where it can be read, as polkitd does, else what the caller says.
            "unix-process" => detail_u32(details, "pid")
                .and_then(proc_uid)
                .or_else(|| detail_u32(details, "uid").filter(|u| *u != u32::MAX)),
            "system-bus-name" => {
                let name = details.get("name")?;
                let name = <&str>::try_from(name).ok()?.to_string();
                let bus = fdo::DBusProxy::new(conn).await.ok()?;
                let name = zbus::names::BusName::try_from(name).ok()?;
                bus.get_connection_unix_user(name).await.ok()
            }
            // The guest has one session, the user's.
            "unix-session" => Some(self.uid),
            _ => None,
        }
    }

    fn allowed(&self, uid: Option<u32>) -> bool {
        matches!(uid, Some(u) if u == self.uid || u == 0)
    }
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
impl Authority {
    /// Never a challenge: there is no password to ask for here.
    async fn check_authorization(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        subject: Subject,
        action_id: String,
        _details: HashMap<String, String>,
        _flags: u32,
        _cancellation_id: String,
    ) -> (AuthorizationResult,) {
        let uid = self.subject_uid(conn, &subject).await;
        let ok = self.allowed(uid);
        if !ok {
            eprintln!(
                "droidc polkit: refused {action_id} for {} (uid {uid:?})",
                subject.0
            );
        }
        // A one-element tuple: zbus sends a bare struct's fields as separate out arguments.
        (AuthorizationResult(ok, false, HashMap::new()),)
    }

    fn cancel_check_authorization(&self, _cancellation_id: String) {}

    /// `a(ssssssuuua{ss})`: no action descriptions are read here.
    fn enumerate_actions(&self, _locale: String) -> Vec<ActionDescription> {
        Vec::new()
    }

    fn register_authentication_agent(
        &self,
        _subject: Subject,
        _locale: String,
        _object_path: String,
    ) {
    }

    fn register_authentication_agent_with_options(
        &self,
        _subject: Subject,
        _locale: String,
        _object_path: String,
        _options: HashMap<String, OwnedValue>,
    ) {
    }

    fn unregister_authentication_agent(&self, _subject: Subject, _object_path: String) {}

    fn authentication_agent_response(
        &self,
        _cookie: String,
        _identity: Subject,
    ) -> fdo::Result<()> {
        Err(fdo::Error::Failed("no authentication is pending".into()))
    }

    fn authentication_agent_response2(
        &self,
        _uid: u32,
        _cookie: String,
        _identity: Subject,
    ) -> fdo::Result<()> {
        Err(fdo::Error::Failed("no authentication is pending".into()))
    }

    /// `a(ss(sa{sv})tt)`: none are kept.
    fn enumerate_temporary_authorizations(
        &self,
        _subject: Subject,
    ) -> Vec<(String, String, Subject, u64, u64)> {
        Vec::new()
    }

    fn revoke_temporary_authorizations(&self, _subject: Subject) {}

    fn revoke_temporary_authorization_by_id(&self, _id: String) {}

    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn backend_name(&self) -> &'static str {
        "droidc"
    }

    #[zbus(property)]
    fn backend_version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// No temporary authorizations (polkit's feature bit 1).
    #[zbus(property)]
    fn backend_features(&self) -> u32 {
        0
    }
}

pub fn serve(address: &str, uid: u32) -> Result<zbus::blocking::Connection> {
    let ctx = |e: zbus::Error| Error::internal(format!("polkit: {e}"));
    zbus::blocking::connection::Builder::address(address)
        .map_err(ctx)?
        .serve_at(AUTHORITY_PATH, Authority { uid })
        .map_err(ctx)?
        .name(NAME)
        .map_err(ctx)?
        // Never take the name from a real polkitd, and never let one take ours.
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

/// `droidc polkit`: serve on `$DBUS_SYSTEM_BUS_ADDRESS` until killed.
pub fn run() -> Result<i32> {
    let address = std::env::var("DBUS_SYSTEM_BUS_ADDRESS")
        .ok()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| Error::invalid("DBUS_SYSTEM_BUS_ADDRESS is not set"))?;
    let _conn = match serve(&address, rustix::process::getuid().as_raw()) {
        Ok(c) => c,
        Err(e) if e.kind == crate::error::ErrorKind::Locked => {
            eprintln!("droidc polkit: {e}; leaving the existing service in charge");
            return Ok(0);
        }
        Err(e) => return Err(e).context("polkit"),
    };
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use zbus::zvariant::Value;

    fn which(name: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    }

    /// The body signature as sent: zbus's own `Signature` shows one struct argument and three
    /// separate arguments alike, so read the header's signature field (code 8, type `g`).
    fn wire_signature(raw: &[u8]) -> &str {
        let at = raw.windows(4).position(|w| w == b"\x08\x01g\x00").unwrap() + 4;
        std::str::from_utf8(&raw[at + 1..at + 1 + raw[at] as usize]).unwrap()
    }

    fn check(
        c: &zbus::blocking::Connection,
        subject: (&str, HashMap<&str, Value>),
    ) -> AuthorizationResult {
        let reply = c
            .call_method(
                Some(NAME),
                AUTHORITY_PATH,
                Some("org.freedesktop.PolicyKit1.Authority"),
                "CheckAuthorization",
                &(
                    subject,
                    "org.freedesktop.Flatpak.app-install",
                    HashMap::<&str, &str>::new(),
                    1u32,
                    "",
                ),
            )
            .unwrap();
        // One (bba{ss}) out argument, not three: libpolkit reads the reply as "(@(bba{ss}))".
        let body = reply.body();
        assert_eq!(wire_signature(reply.data()), "(bba{ss})");
        let (r,): (AuthorizationResult,) = body.deserialize().unwrap();
        r
    }

    /// Serve on a private dbus-daemon and ask the way flatpak-system-helper and pkcheck do.
    #[test]
    fn authorizes_the_user_and_root_and_nobody_else() {
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
        let me = rustix::process::getuid().as_raw();
        let _server = serve(&addr, me).unwrap();
        assert_eq!(
            serve(&addr, me).unwrap_err().kind,
            crate::error::ErrorKind::Locked
        );

        let c = zbus::blocking::connection::Builder::address(addr.as_str())
            .unwrap()
            .build()
            .unwrap();
        // flatpak-system-helper: the caller's bus name
        let name = c.unique_name().unwrap().to_string();
        let r = check(
            &c,
            (
                "system-bus-name",
                HashMap::from([("name", Value::from(name))]),
            ),
        );
        assert_eq!(r, AuthorizationResult(true, false, HashMap::new()));
        // pkcheck --process: a pid (ours), with or without its uid
        let pid = std::process::id();
        assert!(
            check(
                &c,
                (
                    "unix-process",
                    HashMap::from([("pid", Value::from(pid)), ("start-time", Value::from(0u64))])
                )
            )
            .0
        );
        assert!(
            check(
                &c,
                (
                    "unix-process",
                    HashMap::from([("pid", Value::from(pid)), ("uid", Value::from(0i32))])
                )
            )
            .0
        );
        // someone else (a process that is gone, so only its stated uid is left)
        assert!(
            !check(
                &c,
                (
                    "unix-process",
                    HashMap::from([
                        ("pid", Value::from(u32::MAX - 1)),
                        ("uid", Value::from((me + 1) as i32))
                    ])
                )
            )
            .0
        );
        assert!(!check(&c, ("no-such-kind", HashMap::new())).0);
        let _ = bus.kill();
        let _ = bus.wait();
    }
}

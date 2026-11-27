//! Keeping Android's screen on while the desktop asks not to idle, and leaving screen lock to Android.
//!
//! GNOME apps that must not be interrupted (a video, a presentation, a long download in some apps)
//! take an idle inhibitor from `gnome-session`, which shows up as bit 8 of `org.gnome.SessionManager`'s
//! `InhibitedActions`. `droidc host-link` watches that property and sends `keep-awake` lines, and the
//! host keeps the screen on while the desktop is showing.
//!
//! In the other direction, Android locks and blanks the device itself, so a GNOME lock screen and idle
//! blank on top of it would only ask for a second password. [`android_locks_settings`] is what the
//! host's `screen-lock` line writes to turn GNOME's off.

use crate::error::{Error, Result};

pub const SESSION_MANAGER: &str = "org.gnome.SessionManager";
pub const SESSION_MANAGER_PATH: &str = "/org/gnome/SessionManager";
/// `GSM_INHIBITOR_FLAG_IDLE`.
pub const INHIBIT_IDLE: u32 = 8;

/// Whether these `InhibitedActions` should keep the screen on.
pub fn keeps_awake(inhibited: u32) -> bool {
    inhibited & INHIBIT_IDLE != 0
}

/// GSettings that leave locking and blanking to Android.
pub fn android_locks_settings() -> Vec<(&'static str, &'static str, String)> {
    vec![
        (
            "org.gnome.desktop.screensaver",
            "lock-enabled",
            "false".into(),
        ),
        ("org.gnome.desktop.session", "idle-delay", "uint32 0".into()),
        (
            "org.gnome.settings-daemon.plugins.power",
            "sleep-inactive-ac-type",
            "'nothing'".into(),
        ),
        (
            "org.gnome.settings-daemon.plugins.power",
            "sleep-inactive-battery-type",
            "'nothing'".into(),
        ),
    ]
}

fn inhibited(conn: &zbus::blocking::Connection) -> Option<u32> {
    let reply = conn
        .call_method(
            Some(SESSION_MANAGER),
            SESSION_MANAGER_PATH,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(SESSION_MANAGER, "InhibitedActions"),
        )
        .ok()?;
    let v: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
    u32::try_from(v).ok()
}

/// Calls `changed` each time whether the screen should stay on changes, starting from off (so nothing
/// is sent while no app inhibits idle), until the connection closes. Runs on threads of its own; returns once they are started.
pub fn watch(
    conn: zbus::blocking::Connection,
    changed: impl Fn(bool) + Send + Sync + 'static,
) -> Result<()> {
    use std::sync::{Arc, Mutex};
    let ctx = |e: zbus::Error| Error::internal(format!("session bus: {e}"));
    let props = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")
        .map_err(ctx)?
        .member("PropertiesChanged")
        .map_err(ctx)?
        .path(SESSION_MANAGER_PATH)
        .map_err(ctx)?
        .build();
    let owner = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .map_err(ctx)?
        .interface("org.freedesktop.DBus")
        .map_err(ctx)?
        .member("NameOwnerChanged")
        .map_err(ctx)?
        .arg(0, SESSION_MANAGER)
        .map_err(ctx)?
        .build();
    let props =
        zbus::blocking::MessageIterator::for_match_rule(props, &conn, Some(16)).map_err(ctx)?;
    let owner =
        zbus::blocking::MessageIterator::for_match_rule(owner, &conn, Some(16)).map_err(ctx)?;

    // Reports only changes; a session manager that goes away lets the screen sleep again.
    let last: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let changed = Arc::new(changed);
    let check = {
        let conn = conn.clone();
        move || {
            let now = inhibited(&conn).is_some_and(keeps_awake);
            let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
            if *l != now {
                *l = now;
                changed(now);
            }
        }
    };
    let check = Arc::new(check);
    check();
    for (name, it) in [("props", props), ("owner", owner)] {
        let check = check.clone();
        std::thread::Builder::new()
            .name(format!("awake-{name}"))
            .spawn(move || {
                for m in it {
                    if m.is_err() {
                        break;
                    }
                    check();
                }
            })
            .map_err(|e| Error::internal(format!("thread: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn only_idle_inhibitors_keep_the_screen_on() {
        assert!(!keeps_awake(0));
        assert!(!keeps_awake(1 | 2 | 4)); // logout, switch user, suspend
        assert!(keeps_awake(8));
        assert!(keeps_awake(4 | 8));
    }

    #[test]
    fn android_locks_turns_off_gnome_lock_and_blank() {
        let s = android_locks_settings();
        assert!(s.contains(&(
            "org.gnome.desktop.screensaver",
            "lock-enabled",
            "false".into()
        )));
        assert!(s.contains(&("org.gnome.desktop.session", "idle-delay", "uint32 0".into())));
    }

    struct FakeSm {
        inhibited: u32,
    }

    #[zbus::interface(name = "org.gnome.SessionManager")]
    impl FakeSm {
        #[zbus(property)]
        fn inhibited_actions(&self) -> u32 {
            self.inhibited
        }
    }

    /// Against a private bus: a fake gnome-session's idle inhibitor turns keep-awake on and off.
    #[test]
    fn follows_the_session_managers_idle_inhibitor() {
        let Ok(mut daemon) = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            eprintln!("skipped: no dbus-daemon");
            return;
        };
        let mut addr = String::new();
        std::io::BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut addr)
            .unwrap();
        let addr = addr.trim().to_string();

        let sm = zbus::blocking::connection::Builder::address(addr.as_str())
            .unwrap()
            .serve_at(SESSION_MANAGER_PATH, FakeSm { inhibited: 0 })
            .unwrap()
            .name(SESSION_MANAGER)
            .unwrap()
            .build()
            .unwrap();
        let client = zbus::blocking::connection::Builder::address(addr.as_str())
            .unwrap()
            .build()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        watch(client, move |on| {
            let _ = tx.lock().unwrap().send(on);
        })
        .unwrap();
        let next = || rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let set = |v: u32| {
            let iface = sm
                .object_server()
                .interface::<_, FakeSm>(SESSION_MANAGER_PATH)
                .unwrap();
            iface.get_mut().inhibited = v;
            zbus::block_on(
                iface
                    .get()
                    .inhibited_actions_changed(iface.signal_emitter()),
            )
            .unwrap();
        };
        set(4); // suspend only: still off, so nothing is sent
        set(8 | 4);
        assert!(next(), "idle inhibited, and nothing before it");
        set(0);
        assert!(!next(), "released");
        set(8);
        assert!(next());
        // gnome-session goes away: the screen may sleep again.
        drop(sm);
        assert!(!next(), "session manager gone");

        let _ = daemon.kill();
        let _ = daemon.wait();
    }
}

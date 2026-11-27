//! `droidc host-link`'s notification mirror against a real session bus: a fake GNOME Shell owns both
//! notification services, a client posts through each, and the link reports them on stdout; taps and
//! dismissals from the host reach the app and the shell. Needs `dbus-daemon`; skipped without it.

#![cfg(unix)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::zvariant::{OwnedValue, Value};

fn have(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

type Log = Arc<Mutex<Vec<String>>>;

struct Fdo {
    log: Log,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Fdo {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        _app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        _body: String,
        _actions: Vec<String>,
        _hints: HashMap<String, OwnedValue>,
        _timeout: i32,
    ) -> u32 {
        self.log.lock().unwrap().push(format!("notify {summary}"));
        if replaces_id != 0 {
            replaces_id
        } else {
            7
        }
    }

    fn close_notification(&self, id: u32) {
        self.log.lock().unwrap().push(format!("close {id}"));
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
}

struct Gtk {
    log: Log,
}

#[zbus::interface(name = "org.gtk.Notifications")]
impl Gtk {
    fn add_notification(&self, app_id: String, id: String, _n: HashMap<String, OwnedValue>) {
        self.log.lock().unwrap().push(format!("add {app_id} {id}"));
    }

    fn remove_notification(&self, app_id: String, id: String) {
        self.log
            .lock()
            .unwrap()
            .push(format!("remove {app_id} {id}"));
    }
}

struct App {
    log: Log,
}

#[zbus::interface(name = "org.freedesktop.Application")]
impl App {
    fn activate(&self, _platform: HashMap<String, OwnedValue>) {
        self.log.lock().unwrap().push("activate".into());
    }

    fn activate_action(
        &self,
        name: String,
        params: Vec<OwnedValue>,
        _platform: HashMap<String, OwnedValue>,
    ) {
        let params: Vec<String> = params
            .iter()
            .map(|p| match &**p {
                Value::Str(s) => s.to_string(),
                Value::Value(v) => format!("{v}"),
                v => format!("{v}"),
            })
            .collect();
        self.log
            .lock()
            .unwrap()
            .push(format!("action {name} {}", params.join(",")));
    }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Reads stdout lines until one satisfies `want`, or fails after a while.
fn wait_for(
    lines: &mpsc::Receiver<serde_json::Value>,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let v = lines
            .recv_timeout(left)
            .expect("no matching line from host-link");
        if want(&v) {
            return v;
        }
    }
}

#[test]
fn guest_notifications_reach_the_host_and_its_taps_come_back() {
    if !have("dbus-daemon") {
        eprintln!("skipped: no dbus-daemon");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let addr = format!("unix:path={}", dir.path().join("bus").display());
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--nopidfile"])
        .arg(format!("--address={addr}"))
        .spawn()
        .unwrap();
    wait_until("dbus-daemon", || dir.path().join("bus").exists());

    let shell_log: Log = Arc::default();
    let shell = zbus::blocking::connection::Builder::address(addr.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .name("org.gtk.Notifications")
        .unwrap()
        .serve_at(
            "/org/freedesktop/Notifications",
            Fdo {
                log: shell_log.clone(),
            },
        )
        .unwrap()
        .serve_at(
            "/org/gtk/Notifications",
            Gtk {
                log: shell_log.clone(),
            },
        )
        .unwrap()
        .build()
        .unwrap();
    let app_log: Log = Arc::default();
    let _app = zbus::blocking::connection::Builder::address(addr.as_str())
        .unwrap()
        .name("org.example.Chat-App")
        .unwrap()
        .serve_at(
            "/org/example/Chat_App",
            App {
                log: app_log.clone(),
            },
        )
        .unwrap()
        .build()
        .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_droidc"))
        .arg("host-link")
        .env("DBUS_SESSION_BUS_ADDRESS", &addr)
        .env("DROIDC_GSETTINGS", "true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, lines) = mpsc::channel();
    let out = BufReader::new(child.stdout.take().unwrap());
    std::thread::spawn(move || {
        for l in out.lines() {
            let Ok(l) = l else { break };
            let _ = tx.send(serde_json::from_str::<serde_json::Value>(&l).unwrap());
        }
    });
    let stderr = Arc::new(Mutex::new(String::new()));
    let err = BufReader::new(child.stderr.take().unwrap());
    let e = stderr.clone();
    std::thread::spawn(move || {
        for l in err.lines() {
            let Ok(l) = l else { break };
            e.lock().unwrap().push_str(&(l + "\n"));
        }
    });
    wait_for(&lines, |v| v["kind"] == "ready");
    wait_until("the monitor", || {
        stderr.lock().unwrap().contains("watching the session bus")
    });

    let client = zbus::blocking::connection::Builder::address(addr.as_str())
        .unwrap()
        .build()
        .unwrap();

    // libnotify's way.
    let mut hints: HashMap<&str, Value> = HashMap::new();
    hints.insert("urgency", Value::from(2u8));
    hints.insert("desktop-entry", Value::from("org.gnome.Evolution"));
    let reply = client
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Evolution",
                0u32,
                "mail-unread",
                "New mail",
                "From Ada",
                vec!["default", "Open"],
                hints,
                -1i32,
            ),
        )
        .unwrap();
    let (id,): (u32,) = reply.body().deserialize().unwrap();
    assert_eq!(id, 7);
    let n = wait_for(&lines, |v| v["kind"] == "notify");
    assert_eq!(
        n,
        serde_json::json!({
            "kind": "notify", "key": "fdo:7", "app_id": "org.gnome.Evolution",
            "app_name": "Evolution", "title": "New mail", "body": "From Ada",
            "icon": {"names": ["mail-unread"]}, "priority": "urgent",
        })
    );

    // GNotification's way, with a reply button.
    let mut button: HashMap<&str, Value> = HashMap::new();
    button.insert("label", Value::from("Reply"));
    button.insert("action", Value::from("app.reply"));
    button.insert("target", Value::from("room-1"));
    button.insert("purpose", Value::from("im.reply-with-text"));
    let mut note: HashMap<&str, Value> = HashMap::new();
    note.insert("title", Value::from("Ada"));
    note.insert("body", Value::from("Lunch?"));
    note.insert("buttons", Value::from(vec![button]));
    client
        .call_method(
            Some("org.gtk.Notifications"),
            "/org/gtk/Notifications",
            Some("org.gtk.Notifications"),
            "AddNotification",
            &("org.example.Chat-App", "m1", note),
        )
        .unwrap();
    let n = wait_for(&lines, |v| v["kind"] == "notify");
    assert_eq!(n["key"], "gtk:org.example.Chat-App/m1");
    assert_eq!(
        n["actions"],
        serde_json::json!([{"id": "0", "label": "Reply", "reply": true}])
    );

    // The host: a reply, a tap on the notification itself, then dismissing the libnotify one.
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"kind":"notification-action","key":"gtk:org.example.Chat-App/m1","action":"0","text":"Sure"}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"kind":"notification-action","key":"gtk:org.example.Chat-App/m1"}}"#
    )
    .unwrap();
    writeln!(stdin, r#"{{"kind":"notification-closed","key":"fdo:7"}}"#).unwrap();
    stdin.flush().unwrap();
    wait_until("the app's actions", || app_log.lock().unwrap().len() >= 2);
    assert_eq!(
        *app_log.lock().unwrap(),
        vec![
            "action reply room-1,Sure".to_string(),
            "activate".to_string()
        ]
    );
    wait_until("the shell to close it", || {
        shell_log.lock().unwrap().iter().any(|l| l == "close 7")
    });

    // The app withdraws its own notification: the host hears of it.
    client
        .call_method(
            Some("org.gtk.Notifications"),
            "/org/gtk/Notifications",
            Some("org.gtk.Notifications"),
            "RemoveNotification",
            &("org.example.Chat-App", "m1"),
        )
        .unwrap();
    let c = wait_for(&lines, |v| v["kind"] == "close-notification");
    assert_eq!(c["key"], "gtk:org.example.Chat-App/m1");

    // The shell closing a notification itself (it expired) reaches the host too.
    client
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "",
                0u32,
                "",
                "Second",
                "",
                Vec::<&str>::new(),
                HashMap::<&str, Value>::new(),
                -1i32,
            ),
        )
        .unwrap();
    wait_for(&lines, |v| v["kind"] == "notify" && v["title"] == "Second");
    let emitter =
        zbus::object_server::SignalEmitter::new(shell.inner(), "/org/freedesktop/Notifications")
            .unwrap();
    zbus::block_on(Fdo::notification_closed(&emitter, 7, 2)).unwrap();
    let c = wait_for(&lines, |v| v["kind"] == "close-notification");
    assert_eq!(c["key"], "fdo:7");

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "{}", stderr.lock().unwrap());
    let _ = daemon.kill();
    let _ = daemon.wait();
}

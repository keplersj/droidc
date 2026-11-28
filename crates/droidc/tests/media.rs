//! `droidc media` against a real session bus: a fake MPRIS player shows up on stdout with its
//! track, and a `play-pause` on stdin reaches it. Needs `dbus-daemon`; skipped without it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

fn have(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

struct Root;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    #[zbus(property)]
    fn identity(&self) -> String {
        "Test Player".into()
    }
}

struct Player {
    status: Arc<Mutex<String>>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn play_pause(&self) {
        let mut s = self.status.lock().unwrap();
        *s = if *s == "Playing" { "Paused" } else { "Playing" }.into();
    }
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.status.lock().unwrap().clone()
    }
    #[zbus(property)]
    fn metadata(&self) -> std::collections::HashMap<String, OwnedValue> {
        let mut m = std::collections::HashMap::new();
        m.insert(
            "xesam:title".into(),
            Value::from("Test tone").try_to_owned().unwrap(),
        );
        m.insert(
            "xesam:artist".into(),
            Value::from(vec!["droidc"]).try_to_owned().unwrap(),
        );
        m.insert(
            "mpris:trackid".into(),
            Value::from(ObjectPath::try_from("/test/track/1").unwrap())
                .try_to_owned()
                .unwrap(),
        );
        m.insert(
            "mpris:length".into(),
            Value::from(60_000_000i64).try_to_owned().unwrap(),
        );
        m
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }
}

/// Reads stdout lines until one satisfies `want`, or fails after a while.
fn wait_for(
    lines: &mut impl Iterator<Item = std::io::Result<String>>,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    for line in lines {
        let line = line.unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if want(&v) {
            return v;
        }
        assert!(Instant::now() < deadline, "gave up; last line: {line}");
    }
    panic!("droidc media ended early");
}

#[test]
fn a_player_on_the_bus_is_listed_and_takes_commands() {
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
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.path().join("bus").exists() {
        assert!(Instant::now() < deadline, "dbus-daemon did not start");
        std::thread::sleep(Duration::from_millis(50));
    }

    let status = Arc::new(Mutex::new("Paused".to_string()));
    let _player = zbus::blocking::connection::Builder::address(addr.as_str())
        .unwrap()
        .name("org.mpris.MediaPlayer2.droidctest")
        .unwrap()
        .serve_at("/org/mpris/MediaPlayer2", Root)
        .unwrap()
        .serve_at(
            "/org/mpris/MediaPlayer2",
            Player {
                status: status.clone(),
            },
        )
        .unwrap()
        .build()
        .unwrap();

    let mut media = Command::new(env!("CARGO_BIN_EXE_droidc"))
        .args(["media", "--timeout", "10", "--poll", "200"])
        .env("DBUS_SESSION_BUS_ADDRESS", &addr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(media.stdout.take().unwrap()).lines();

    let snap = wait_for(&mut lines, |v| {
        v["players"].as_array().is_some_and(|p| !p.is_empty())
    });
    assert_eq!(snap["active"], "droidctest");
    let p = &snap["players"][0];
    assert_eq!(p["identity"], "Test Player");
    assert_eq!(p["title"], "Test tone");
    assert_eq!(p["artist"], "droidc");
    assert_eq!(p["status"], "Paused");
    assert_eq!(p["length_us"], 60_000_000);

    let mut stdin = media.stdin.take().unwrap();
    writeln!(stdin, "play-pause droidctest").unwrap();
    wait_for(&mut lines, |v| v["players"][0]["status"] == "Playing");
    assert_eq!(*status.lock().unwrap(), "Playing");

    drop(stdin);
    drop(lines);
    let _ = media.kill();
    let _ = media.wait();
    let _ = daemon.kill();
    let _ = daemon.wait();
}

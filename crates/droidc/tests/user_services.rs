//! The container's user services (`usersvc.rs`) across real processes: supervisors share one bus, it
//! outlives the supervisor that started it, it is started again when it dies, and it stops once the
//! last supervisor has gone. Needs `dbus-daemon`; skipped without it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn have(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

fn droidc(run: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_droidc"));
    c.env("XDG_RUNTIME_DIR", run)
        .env("DROIDC_USER_LINGER", "1")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("DROIDC_USER_SERVICES")
        .env_remove("DROIDC_USER_SERVICES_WAIT")
        .stdin(Stdio::null());
    c
}

fn supervise(run: &Path, script: &str) -> Child {
    droidc(run)
        .args(["supervise", "--", "sh", "-c", script])
        .spawn()
        .unwrap()
}

fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn state(run: &Path) -> Option<serde_json::Value> {
    std::fs::read(run.join("droidc/state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn bus_answers(run: &Path) -> bool {
    Command::new("dbus-send")
        .args([
            &format!("--bus=unix:path={}", run.join("bus").display()),
            "--print-reply",
            "--dest=org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.GetId",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn supervisors_share_one_bus_that_outlives_its_starter() {
    if !have("dbus-daemon") || !have("dbus-send") {
        eprintln!("skipped: needs dbus-daemon and dbus-send");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let run: PathBuf = d.path().join("run");
    let out = |n: &str| d.path().join(n);

    // the first supervisor starts the keeper; it ends before the second one does
    let mut a = supervise(
        &run,
        &format!(
            "echo \"$DBUS_SESSION_BUS_ADDRESS\" > {}; sleep 1",
            out("a").display()
        ),
    );
    let mut b = supervise(
        &run,
        &format!(
            "echo \"$DBUS_SESSION_BUS_ADDRESS\" > {}; sleep 4",
            out("b").display()
        ),
    );
    assert!(a.wait().unwrap().success());
    let keeper = state(&run).expect("the keeper published its state")["pid"]
        .as_u64()
        .unwrap();
    let addr = std::fs::read_to_string(out("a")).unwrap();
    assert_eq!(
        addr.trim(),
        format!("unix:path={}", run.join("bus").display())
    );
    // the bus outlives the supervisor that started it while another one runs
    std::thread::sleep(Duration::from_millis(1500));
    assert!(bus_answers(&run), "the bus went with its first supervisor");
    assert_eq!(std::fs::read_to_string(out("b")).unwrap(), addr);

    // a bus that dies is started again at the same address
    let daemon = Command::new("pgrep")
        .args(["-P", &keeper.to_string(), "dbus-daemon"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .expect("the keeper runs dbus-daemon");
    Command::new("kill").arg(&daemon).status().unwrap();
    wait_until("the bus to go", Duration::from_secs(5), || {
        !bus_answers(&run)
    });
    wait_until("the bus to come back", Duration::from_secs(10), || {
        bus_answers(&run)
    });
    assert_eq!(
        state(&run).unwrap()["pid"].as_u64().unwrap(),
        keeper,
        "same keeper"
    );

    // after the last supervisor, the keeper lingers, then stops and takes the bus with it
    assert!(b.wait().unwrap().success());
    wait_until("the keeper to stop", Duration::from_secs(15), || {
        state(&run).is_none() && !bus_answers(&run)
    });

    // and the next supervisor starts a new one
    let mut c = supervise(&run, "test -n \"$DBUS_SESSION_BUS_ADDRESS\"");
    assert!(c.wait().unwrap().success());
}

#[test]
fn an_inherited_bus_that_answers_is_kept() {
    let d = tempfile::tempdir().unwrap();
    let run = d.path().join("run");
    let sock = d.path().join("other-bus");
    let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let addr = format!("unix:path={}", sock.display());
    let st = droidc(&run)
        .env("DBUS_SESSION_BUS_ADDRESS", &addr)
        .args([
            "supervise",
            "--",
            "sh",
            "-c",
            &format!("test \"$DBUS_SESSION_BUS_ADDRESS\" = '{addr}'"),
        ])
        .status()
        .unwrap();
    assert!(st.success());
    assert!(
        !run.join("droidc/state.json").exists(),
        "no user services for it"
    );
}

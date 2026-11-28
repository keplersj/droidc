//! Small CLI behaviours (droidc #9).
use std::process::Command;

fn droidc() -> Command {
    Command::new(env!("CARGO_BIN_EXE_droidc"))
}

#[test]
fn version_starts_with_the_package_version() {
    let out = droidc().arg("--version").output().unwrap();
    assert!(out.status.success());
    let v = String::from_utf8(out.stdout).unwrap();
    let want = format!("droidc {}", env!("CARGO_PKG_VERSION"));
    assert!(v.starts_with(&want), "{v:?}");
    // With a git checkout (or DROIDC_GIT_REV) the revision follows in parentheses.
    if let Some(rev) = droidc::git_rev() {
        assert_eq!(v.trim(), format!("{want} ({rev})"));
    }
}

#[test]
fn status_reports_the_version() {
    let d = tempfile::tempdir().unwrap();
    let out = droidc()
        .arg("--root")
        .arg(d.path())
        .args(["--json", "status"])
        .output()
        .unwrap();
    let last: serde_json::Value = serde_json::from_str(
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(last["ok"], true);
    assert_eq!(last["data"]["version"], droidc::VERSION);
    assert_eq!(last["data"]["git_rev"].as_str(), droidc::git_rev());
}

#[test]
fn session_without_a_subcommand_shows() {
    let d = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = droidc()
            .arg("--root")
            .arg(d.path())
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let s = String::from_utf8(out.stdout).unwrap();
        serde_json::from_str::<serde_json::Value>(s.lines().last().unwrap()).unwrap()
    };
    let bare = run(&["session"]);
    assert_eq!(bare["ok"], true);
    assert_eq!(bare["data"], run(&["session", "show"])["data"]);
}

#[test]
fn session_list_has_the_override_and_its_devices() {
    let d = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = droidc()
            .arg("--root")
            .arg(d.path())
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let s = String::from_utf8(out.stdout).unwrap();
        serde_json::from_str::<serde_json::Value>(s.lines().last().unwrap()).unwrap()
    };
    let empty = run(&["session", "list"]);
    assert_eq!(empty["data"]["sessions"], serde_json::json!([]));
    assert_eq!(empty["data"]["default"], serde_json::Value::Null);
    let set = run(&[
        "session",
        "set",
        "--renderer",
        "wayland",
        "--exec",
        r#"["phoc","-E","phosh-session"]"#,
        "--name",
        "Phosh",
        "--form-factors",
        "Phone;tablet",
    ]);
    assert_eq!(set["data"]["id"], "override");
    let list = run(&["session", "list"]);
    assert_eq!(list["data"]["default"], "override");
    let s = &list["data"]["sessions"][0];
    assert_eq!(s["name"], "Phosh");
    assert_eq!(s["runnable"], true);
    assert_eq!(s["form_factors"], serde_json::json!(["phone", "tablet"]));
}

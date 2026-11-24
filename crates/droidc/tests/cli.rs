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

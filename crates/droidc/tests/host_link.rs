//! `droidc host-link` as the host runs it: a `ready` line on stdout, then an `appearance` line on
//! stdin becomes GSettings writes (through a fake `gsettings`), and closing stdin ends it cleanly.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn appearance_from_the_host_reaches_gsettings() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls");
    let fake = dir.path().join("gsettings");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$*\" >> {}\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_droidc"))
        .arg("host-link")
        .env("DROIDC_GSETTINGS", &fake)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    out.read_line(&mut first).unwrap();
    let ready: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(ready["kind"], "ready", "{first}");

    {
        let mut stdin = child.stdin.take().unwrap();
        // Something a newer host might send, then the real thing twice (the second changes nothing).
        writeln!(stdin, r#"{{"kind":"from-the-future","x":1}}"#).unwrap();
        let a = r##"{"kind":"appearance","dark":true,"accent":"#6750A4","text_scale":1.3}"##;
        writeln!(stdin, "{a}").unwrap();
        writeln!(stdin, "{a}").unwrap();
        writeln!(stdin, r#"{{"kind":"appearance","dark":false}}"#).unwrap();
    }
    let status = child.wait().unwrap();
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    assert!(status.success(), "{err}");
    assert!(err.contains("skipped a line"), "{err}");

    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        vec![
            "set org.gnome.desktop.interface color-scheme 'prefer-dark'",
            "set org.gnome.desktop.interface accent-color 'purple'",
            "set org.gnome.desktop.interface text-scaling-factor 1.30",
            "set org.gnome.desktop.interface color-scheme 'default'",
        ]
    );
}

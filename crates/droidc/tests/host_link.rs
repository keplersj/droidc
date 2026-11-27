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

/// Reads host-link's next line to the host.
fn next_json(out: &mut impl BufRead) -> serde_json::Value {
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
}

#[test]
fn droidc_adb_pair_gets_the_code_from_the_host_and_pairs() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("link.sock");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = dir.path().join("adb-calls");
    // A fake adb: logs its args, stdin and server socket, and answers like the real one.
    std::fs::write(
        bin.join("adb"),
        format!(
            "#!/bin/sh\nin=$(cat)\necho \"$* | $in | $ADB_SERVER_SOCKET\" >> {}\n\
             case \"$1\" in pair) echo \"Enter pairing code: Successfully paired to $2 [guid=x]\";;\n\
             connect) echo \"connected to $2\";; esac\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(bin.join("adb"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut link = Command::new(env!("CARGO_BIN_EXE_droidc"))
        .arg("host-link")
        .env("DROIDC_HOST_LINK_SOCKET", &socket)
        .env("DROIDC_GSETTINGS", "/bin/true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(link.stdout.take().unwrap());
    let mut to_link = link.stdin.take().unwrap();
    assert_eq!(next_json(&mut out)["kind"], "ready");

    // Anything but the guest commands' own kinds is refused at the socket.
    {
        let mut s = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        writeln!(s, r#"{{"kind":"appearance","dark":true}}"#).unwrap();
        let mut reply = String::new();
        BufReader::new(&s).read_line(&mut reply).unwrap();
        let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["ok"], false, "{reply}");
    }

    let pair = Command::new(env!("CARGO_BIN_EXE_droidc"))
        .args(["adb", "pair"])
        .env("DROIDC_HOST_LINK_SOCKET", &socket)
        .env("HOME", dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env_remove("ADB_SERVER_SOCKET")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let req = next_json(&mut out);
    assert_eq!(req["kind"], "adb-pair", "{req}");
    let id = req["id"].as_u64().unwrap();
    writeln!(
        to_link,
        r#"{{"kind":"progress","id":{id},"message":"Type the code in Selkie's notification."}}"#
    )
    .unwrap();
    writeln!(
        to_link,
        r#"{{"kind":"reply","id":{id},"ok":true,"port":37215,"code":"123 456"}}"#
    )
    .unwrap();

    let req = next_json(&mut out);
    assert_eq!(req["kind"], "adb-port", "{req}");
    let id = req["id"].as_u64().unwrap();
    writeln!(
        to_link,
        r#"{{"kind":"reply","id":{id},"ok":true,"port":41000}}"#
    )
    .unwrap();

    let done = pair.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&done.stdout);
    let stderr = String::from_utf8_lossy(&done.stderr);
    assert!(done.status.success(), "{stdout}{stderr}");
    assert!(
        stderr.contains("Type the code in Selkie's notification."),
        "{stderr}"
    );
    assert!(
        stdout.contains("Connected to this device at 127.0.0.1:41000"),
        "{stdout}"
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        vec![
            "pair 127.0.0.1:37215 | 123456 | localfilesystem:/run/droidc-adb.sock",
            "connect 127.0.0.1:41000 |  | localfilesystem:/run/droidc-adb.sock",
        ]
    );

    drop(to_link);
    assert!(link.wait().unwrap().success());
    assert!(!socket.exists(), "the link removes its socket when it ends");
}

#[test]
fn droidc_adb_says_why_without_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_droidc"))
        .args(["adb", "pair"])
        .env("DROIDC_HOST_LINK_SOCKET", dir.path().join("none.sock"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("can't reach Selkie"), "{err}");
}

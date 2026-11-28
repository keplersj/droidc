//! `droidc adb`: the container's own `adb`, already connected to the Android device it runs on.
//!
//! `droidc adb pair` pairs it with the device's Wireless debugging the way Shizuku does: through
//! [`crate::host_link`] it asks the host (Selkie) to open Wireless debugging and wait for the user to tap
//! "Pair device with pairing code"; the host finds that pairing service by mDNS and takes the six-digit
//! code in a notification, so Settings can stay on screen. The code comes back here and this adb pairs
//! (`adb pair 127.0.0.1:<port>`, code on stdin), so the key stays in the guest's `~/.android` and the
//! host never holds one.
//!
//! Any other `droidc adb ARGS` asks the host for Wireless debugging's current port, runs
//! `adb connect 127.0.0.1:<port>` (the pairing lasts, the port changes each time it turns on), then runs
//! `adb ARGS`. With no host link or Wireless debugging off it runs `adb ARGS` as it is.
//!
//! The adb server listens on a Unix socket in the container's `/run` ([`SERVER_SOCKET_SPEC`]) unless
//! `ADB_SERVER_SOCKET` says otherwise, never on TCP 5037: every Android app shares localhost, and an adb
//! server there would hand this device's shell to any of them.

use crate::error::{Error, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Wireless debugging listens on every address; the device reaches itself on loopback.
pub const HOST: &str = "127.0.0.1";
/// Where the server listens unless `ADB_SERVER_SOCKET` is set.
pub const SERVER_SOCKET_SPEC: &str = "localfilesystem:/run/droidc-adb.sock";
/// How long `pair` waits for the user to open the dialog and type the code.
const PAIR_WAIT: Duration = Duration::from_secs(10 * 60);
/// How long a plain command waits for the host to look up the port.
const PORT_WAIT: Duration = Duration::from_secs(8);

/// What `adb pair` printed, read: it exits 0 whether or not it paired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairResult {
    Paired,
    WrongCode,
    Failed(String),
}

pub fn pair_result(output: &str) -> PairResult {
    if output.contains("Successfully paired") {
        PairResult::Paired
    } else if output.contains("Wrong password") {
        PairResult::WrongCode
    } else {
        PairResult::Failed(last_line(output).unwrap_or_else(|| "adb pair printed nothing".into()))
    }
}

/// `adb connect` exits 0 when it fails too.
pub fn connected(output: &str) -> bool {
    output
        .lines()
        .any(|l| l.starts_with("connected to") || l.starts_with("already connected to"))
}

/// The prompt and the reply share a line when the code comes on stdin, so the prompt is cut off.
fn last_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(|l| l.trim().trim_start_matches("Enter pairing code:").trim())
        .rfind(|l| !l.is_empty())
        .map(str::to_string)
}

/// Only the digits: Settings shows the code as six digits.
pub fn clean_code(typed: &str) -> String {
    typed.chars().filter(char::is_ascii_digit).collect()
}

/// One request to the host through the link: `progress` messages go to `on_progress`, and the `reply`
/// is returned. Errors say what the user can do.
pub fn ask_host(
    socket: &str,
    request: &Value,
    wait: Duration,
    mut on_progress: impl FnMut(&str),
) -> Result<Value> {
    let mut s = UnixStream::connect(socket).map_err(|e| {
        Error::internal(format!(
            "can't reach Selkie ({socket}: {e}). Start a terminal or the desktop from Selkie, with a Selkie \
             that has droidc host-link"
        ))
    })?;
    s.set_read_timeout(Some(wait))
        .map_err(|e| Error::internal(e.to_string()))?;
    writeln!(s, "{request}").map_err(|e| Error::internal(format!("{socket}: {e}")))?;
    for line in BufReader::new(&s).lines() {
        let line = line.map_err(|e| match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                Error::internal("gave up waiting for Selkie")
            }
            _ => Error::internal(format!("{socket}: {e}")),
        })?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match v.get("kind").and_then(Value::as_str) {
            Some("progress") => {
                if let Some(m) = v.get("message").and_then(Value::as_str) {
                    on_progress(m);
                }
            }
            Some("reply") => return Ok(v),
            _ => {}
        }
    }
    Err(Error::internal("Selkie closed the link without an answer"))
}

/// A reply's `error`, or a default.
fn reply_error(v: &Value) -> String {
    v.get("error")
        .and_then(Value::as_str)
        .unwrap_or("Selkie said no")
        .to_string()
}

/// `adb` with the server socket set, found on a `PATH` that has `~/.local/bin` and `~/bin` first.
fn adb() -> Command {
    let mut c = Command::new("adb");
    if std::env::var_os("ADB_SERVER_SOCKET").is_none() {
        c.env("ADB_SERVER_SOCKET", SERVER_SOCKET_SPEC);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        let mut dirs = vec![home.join(".local/bin"), home.join("bin")];
        if let Some(p) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&p));
        }
        if let Ok(p) = std::env::join_paths(dirs) {
            c.env("PATH", p);
        }
    }
    c
}

fn no_adb(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::internal(
            "there's no adb in this container. Install it (Android's platform-tools, or your \
             distribution's android-tools), or put it in ~/.local/bin",
        )
    } else {
        Error::internal(format!("adb: {e}"))
    }
}

/// Runs adb with `args`, `stdin` written first, and returns what it printed (stdout and stderr).
fn adb_output(args: &[String], stdin: Option<&str>) -> Result<String> {
    let mut child = adb()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(no_adb)?;
    if let Some(mut w) = child.stdin.take() {
        if let Some(text) = stdin {
            let _ = w.write_all(text.as_bytes());
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| Error::internal(format!("adb: {e}")))?;
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

/// Wireless debugging's port now, from the host, or why there is none.
fn current_port(socket: &str) -> Result<u16> {
    let v = ask_host(socket, &json!({"kind": "adb-port"}), PORT_WAIT, |_| {})?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(Error::internal(reply_error(&v)));
    }
    v.get("port")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| Error::internal("Selkie sent no port"))
}

/// Connects to this device. Returns the port, or why not.
fn connect(socket: &str) -> Result<u16> {
    let port = current_port(socket)?;
    let out = adb_output(&["connect".into(), format!("{HOST}:{port}")], None)?;
    if connected(&out) {
        Ok(port)
    } else {
        Err(Error::internal(
            last_line(&out).unwrap_or_else(|| "adb connect failed".into()),
        ))
    }
}

fn pair(socket: &str) -> Result<i32> {
    eprintln!("Asking Selkie to open Wireless debugging…");
    let v = ask_host(socket, &json!({"kind": "adb-pair"}), PAIR_WAIT, |m| {
        eprintln!("{m}")
    })?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(Error::internal(reply_error(&v)));
    }
    let port = v
        .get("port")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::internal("Selkie sent no pairing port"))?;
    let code = clean_code(v.get("code").and_then(Value::as_str).unwrap_or(""));
    if code.is_empty() {
        return Err(Error::internal("Selkie sent no pairing code"));
    }
    let out = adb_output(
        &["pair".into(), format!("{HOST}:{port}")],
        Some(&format!("{code}\n")),
    )?;
    match pair_result(&out) {
        PairResult::Paired => eprintln!("Paired with this device."),
        PairResult::WrongCode => {
            return Err(Error::internal(
                "the code was wrong or the dialog closed. Run droidc adb pair again",
            ));
        }
        PairResult::Failed(m) => return Err(Error::internal(format!("adb pair: {m}"))),
    }
    match connect(socket) {
        Ok(port) => {
            println!("Connected to this device at {HOST}:{port}.");
            Ok(0)
        }
        Err(e) => {
            eprintln!("Paired, but not connected yet: {e}");
            Ok(0)
        }
    }
}

/// Runs `droidc adb ARGS`. Returns only on error or for `pair`; otherwise it becomes adb.
pub fn run(args: &[String]) -> Result<i32> {
    let socket = crate::host_link::socket_path();
    if args.len() == 1 && args[0] == "pair" {
        return pair(&socket);
    }
    // A failed connect is only worth a word when adb has nothing else to do.
    match connect(&socket) {
        Ok(_) => {}
        Err(e) if args.is_empty() || args[0] == "devices" => {
            eprintln!("droidc adb: not connected to this device: {e}")
        }
        Err(_) => {}
    }
    let e = adb().args(args).exec();
    Err(no_adb(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_output() {
        assert_eq!(
            pair_result(
                "Enter pairing code: Successfully paired to 127.0.0.1:37215 [guid=adb-x]\n"
            ),
            PairResult::Paired
        );
        assert_eq!(
            pair_result("Enter pairing code: Failed: Wrong password or connection was dropped.\n"),
            PairResult::WrongCode
        );
        assert_eq!(
            pair_result("Enter pairing code: Failed: Unable to start pairing client.\n"),
            PairResult::Failed("Failed: Unable to start pairing client.".into())
        );
        assert_eq!(
            pair_result(""),
            PairResult::Failed("adb pair printed nothing".into())
        );
    }

    #[test]
    fn connect_output() {
        assert!(connected("connected to 127.0.0.1:41000\n"));
        assert!(connected("already connected to 127.0.0.1:41000\n"));
        assert!(!connected(
            "failed to connect to '127.0.0.1:41000': Connection refused\n"
        ));
    }

    #[test]
    fn codes_keep_their_digits() {
        assert_eq!(clean_code(" 123 456\n"), "123456");
        assert_eq!(clean_code(""), "");
    }
}

//! `droidc media`: the guest's MPRIS media players, for the host.
//!
//! Runs inside the guest on its session bus (Selkie starts it under `droidc supervise`, which joins
//! the user-services bus). It watches every `org.mpris.MediaPlayer2.*` name and writes one JSON line
//! to stdout each time what a host would show changes: which players exist, which one is active, and
//! each player's title, artist, album, art, length, state and what it can do. Positions ride along
//! but don't trigger a line on their own (the host extrapolates while playing); a seek does.
//!
//! Commands come in on stdin, one per line: `<verb> <player> [argument]`, where `<player>` is the
//! `name` from the JSON (the bus name without `org.mpris.MediaPlayer2.`) and the verb is one of
//! `play`, `pause`, `play-pause`, `stop`, `next`, `previous`, `seek-to <microseconds>` or `raise`.
//! Status and errors go to stderr. Closing stdout ends it.

use crate::error::{Error, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use zbus::zvariant::{OwnedValue, Value};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
/// A player that never answers must not hold the bridge up.
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct Opts {
    /// How long to wait for the session bus.
    pub timeout: Duration,
    /// How often to re-read the players when no signal arrives.
    pub poll: Duration,
}

/// One MPRIS player as the host sees it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Player {
    pub name: String,
    pub identity: String,
    /// `Playing`, `Paused` or `Stopped`.
    pub status: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// As the player gives it: `file://` paths are in the guest's filesystem.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub art_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length_us: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_us: Option<i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub track_id: String,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_seek: bool,
}

/// One line of output.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Snapshot {
    /// The player a host should show: the one playing, else the last one that played, else the
    /// first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    pub players: Vec<Player>,
}

impl Snapshot {
    /// The snapshot with positions dropped: what decides whether a line is worth writing.
    fn shape(&self) -> Snapshot {
        let mut s = self.clone();
        for p in &mut s.players {
            p.position_us = None;
        }
        s
    }
}

fn dbus_err(e: zbus::Error) -> Error {
    Error::internal(format!("D-Bus: {e}"))
}

fn as_str(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.to_string()),
        Value::Value(v) => as_str(v),
        _ => None,
    }
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::I64(n) => Some(*n),
        Value::U64(n) => i64::try_from(*n).ok(),
        Value::I32(n) => Some(i64::from(*n)),
        Value::U32(n) => Some(i64::from(*n)),
        Value::F64(n) => Some(*n as i64),
        Value::Value(v) => as_i64(v),
        _ => None,
    }
}

fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Value(v) => as_bool(v),
        _ => None,
    }
}

/// A string, or a list of strings joined with ", " (`xesam:artist` is a list).
fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().filter_map(as_str).collect();
            Some(parts.join(", "))
        }
        Value::Value(v) => as_text(v),
        v => as_str(v),
    }
}

/// Builds a player from its `org.mpris.MediaPlayer2.Player` properties and its root `Identity`.
pub fn player_from(
    name: &str,
    identity: Option<&str>,
    props: &HashMap<String, OwnedValue>,
) -> Player {
    let b = |k: &str| props.get(k).and_then(|v| as_bool(v)).unwrap_or(false);
    let mut p = Player {
        name: name.to_string(),
        identity: identity.unwrap_or(name).to_string(),
        status: props
            .get("PlaybackStatus")
            .and_then(|v| as_str(v))
            .unwrap_or_else(|| "Stopped".into()),
        position_us: props.get("Position").and_then(|v| as_i64(v)),
        can_play: b("CanPlay"),
        can_pause: b("CanPause"),
        can_next: b("CanGoNext"),
        can_previous: b("CanGoPrevious"),
        can_seek: b("CanSeek"),
        ..Player::default()
    };
    if let Some(Value::Dict(meta)) = props.get("Metadata").map(|v| &**v) {
        let meta: HashMap<String, Value> = meta
            .iter()
            .filter_map(|(k, v)| Some((as_str(k)?, v.try_clone().ok()?)))
            .collect();
        let text = |k: &str| meta.get(k).and_then(as_text).unwrap_or_default();
        p.title = text("xesam:title");
        p.artist = text("xesam:artist");
        p.album = text("xesam:album");
        p.art_url = text("mpris:artUrl");
        p.track_id = text("mpris:trackid");
        p.length_us = meta.get("mpris:length").and_then(as_i64).filter(|n| *n > 0);
    }
    p
}

/// The player to show: playing, else the last one seen playing, else the first.
pub fn pick_active(players: &[Player], last_playing: Option<&str>) -> Option<String> {
    players
        .iter()
        .find(|p| p.status == "Playing")
        .or_else(|| {
            players
                .iter()
                .find(|p| Some(p.name.as_str()) == last_playing)
        })
        .or_else(|| players.first())
        .map(|p| p.name.clone())
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// An argument-less `org.mpris.MediaPlayer2.Player` method.
    Call(&'static str),
    SeekTo(i64),
    Raise,
}

/// Parses one stdin line into the player it names and what to do.
pub fn parse_command(line: &str) -> Result<(String, Command)> {
    let mut words = line.split_whitespace();
    let verb = words
        .next()
        .ok_or_else(|| Error::invalid("empty command"))?;
    let player = words
        .next()
        .ok_or_else(|| Error::invalid(format!("{verb}: which player?")))?
        .to_string();
    let cmd = match verb {
        "play" => Command::Call("Play"),
        "pause" => Command::Call("Pause"),
        "play-pause" => Command::Call("PlayPause"),
        "stop" => Command::Call("Stop"),
        "next" => Command::Call("Next"),
        "previous" => Command::Call("Previous"),
        "raise" => Command::Raise,
        "seek-to" => {
            let us = words
                .next()
                .and_then(|w| w.parse::<i64>().ok())
                .filter(|n| *n >= 0)
                .ok_or_else(|| Error::invalid("seek-to needs a position in microseconds"))?;
            Command::SeekTo(us)
        }
        v => return Err(Error::invalid(format!("unknown command {v}"))),
    };
    if words.next().is_some() {
        return Err(Error::invalid(format!("{verb}: too many arguments")));
    }
    Ok((player, cmd))
}

fn connect(timeout: Duration) -> Result<zbus::blocking::Connection> {
    let deadline = Instant::now() + timeout;
    let mut said = false;
    loop {
        let conn = zbus::blocking::connection::Builder::session()
            .map(|b| b.method_timeout(CALL_TIMEOUT))
            .and_then(|b| b.build());
        match conn {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() < deadline => {
                if !said {
                    eprintln!("media: waiting for the session bus: {e}");
                    said = true;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => return Err(dbus_err(e)),
        }
    }
}

fn get_all(
    conn: &zbus::blocking::Connection,
    bus: &str,
    iface: &str,
) -> Result<HashMap<String, OwnedValue>> {
    conn.call_method(
        Some(bus),
        PATH,
        Some("org.freedesktop.DBus.Properties"),
        "GetAll",
        &(iface,),
    )
    .map_err(dbus_err)?
    .body()
    .deserialize()
    .map_err(dbus_err)
}

/// Reads every player now on the bus. A player that fails to answer is left out.
fn read_players(conn: &zbus::blocking::Connection) -> Result<Vec<Player>> {
    let dbus = zbus::blocking::fdo::DBusProxy::new(conn).map_err(dbus_err)?;
    let mut names: Vec<String> = dbus
        .list_names()
        .map_err(|e| dbus_err(e.into()))?
        .into_iter()
        .map(|n| n.to_string())
        .filter(|n| n.starts_with(PREFIX))
        .collect();
    names.sort();
    let mut players = Vec::new();
    for bus in names {
        let props = match get_all(conn, &bus, PLAYER) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("media: {bus}: {e}");
                continue;
            }
        };
        let identity = get_all(conn, &bus, ROOT)
            .ok()
            .and_then(|r| r.get("Identity").and_then(|v| as_str(v)));
        players.push(player_from(
            &bus[PREFIX.len()..],
            identity.as_deref(),
            &props,
        ));
    }
    Ok(players)
}

fn run_command(conn: &zbus::blocking::Connection, players: &[Player], line: &str) -> Result<()> {
    let (name, cmd) = parse_command(line)?;
    let bus = format!("{PREFIX}{name}");
    match cmd {
        Command::Call(m) => conn.call_method(Some(bus.as_str()), PATH, Some(PLAYER), m, &()),
        Command::Raise => conn.call_method(Some(bus.as_str()), PATH, Some(ROOT), "Raise", &()),
        Command::SeekTo(us) => {
            let track = players
                .iter()
                .find(|p| p.name == name)
                .map(|p| p.track_id.clone())
                .filter(|t| !t.is_empty())
                .ok_or_else(|| Error::not_found(format!("{name} gives no track id to seek in")))?;
            let track = zbus::zvariant::ObjectPath::try_from(track.as_str())
                .map_err(|e| Error::internal(format!("track id {track}: {e}")))?;
            conn.call_method(
                Some(bus.as_str()),
                PATH,
                Some(PLAYER),
                "SetPosition",
                &(track, us),
            )
        }
    }
    .map(|_| ())
    .map_err(dbus_err)
}

/// What wakes the main loop.
enum Event {
    /// Something on the bus changed.
    Changed,
    /// A player seeked: write a line even if nothing else changed.
    Seeked,
    Command(String),
    StdinClosed,
}

fn watch(conn: &zbus::blocking::Connection, tx: &mpsc::Sender<Event>) -> Result<()> {
    let rules = [
        (
            zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.freedesktop.DBus.Properties")
                .and_then(|b| b.member("PropertiesChanged"))
                .and_then(|b| b.path(PATH))
                .map(|b| b.build()),
            false,
        ),
        (
            zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(PLAYER)
                .and_then(|b| b.member("Seeked"))
                .map(|b| b.build()),
            true,
        ),
        (
            zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender("org.freedesktop.DBus")
                .and_then(|b| b.member("NameOwnerChanged"))
                .and_then(|b| b.arg0ns(ROOT))
                .map(|b| b.build()),
            false,
        ),
    ];
    for (rule, seek) in rules {
        let rule = rule.map_err(dbus_err)?;
        let it = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(16))
            .map_err(dbus_err)?;
        let tx = tx.clone();
        std::thread::spawn(move || {
            for m in it {
                if m.is_err() {
                    break;
                }
                let ev = if seek { Event::Seeked } else { Event::Changed };
                if tx.send(ev).is_err() {
                    break;
                }
            }
        });
    }
    Ok(())
}

pub fn run(opts: &Opts) -> Result<i32> {
    let conn = connect(opts.timeout)?;
    let (tx, rx) = mpsc::channel();
    watch(&conn, &tx)?;
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if !line.trim().is_empty() && tx.send(Event::Command(line)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Event::StdinClosed);
        });
    }
    eprintln!("media: watching the session bus for MPRIS players");
    let mut out = std::io::stdout().lock();
    let mut last: Option<Snapshot> = None;
    let mut last_playing: Option<String> = None;
    let mut force = true;
    let mut stdin_open = true;
    loop {
        let players = read_players(&conn)?;
        if let Some(p) = players.iter().find(|p| p.status == "Playing") {
            last_playing = Some(p.name.clone());
        }
        let snap = Snapshot {
            active: pick_active(&players, last_playing.as_deref()),
            players,
        };
        if force || last.as_ref().map(Snapshot::shape) != Some(snap.shape()) {
            let line =
                serde_json::to_string(&snap).map_err(|e| Error::internal(format!("JSON: {e}")))?;
            // The host closing our stdout is how it stops us.
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                return Ok(0);
            }
            last = Some(snap);
        }
        force = false;
        // Signals often come in bursts: read once after the first settles.
        let first = if stdin_open {
            rx.recv_timeout(opts.poll).ok()
        } else {
            std::thread::sleep(opts.poll);
            rx.try_recv().ok()
        };
        let mut events: Vec<Event> = first.into_iter().collect();
        std::thread::sleep(Duration::from_millis(50));
        events.extend(rx.try_iter());
        for ev in events {
            match ev {
                Event::Changed => {}
                Event::Seeked => force = true,
                Event::StdinClosed => stdin_open = false,
                Event::Command(line) => {
                    let players = last.as_ref().map(|s| s.players.as_slice()).unwrap_or(&[]);
                    if let Err(e) = run_command(&conn, players, &line) {
                        eprintln!("media: {}: {e}", line.trim());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Dict, Signature};

    fn ov(v: Value) -> OwnedValue {
        v.try_to_owned().unwrap()
    }

    fn props(status: &str, title: &str, artists: &[&str]) -> HashMap<String, OwnedValue> {
        let mut meta: Dict = Dict::new(&Signature::Str, &Signature::Variant);
        meta.append(
            Value::from("xesam:title"),
            Value::Value(Box::new(Value::from(title))),
        )
        .unwrap();
        let mut a = Array::new(&Signature::Str);
        for s in artists {
            a.append(Value::from(*s)).unwrap();
        }
        meta.append(
            Value::from("xesam:artist"),
            Value::Value(Box::new(Value::Array(a))),
        )
        .unwrap();
        meta.append(
            Value::from("mpris:length"),
            Value::Value(Box::new(Value::from(180_000_000i64))),
        )
        .unwrap();
        meta.append(
            Value::from("mpris:trackid"),
            Value::Value(Box::new(Value::ObjectPath(
                zbus::zvariant::ObjectPath::try_from("/track/1").unwrap(),
            ))),
        )
        .unwrap();
        let mut p = HashMap::new();
        p.insert("PlaybackStatus".into(), ov(Value::from(status)));
        p.insert("Metadata".into(), ov(Value::Dict(meta)));
        p.insert("Position".into(), ov(Value::from(5_000_000i64)));
        p.insert("CanPlay".into(), ov(Value::from(true)));
        p.insert("CanGoNext".into(), ov(Value::from(true)));
        p
    }

    #[test]
    fn a_player_reads_its_track_and_what_it_can_do() {
        let p = player_from(
            "Lollypop",
            Some("Lollypop"),
            &props("Playing", "Hayloft", &["Mother Mother", "Guest"]),
        );
        assert_eq!(p.title, "Hayloft");
        assert_eq!(p.artist, "Mother Mother, Guest");
        assert_eq!(p.status, "Playing");
        assert_eq!(p.length_us, Some(180_000_000));
        assert_eq!(p.position_us, Some(5_000_000));
        assert_eq!(p.track_id, "/track/1");
        assert!(p.can_play && p.can_next && !p.can_pause && !p.can_seek);
    }

    #[test]
    fn a_player_with_no_properties_is_stopped_and_named_by_its_bus_name() {
        let p = player_from("firefox.instance_1_2", None, &HashMap::new());
        assert_eq!(p.status, "Stopped");
        assert_eq!(p.identity, "firefox.instance_1_2");
        assert_eq!(p.title, "");
    }

    #[test]
    fn the_playing_player_is_active_then_the_last_one_that_played() {
        let mk = |n: &str, s: &str| Player {
            name: n.into(),
            status: s.into(),
            ..Player::default()
        };
        let ps = [mk("a", "Paused"), mk("b", "Playing")];
        assert_eq!(pick_active(&ps, Some("a")).as_deref(), Some("b"));
        let ps = [mk("a", "Paused"), mk("b", "Paused")];
        assert_eq!(pick_active(&ps, Some("b")).as_deref(), Some("b"));
        assert_eq!(pick_active(&ps, Some("gone")).as_deref(), Some("a"));
        assert_eq!(pick_active(&[], None), None);
    }

    #[test]
    fn commands_name_a_player_and_a_verb() {
        assert_eq!(
            parse_command("play-pause spotify").unwrap(),
            ("spotify".into(), Command::Call("PlayPause"))
        );
        assert_eq!(
            parse_command("seek-to vlc 1500000").unwrap(),
            ("vlc".into(), Command::SeekTo(1_500_000))
        );
        assert_eq!(parse_command("raise vlc").unwrap().1, Command::Raise);
        assert!(parse_command("play").is_err());
        assert!(parse_command("seek-to vlc -4").is_err());
        assert!(parse_command("dance vlc").is_err());
        assert!(parse_command("next vlc now").is_err());
    }

    #[test]
    fn a_moving_position_alone_does_not_change_the_shape() {
        let mut a = Snapshot {
            active: Some("x".into()),
            players: vec![Player {
                name: "x".into(),
                position_us: Some(1),
                ..Player::default()
            }],
        };
        let mut b = a.clone();
        b.players[0].position_us = Some(2_000_000);
        assert_eq!(a.shape(), b.shape());
        a.players[0].status = "Playing".into();
        assert_ne!(a.shape(), b.shape());
    }
}

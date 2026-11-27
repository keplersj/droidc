//! The guest's notifications, mirrored to Android over `droidc host-link`.
//!
//! GNOME Shell owns both notification services on the session bus, and an app reaches it one of two
//! ways: libnotify and friends call `org.freedesktop.Notifications.Notify`, while GApplication's
//! `GNotification` (and the portal, for Flatpaks) calls `org.gtk.Notifications.AddNotification`.
//! droidc can't replace the shell's server, so it watches the bus as a monitor
//! (`org.freedesktop.DBus.Monitoring.BecomeMonitor`) and reads those calls as they go by: each one
//! becomes a `notify` line for the host, and each withdrawal (`CloseNotification`,
//! `NotificationClosed`, `RemoveNotification`) a `close-notification` line.
//!
//! Going the other way, the host reports taps and dismissals on Android:
//!
//! - **An action** (`notification-action`) on a `GNotification` is run the way the shell would run
//!   it: `org.freedesktop.Application.ActivateAction` on the app's bus name for an `app.` action,
//!   `Activate` otherwise. Text typed into a reply is appended to the action's parameters, as the
//!   notification portal does. `org.freedesktop.Notifications` actions can't be run: only the server
//!   may emit `ActionInvoked`, so those notifications carry no actions and a tap only opens the
//!   desktop.
//! - **A dismissal** (`notification-closed`) withdraws the notification from the shell too.
//!
//! Known gaps: a notification the user closes in GNOME's own list sends nothing on the bus for
//! `GNotification`s, so it stays on Android until the app withdraws it or the user dismisses it there;
//! notifications already showing when the link starts aren't seen; images sent as bytes are dropped.

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use zbus::message::Type as MsgType;
use zbus::zvariant::{OwnedValue, Value};

const FDO: &str = "org.freedesktop.Notifications";
const FDO_PATH: &str = "/org/freedesktop/Notifications";
const GTK: &str = "org.gtk.Notifications";
const GTK_PATH: &str = "/org/gtk/Notifications";
const APP: &str = "org.freedesktop.Application";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Notify calls waiting for their reply (which carries the id). Replies come within milliseconds,
/// so a short queue is plenty; the oldest is dropped if a reply never comes.
const PENDING_MAX: usize = 64;

/// A notification as the host shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Notification {
    /// Stable across updates of the same notification: `fdo:<id>` or `gtk:<app id>/<id>`.
    pub key: String,
    /// The app's ID (its desktop file name without `.desktop`), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// The name the app gave itself (`org.freedesktop.Notifications` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<Icon>,
    /// `low`, `normal`, `high` or `urgent`.
    pub priority: &'static str,
    /// The sender's category (`im.received`, `email.arrived`, …), when it gives one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Buttons the host can offer, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

/// An icon in the guest: theme names to look up, or a file.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Icon {
    /// Icon theme names, best first.
    Names(Vec<String>),
    /// An absolute path in the guest.
    Path(String),
}

/// One button.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Action {
    /// What the host sends back in `notification-action`.
    pub id: String,
    pub label: String,
    /// The button takes typed text (the portal's `im.reply-with-text` purpose).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reply: bool,
}

/// What a `notification-action` or `notification-closed` acts on.
#[derive(Debug, Clone, PartialEq)]
enum Source {
    Fdo(u32),
    Gtk {
        app_id: String,
        id: String,
        default: Option<GAction>,
        buttons: Vec<GAction>,
    },
}

/// A `GNotification` action: its detailed name and target.
#[derive(Debug, Clone, PartialEq)]
struct GAction {
    name: String,
    target: Option<OwnedValue>,
}

/// A change the host should hear about.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Show(Notification),
    Close(String),
}

fn as_str(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.to_string()),
        Value::Value(v) => as_str(v),
        _ => None,
    }
}

fn as_u8(v: &Value) -> Option<u8> {
    match v {
        Value::U8(n) => Some(*n),
        Value::U32(n) => u8::try_from(*n).ok(),
        Value::I32(n) => u8::try_from(*n).ok(),
        Value::Value(v) => as_u8(v),
        _ => None,
    }
}

fn unwrap<'a>(v: &'a Value<'a>) -> &'a Value<'a> {
    match v {
        Value::Value(inner) => unwrap(inner),
        v => v,
    }
}

/// An `app_icon` or image hint: a path or `file://` URI, else an icon name.
fn icon_from_str(s: &str) -> Option<Icon> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(p) = s.strip_prefix("file://") {
        return Some(Icon::Path(p.to_string()));
    }
    if s.starts_with('/') {
        return Some(Icon::Path(s.to_string()));
    }
    Some(Icon::Names(vec![s.to_string()]))
}

/// A serialized `GIcon`: `('themed', <['a', 'b']>)` or `('file', <'file:///…'>)`. Bytes are dropped.
fn icon_from_gicon(v: &Value) -> Option<Icon> {
    match unwrap(v) {
        Value::Structure(s) => {
            let f = s.fields();
            let kind = as_str(f.first()?)?;
            let data = unwrap(f.get(1)?);
            match kind.as_str() {
                "themed" => match data {
                    Value::Array(a) => {
                        let names: Vec<String> = a.iter().filter_map(as_str).collect();
                        (!names.is_empty()).then_some(Icon::Names(names))
                    }
                    v => as_str(v).map(|n| Icon::Names(vec![n])),
                },
                "file" => as_str(data).and_then(|u| icon_from_str(&u)),
                _ => None,
            }
        }
        // A plain string is GIcon's other serialized form.
        v => as_str(v).and_then(|s| icon_from_str(&s)),
    }
}

/// `org.freedesktop.Notifications.Notify`'s arguments, once the reply gave the id.
pub type FdoNotify = (
    String,
    u32,
    String,
    String,
    String,
    Vec<String>,
    HashMap<String, OwnedValue>,
    i32,
);

/// A notification from `Notify`. Its actions are left out: only the server can invoke them.
pub fn from_fdo(id: u32, call: &FdoNotify) -> Notification {
    let (app_name, _replaces, app_icon, summary, body, _actions, hints, _timeout) = call;
    let hint = |k: &str| hints.get(k).map(|v| &**v);
    let priority = match hint("urgency").and_then(as_u8) {
        Some(0) => "low",
        Some(2) => "urgent",
        _ => "normal",
    };
    let icon = ["image-path", "image_path"]
        .iter()
        .find_map(|k| hint(k).and_then(as_str))
        .and_then(|s| icon_from_str(&s))
        .or_else(|| icon_from_str(app_icon));
    Notification {
        key: format!("fdo:{id}"),
        app_id: hint("desktop-entry")
            .and_then(as_str)
            .map(|s| s.trim_end_matches(".desktop").to_string())
            .filter(|s| !s.is_empty()),
        app_name: Some(app_name.clone()).filter(|s| !s.is_empty()),
        title: summary.clone(),
        body: body.clone(),
        icon,
        priority,
        category: hint("category").and_then(as_str).filter(|s| !s.is_empty()),
        actions: Vec::new(),
    }
}

fn gaction(name: Option<String>, target: Option<&Value>) -> Option<GAction> {
    let name = name.filter(|n| !n.is_empty())?;
    Some(GAction {
        name,
        target: target.and_then(|t| unwrap(t).try_to_owned().ok()),
    })
}

/// A notification from `AddNotification`, with what's needed to run its actions later.
fn from_gtk(app_id: &str, id: &str, n: &HashMap<String, OwnedValue>) -> (Notification, Source) {
    let get = |k: &str| n.get(k).map(|v| &**v);
    let priority = match get("priority").and_then(as_str).as_deref() {
        Some("low") => "low",
        Some("high") => "high",
        Some("urgent") => "urgent",
        _ => "normal",
    };
    let default = gaction(
        get("default-action").and_then(as_str),
        get("default-action-target"),
    );
    let mut buttons = Vec::new();
    let mut actions = Vec::new();
    if let Some(Value::Array(list)) = get("buttons").map(unwrap) {
        for b in list.iter() {
            let Value::Dict(d) = unwrap(b) else { continue };
            let field = |k: &str| {
                d.iter()
                    .find(|(name, _)| as_str(name).as_deref() == Some(k))
                    .map(|(_, v)| unwrap(v))
            };
            let label = field("label").and_then(as_str);
            let action = gaction(field("action").and_then(as_str), field("target"));
            let (Some(label), Some(action)) = (label, action) else {
                continue;
            };
            let purpose = field("purpose").and_then(as_str);
            actions.push(Action {
                id: buttons.len().to_string(),
                label,
                reply: purpose.as_deref() == Some("im.reply-with-text"),
            });
            buttons.push(action);
        }
    }
    let note = Notification {
        key: format!("gtk:{app_id}/{id}"),
        app_id: Some(app_id.to_string()).filter(|s| !s.is_empty()),
        app_name: None,
        title: get("title").and_then(as_str).unwrap_or_default(),
        body: get("body").and_then(as_str).unwrap_or_default(),
        icon: get("icon").and_then(icon_from_gicon),
        priority,
        category: get("category").and_then(as_str).filter(|s| !s.is_empty()),
        actions,
    };
    let src = Source::Gtk {
        app_id: app_id.to_string(),
        id: id.to_string(),
        default,
        buttons,
    };
    (note, src)
}

/// The object path GApplication exports for an app ID: `org.gnome.Foo-Bar` is
/// `/org/gnome/Foo_Bar`.
pub fn app_path(app_id: &str) -> String {
    let mut p = String::with_capacity(app_id.len() + 1);
    p.push('/');
    for c in app_id.chars() {
        p.push(match c {
            '.' => '/',
            '-' => '_',
            c => c,
        });
    }
    p
}

/// Reads the bus traffic a monitor sees and turns it into changes for the host.
#[derive(Default)]
struct Tracker {
    /// `(caller, serial)` of `Notify` calls awaiting their reply.
    pending: VecDeque<((String, u32), FdoNotify)>,
    shown: HashMap<String, Source>,
}

impl Tracker {
    fn on_message(&mut self, msg: &zbus::Message) -> Option<Change> {
        let h = msg.header();
        match h.message_type() {
            MsgType::MethodCall => {
                let iface = h.interface()?.to_string();
                let member = h.member()?.to_string();
                let sender = h.sender().map(|s| s.to_string()).unwrap_or_default();
                match (iface.as_str(), member.as_str()) {
                    (FDO, "Notify") => {
                        let call: FdoNotify = msg.body().deserialize().ok()?;
                        if self.pending.len() >= PENDING_MAX {
                            self.pending.pop_front();
                        }
                        let serial = h.primary().serial_num().get();
                        self.pending.push_back(((sender, serial), call));
                        None
                    }
                    (FDO, "CloseNotification") => {
                        let (id,): (u32,) = msg.body().deserialize().ok()?;
                        self.close(format!("fdo:{id}"))
                    }
                    (GTK, "AddNotification") => {
                        let (app_id, id, n): (String, String, HashMap<String, OwnedValue>) =
                            msg.body().deserialize().ok()?;
                        let (note, src) = from_gtk(&app_id, &id, &n);
                        self.shown.insert(note.key.clone(), src);
                        Some(Change::Show(note))
                    }
                    (GTK, "RemoveNotification") => {
                        let (app_id, id): (String, String) = msg.body().deserialize().ok()?;
                        self.close(format!("gtk:{app_id}/{id}"))
                    }
                    _ => None,
                }
            }
            MsgType::MethodReturn => {
                if self.pending.is_empty() {
                    return None;
                }
                let to = h.destination()?.to_string();
                let serial = h.reply_serial()?.get();
                let at = self
                    .pending
                    .iter()
                    .position(|((c, s), _)| *s == serial && *c == to)?;
                let (_, call) = self.pending.remove(at)?;
                let (id,): (u32,) = msg.body().deserialize().ok()?;
                let note = from_fdo(id, &call);
                self.shown.insert(note.key.clone(), Source::Fdo(id));
                Some(Change::Show(note))
            }
            MsgType::Signal => {
                if h.interface()?.as_str() != FDO || h.member()?.as_str() != "NotificationClosed" {
                    return None;
                }
                let (id, _reason): (u32, u32) = msg.body().deserialize().ok()?;
                self.close(format!("fdo:{id}"))
            }
            _ => None,
        }
    }

    /// Forgets a notification; only one the host was told about is worth a line.
    fn close(&mut self, key: String) -> Option<Change> {
        self.shown.remove(&key).map(|_| Change::Close(key))
    }
}

/// What the host's lines act on: shared between the bus thread and the link.
pub struct Mirror {
    tracker: Mutex<Tracker>,
    conn: OnceLock<zbus::blocking::Connection>,
}

impl Mirror {
    fn new() -> Self {
        Mirror {
            tracker: Mutex::new(Tracker::default()),
            conn: OnceLock::new(),
        }
    }

    fn source(&self, key: &str) -> Option<Source> {
        self.tracker.lock().ok()?.shown.get(key).cloned()
    }

    fn conn(&self) -> Result<&zbus::blocking::Connection, String> {
        self.conn
            .get()
            .ok_or_else(|| "not connected to the session bus".to_string())
    }

    /// Runs a button (`action` is its id) or, with none, the notification's default action.
    pub fn activate(
        &self,
        key: &str,
        action: Option<&str>,
        text: Option<&str>,
    ) -> Result<(), String> {
        let src = self
            .source(key)
            .ok_or_else(|| format!("{key}: no such notification"))?;
        let Source::Gtk {
            app_id,
            default,
            buttons,
            ..
        } = src
        else {
            // Only the shell can invoke these; the host opening the desktop is all there is.
            return Ok(());
        };
        let chosen = match action {
            Some(a) => Some(
                a.parse::<usize>()
                    .ok()
                    .and_then(|i| buttons.get(i).cloned())
                    .ok_or_else(|| format!("{key}: no action {a}"))?,
            ),
            None => default,
        };
        let conn = self.conn()?;
        let path = app_path(&app_id);
        let platform: HashMap<&str, Value> = HashMap::new();
        let res = match chosen
            .as_ref()
            .and_then(|a| a.name.strip_prefix("app.").map(|n| (n, a)))
        {
            Some((name, a)) => {
                let mut params: Vec<Value> = Vec::new();
                if let Some(t) = &a.target {
                    params.push(Value::from(t.try_clone().map_err(|e| e.to_string())?));
                }
                if let Some(t) = text {
                    params.push(Value::from(t));
                }
                conn.call_method(
                    Some(app_id.as_str()),
                    path.as_str(),
                    Some(APP),
                    "ActivateAction",
                    &(name, params, platform),
                )
            }
            // No action, or one outside the app's group: bring the app up, as the shell does.
            None => conn.call_method(
                Some(app_id.as_str()),
                path.as_str(),
                Some(APP),
                "Activate",
                &(platform,),
            ),
        };
        res.map(|_| ()).map_err(|e| format!("{app_id}: {e}"))
    }

    /// The user dismissed it on Android: withdraw it from the shell too.
    pub fn dismiss(&self, key: &str) -> Result<(), String> {
        let src = {
            let mut t = self.tracker.lock().map_err(|e| e.to_string())?;
            t.shown.remove(key)
        };
        let Some(src) = src else { return Ok(()) };
        let conn = self.conn()?;
        match src {
            Source::Fdo(id) => {
                conn.call_method(Some(FDO), FDO_PATH, Some(FDO), "CloseNotification", &(id,))
            }
            Source::Gtk { app_id, id, .. } => conn.call_method(
                Some(GTK),
                GTK_PATH,
                Some(GTK),
                "RemoveNotification",
                &(app_id.as_str(), id.as_str()),
            ),
        }
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
}

fn session(timeout: Option<Duration>) -> zbus::Result<zbus::blocking::Connection> {
    let b = zbus::blocking::connection::Builder::session()?;
    match timeout {
        Some(t) => b.method_timeout(t).build(),
        None => b.build(),
    }
}

/// Connects, retrying until the session bus is up (the link may start before it).
fn connect() -> zbus::blocking::Connection {
    let mut said = false;
    loop {
        match session(Some(CALL_TIMEOUT)) {
            Ok(c) => return c,
            Err(e) => {
                if !said {
                    eprintln!("host-link: notifications: waiting for the session bus: {e}");
                    said = true;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

const RULES: [&str; 6] = [
    "type='method_call',interface='org.freedesktop.Notifications',member='Notify'",
    "type='method_call',interface='org.freedesktop.Notifications',member='CloseNotification'",
    "type='signal',interface='org.freedesktop.Notifications',member='NotificationClosed'",
    "type='method_call',interface='org.gtk.Notifications',member='AddNotification'",
    "type='method_call',interface='org.gtk.Notifications',member='RemoveNotification'",
    // Notify's reply carries the id. Monitors can't match on reply serials, so every reply is seen
    // and all but the awaited ones are dropped at once.
    "type='method_return'",
];

fn watch(mirror: &Mirror, out: &Arc<Mutex<dyn Write + Send>>) -> Result<(), String> {
    let conn = connect();
    let _ = mirror.conn.set(conn);
    // A monitor connection can't send anything after this call, so it is a second one.
    let mon = session(None).map_err(|e| e.to_string())?;
    let rules: Vec<&str> = RULES.to_vec();
    mon.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus.Monitoring"),
        "BecomeMonitor",
        &(rules, 0u32),
    )
    .map_err(|e| format!("can't watch the bus: {e}"))?;
    eprintln!("host-link: notifications: watching the session bus");
    for msg in zbus::blocking::MessageIterator::from(mon) {
        let Ok(msg) = msg else { break };
        let change = match mirror.tracker.lock() {
            Ok(mut t) => t.on_message(&msg),
            Err(_) => break,
        };
        let line = match change {
            Some(Change::Show(n)) => serde_json::to_string(&crate::host_link::GuestMsg::Notify(n)),
            Some(Change::Close(key)) => {
                serde_json::to_string(&crate::host_link::GuestMsg::CloseNotification { key })
            }
            None => continue,
        };
        let Ok(line) = line else { continue };
        let Ok(mut o) = out.lock() else { break };
        if writeln!(o, "{line}").and_then(|_| o.flush()).is_err() {
            break;
        }
    }
    Err("the session bus went away".into())
}

/// Starts mirroring in the background. Lines go to `out`, which the link shares.
pub fn start(out: Arc<Mutex<dyn Write + Send>>) -> Arc<Mirror> {
    let mirror = Arc::new(Mirror::new());
    let m = mirror.clone();
    std::thread::spawn(move || {
        if let Err(e) = watch(&m, &out) {
            eprintln!("host-link: notifications: {e}");
        }
    });
    mirror
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov(v: impl Into<Value<'static>>) -> OwnedValue {
        v.into().try_to_owned().unwrap()
    }

    #[test]
    fn app_paths_follow_gapplication() {
        assert_eq!(app_path("org.gnome.Fractal"), "/org/gnome/Fractal");
        assert_eq!(app_path("io.github.foo-bar.App"), "/io/github/foo_bar/App");
    }

    #[test]
    fn libnotify_calls_become_notifications() {
        let mut hints = HashMap::new();
        hints.insert("urgency".to_string(), ov(2u8));
        hints.insert("desktop-entry".to_string(), ov("org.gnome.Evolution"));
        hints.insert("category".to_string(), ov("email.arrived"));
        let call: FdoNotify = (
            "Evolution".into(),
            0,
            "mail-unread".into(),
            "New mail".into(),
            "From Ada".into(),
            vec!["default".into(), "Open".into()],
            hints,
            -1,
        );
        let n = from_fdo(12, &call);
        assert_eq!(n.key, "fdo:12");
        assert_eq!(n.app_id.as_deref(), Some("org.gnome.Evolution"));
        assert_eq!(n.app_name.as_deref(), Some("Evolution"));
        assert_eq!(n.priority, "urgent");
        assert_eq!(n.icon, Some(Icon::Names(vec!["mail-unread".into()])));
        assert_eq!(n.category.as_deref(), Some("email.arrived"));
        assert!(n.actions.is_empty());

        let mut hints = HashMap::new();
        hints.insert("image-path".to_string(), ov("file:///tmp/a.png"));
        let call: FdoNotify = (
            String::new(),
            0,
            String::new(),
            "T".into(),
            String::new(),
            vec![],
            hints,
            -1,
        );
        let n = from_fdo(1, &call);
        assert_eq!(n.icon, Some(Icon::Path("/tmp/a.png".into())));
        assert_eq!(n.app_name, None);
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            serde_json::json!({"key":"fdo:1","title":"T","icon":{"path":"/tmp/a.png"},"priority":"normal"})
        );
    }

    #[test]
    fn gnotifications_keep_their_actions() {
        let mut button = HashMap::new();
        button.insert("label", Value::from("Reply"));
        button.insert("action", Value::from("app.reply"));
        button.insert("target", Value::from("room-1"));
        button.insert("purpose", Value::from("im.reply-with-text"));
        let mut bad = HashMap::new();
        bad.insert("label", Value::from("No action"));
        let mut n = HashMap::new();
        n.insert("title".to_string(), ov("Ada"));
        n.insert("body".to_string(), ov("Lunch?"));
        n.insert("priority".to_string(), ov("high"));
        n.insert(
            "icon".to_string(),
            ov(Value::from((
                "themed",
                Value::from(vec!["avatar-default", "user"]),
            ))),
        );
        n.insert("default-action".to_string(), ov("app.open-room"));
        n.insert(
            "default-action-target".to_string(),
            ov(Value::from("room-1")),
        );
        n.insert("buttons".to_string(), ov(vec![button, bad]));
        let (note, src) = from_gtk("org.gnome.Fractal", "m1", &n);
        assert_eq!(note.key, "gtk:org.gnome.Fractal/m1");
        assert_eq!(note.title, "Ada");
        assert_eq!(note.priority, "high");
        assert_eq!(
            note.icon,
            Some(Icon::Names(vec!["avatar-default".into(), "user".into()]))
        );
        assert_eq!(
            note.actions,
            vec![Action {
                id: "0".into(),
                label: "Reply".into(),
                reply: true
            }]
        );
        let Source::Gtk {
            default, buttons, ..
        } = src
        else {
            panic!("not a GNotification");
        };
        assert_eq!(default.unwrap().name, "app.open-room");
        assert_eq!(buttons.len(), 1);
        assert_eq!(
            buttons[0].target.as_ref().and_then(|t| as_str(t)),
            Some("room-1".into())
        );
    }
}

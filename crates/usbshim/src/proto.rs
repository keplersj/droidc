//! The `usb.open` exchange with the host: one JSON request line, one JSON reply line, and on success
//! one fd passed with the reply as `SCM_RIGHTS`. See `docs/design/usb.md`.

use serde_json::{json, Value};

/// The request line for device `bus`/`dev`, with its newline.
pub fn request(id: u64, bus: u16, dev: u16) -> String {
    let mut s =
        json!({"id": id, "method": "usb.open", "params": {"bus": bus, "dev": dev}}).to_string();
    s.push('\n');
    s
}

/// Why the host did not hand over a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// `not-allowed`, `gone`, `bad-request`, or another kind the host names.
    pub kind: String,
    pub message: String,
}

impl Refusal {
    fn new(kind: &str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }

    /// The errno an `open()` that got this refusal sets.
    pub fn errno(&self) -> i32 {
        match self.kind.as_str() {
            "not-allowed" => libc::EACCES,
            "gone" => libc::ENODEV,
            "bad-request" => libc::EINVAL,
            _ => libc::EIO,
        }
    }
}

/// Reads the reply line to request `id`: `Ok` when it carries a `result` (the fd comes with it),
/// otherwise the host's refusal, or a `protocol` refusal for a line that isn't a reply to `id`.
pub fn reply(line: &[u8], id: u64) -> Result<(), Refusal> {
    let v: Value = serde_json::from_slice(line)
        .map_err(|e| Refusal::new("protocol", format!("unreadable reply: {e}")))?;
    if v.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(Refusal::new("protocol", "reply to another request"));
    }
    if v.get("result").is_some() {
        return Ok(());
    }
    let err = v.get("error");
    let field = |k: &str| {
        err.and_then(|e| e.get(k))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match err {
        Some(_) => Err(Refusal {
            kind: field("kind"),
            message: field("message"),
        }),
        None => Err(Refusal::new(
            "protocol",
            "reply has neither result nor error",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_line() {
        assert_eq!(
            request(7, 1, 5),
            "{\"id\":7,\"method\":\"usb.open\",\"params\":{\"bus\":1,\"dev\":5}}\n"
        );
    }

    #[test]
    fn replies() {
        assert_eq!(reply(br#"{"id":3,"result":{}}"#, 3), Ok(()));
        let r = reply(
            br#"{"id":3,"error":{"kind":"not-allowed","message":"Allow it in Selkie"}}"#,
            3,
        )
        .unwrap_err();
        assert_eq!(r.kind, "not-allowed");
        assert_eq!(r.message, "Allow it in Selkie");
        assert_eq!(r.errno(), libc::EACCES);
    }

    #[test]
    fn errnos() {
        let e = |k: &str| Refusal::new(k, "").errno();
        assert_eq!(e("gone"), libc::ENODEV);
        assert_eq!(e("bad-request"), libc::EINVAL);
        assert_eq!(e("busy"), libc::EIO);
    }

    #[test]
    fn bad_replies() {
        assert_eq!(reply(b"not json", 1).unwrap_err().kind, "protocol");
        assert_eq!(
            reply(br#"{"id":2,"result":{}}"#, 1).unwrap_err().kind,
            "protocol"
        );
        assert_eq!(reply(br#"{"id":1}"#, 1).unwrap_err().kind, "protocol");
    }
}

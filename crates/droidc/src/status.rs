//! `status` and `shell-spec`. Both are read-only and work inside the guest with `DROIDC_ROOT`
//! pointing at the bound root: they only follow the relative `current`/`previous` symlinks.

use crate::error::{Error, Result};
use crate::layout::Root;
use crate::platform::Platform;
use serde_json::{json, Value};
use std::path::Path;

fn rec(root: &Root, id: Option<String>) -> Value {
    id.and_then(|i| root.read_record(&i).ok()).map(|r| serde_json::to_value(r).unwrap()).unwrap_or(Value::Null)
}

pub fn status(root_path: &Path) -> Result<Value> {
    let root = Root::new(root_path);
    let platform = Platform::detect().ok().map(|p| serde_json::to_value(p).unwrap()).unwrap_or(Value::Null);
    let pending = root.read_pending()?.map(|p| serde_json::to_value(p).unwrap()).unwrap_or(Value::Null);
    Ok(json!({
        "root": root_path.display().to_string(),
        "platform": platform,
        "current": rec(&root, root.current_id()),
        "previous": rec(&root, root.previous_id()),
        // Non-null only while an operation is in progress or was interrupted (recovered on next mutating command).
        "staged": pending,
    }))
}

pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub fn shell_spec(root_path: &Path) -> Result<Value> {
    let root = Root::new(root_path);
    let cur = root.current_id().ok_or_else(|| Error::not_found("no current deployment"))?;
    let record = root.read_record(&cur)?;
    let abs = std::path::absolute(root_path).unwrap_or_else(|_| root_path.to_path_buf());
    let r = abs.display().to_string();
    Ok(json!({
        "rootfs": format!("{r}/current/rootfs"),
        "binds": [
            {"host": format!("{r}/var"), "guest": "/var"},
            {"host": format!("{r}/home"), "guest": "/home"},
            {"host": r, "guest": "/run/droidc/root"},
        ],
        "env": {
            "DROIDC_ROOT": "/run/droidc/root",
            "HOME": "/root",
            "PATH": DEFAULT_PATH,
            "TERM": "xterm-256color",
            "LANG": "C.UTF-8",
        },
        "cwd": "/root",
        "session_label": record.session_label,
    }))
}

//! NDJSON event output (`--json`) and human output.
//!
//! In `--json` mode every stdout line is one JSON object with an `event` field; the last line of a
//! run is always `result`. Human text goes to stderr in both modes.

use crate::error::Error;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Emitter {
    json: bool,
    out: Mutex<Box<dyn Write + Send>>,
    last: Mutex<HashMap<String, Instant>>,
}

impl Emitter {
    pub fn stdout(json: bool) -> Self {
        Self::with_writer(json, Box::new(std::io::stdout()))
    }

    pub fn with_writer(json: bool, w: Box<dyn Write + Send>) -> Self {
        Emitter {
            json,
            out: Mutex::new(w),
            last: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_json(&self) -> bool {
        self.json
    }

    fn line(&self, v: &Value) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    pub fn progress(&self, phase: &str, current: u64, total: u64, unit: &str, message: &str) {
        if self.json {
            self.line(&json!({"event":"progress","phase":phase,"current":current,"total":total,"unit":unit,"message":message}));
        } else {
            eprintln!("[{phase}] {message} ({current}/{total} {unit})");
        }
    }

    /// Like `progress`, but at most every 200 ms per phase (the final `current == total` always goes).
    pub fn progress_throttled(
        &self,
        phase: &str,
        current: u64,
        total: u64,
        unit: &str,
        message: &str,
    ) {
        {
            let mut last = self.last.lock().unwrap();
            let now = Instant::now();
            let due = match last.get(phase) {
                Some(t) => now.duration_since(*t) >= Duration::from_millis(200),
                None => true,
            };
            if !due && !(total > 0 && current >= total) {
                return;
            }
            last.insert(phase.to_string(), now);
        }
        self.progress(phase, current, total, unit, message);
    }

    pub fn log(&self, level: &str, message: &str) {
        if self.json {
            self.line(&json!({"event":"log","level":level,"message":message}));
        } else {
            eprintln!("{level}: {message}");
        }
    }
    pub fn info(&self, m: &str) {
        self.log("info", m)
    }
    pub fn warn(&self, m: &str) {
        self.log("warn", m)
    }

    /// Final `result` line (json mode only; human mode output is printed by the caller).
    pub fn result_ok(&self, data: &Value) {
        if self.json {
            self.line(&json!({"event":"result","ok":true,"data":data}));
        }
    }

    pub fn result_err(&self, e: &Error) {
        if self.json {
            self.line(&json!({"event":"result","ok":false,"error":{"kind":e.kind.as_str(),"message":e.message}}));
        } else {
            eprintln!("error ({}): {}", e.kind.as_str(), e.message);
        }
    }
}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    pub struct Buf(pub Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Buf {
        pub fn lines(&self) -> Vec<Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }
    pub fn capture() -> (Emitter, Buf) {
        let b = Buf::default();
        (Emitter::with_writer(true, Box::new(b.clone())), b)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use crate::error::Error;
    use serde_json::json;

    #[test]
    fn ndjson_shapes() {
        let (em, buf) = capture();
        em.progress("download", 1, 10, "bytes", "layer 1/2");
        em.info("hello");
        em.warn("careful");
        em.result_ok(&json!({"changed":true}));
        let l = buf.lines();
        assert_eq!(
            l[0],
            json!({"event":"progress","phase":"download","current":1,"total":10,"unit":"bytes","message":"layer 1/2"})
        );
        assert_eq!(
            l[1],
            json!({"event":"log","level":"info","message":"hello"})
        );
        assert_eq!(l[2]["level"], "warn");
        assert_eq!(
            l[3],
            json!({"event":"result","ok":true,"data":{"changed":true}})
        );
    }

    #[test]
    fn error_result_shape() {
        let (em, buf) = capture();
        em.result_err(&Error::not_found("no such tag"));
        let l = buf.lines();
        assert_eq!(
            l[0],
            json!({"event":"result","ok":false,"error":{"kind":"not-found","message":"no such tag"}})
        );
    }

    #[test]
    fn throttle_always_emits_final() {
        let (em, buf) = capture();
        for i in 0..=100u64 {
            em.progress_throttled("download", i, 100, "bytes", "x");
        }
        let l = buf.lines();
        assert!(l.len() < 101);
        assert_eq!(l.last().unwrap()["current"], 100);
    }
}

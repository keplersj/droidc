//! `droidc audio-out`: streams the guest's sound to the host as raw PCM on stdout.
//!
//! The guest runs PipeWire and WirePlumber, but on Android there is no sound device behind them, so
//! desktop sound goes nowhere. This gives the guest one sink to play into and hands what plays there to
//! the host:
//!
//! 1. waits for the guest's PipeWire to answer;
//! 2. creates a null-audio sink (`selkie-out` by default) with `pw-cli create-node`, or reuses one of
//!    that name left by an earlier run, and makes it the configured default sink with `pw-metadata`;
//! 3. records that sink's monitor with the image's `pw-record` (a static droidc can't load
//!    libpipewire, as in `bridge.rs`) and copies it to stdout as interleaved signed 16-bit
//!    little-endian PCM at `--rate` and `--channels`, stripping the WAV header `pw-record` writes to a
//!    pipe;
//! 4. when stdout closes (the host stopped reading) or the recorder exits, stops the recorder and
//!    removes the sink it created. Killed by a signal, it leaves the sink behind; the next run reuses
//!    it, and with nothing reading it the sink just drops the sound, as the guest did before.
//!
//! The host (Selkie) plays stdout through `AudioTrack`. Status and errors go to stderr only, since
//! stdout carries audio.

use crate::error::{Error, Result};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Opts {
    /// `node.name` of the sink.
    pub sink: String,
    pub rate: u32,
    pub channels: u32,
    /// Recorder latency, as `pw-record --latency` takes it (`20ms`).
    pub latency: String,
    /// How long to wait for the guest's PipeWire.
    pub timeout: Duration,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            sink: "selkie-out".into(),
            rate: 48_000,
            channels: 2,
            latency: "20ms".into(),
            timeout: Duration::from_secs(60),
        }
    }
}

/// Channel positions for a channel count, as PipeWire's `audio.position` spells them.
fn positions(channels: u32) -> &'static str {
    match channels {
        1 => "[ MONO ]",
        2 => "[ FL FR ]",
        4 => "[ FL FR RL RR ]",
        6 => "[ FL FR FC LFE RL RR ]",
        _ => "[ FL FR ]",
    }
}

/// The `pw-cli` argv that creates the sink. `object.linger` keeps it after `pw-cli` exits; we
/// remove it ourselves when we stop.
pub fn create_sink_argv(o: &Opts) -> Vec<String> {
    let props = format!(
        "{{ factory.name=support.null-audio-sink node.name={name} node.description=\"Selkie\" \
         media.class=Audio/Sink audio.rate={rate} audio.channels={ch} audio.position={pos} \
         object.linger=true monitor.channel-volumes=true }}",
        name = o.sink,
        rate = o.rate,
        ch = o.channels,
        pos = positions(o.channels),
    );
    vec![
        "pw-cli".into(),
        "create-node".into(),
        "adapter".into(),
        props,
    ]
}

/// The `pw-metadata` argv that makes the sink the configured default.
pub fn default_sink_argv(o: &Opts) -> Vec<String> {
    vec![
        "pw-metadata".into(),
        "0".into(),
        "default.configured.audio.sink".into(),
        format!("{{ \"name\": \"{}\" }}", o.sink),
        "Spa:String:JSON".into(),
    ]
}

/// The recorder argv: the sink's monitor, S16LE to stdout. `raw` asks for headerless output where
/// `pw-record` has `--raw`; without it a WAV header comes first and `WavStrip` removes it.
pub fn record_argv(o: &Opts, raw: bool) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "pw-record".into(),
        "--target".into(),
        o.sink.clone(),
        "-P".into(),
        "{ stream.capture.sink=true node.name=selkie-out-capture media.role=Music }".into(),
        "--rate".into(),
        o.rate.to_string(),
        "--channels".into(),
        o.channels.to_string(),
        "--format".into(),
        "s16".into(),
        "--latency".into(),
        o.latency.clone(),
    ];
    if raw {
        v.push("--raw".into());
    }
    v.push("-".into());
    v
}

/// Whether this `pw-record` takes `--raw`.
fn has_raw() -> bool {
    Command::new("pw-record")
        .arg("--help")
        .stdin(Stdio::null())
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).contains("--raw")
                || String::from_utf8_lossy(&o.stderr).contains("--raw")
        })
        .unwrap_or(false)
}

/// The id of the node named `name` in `pw-dump`'s JSON, if there is one.
pub fn node_id(dump: &str, name: &str) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_str(dump).ok()?;
    v.as_array()?.iter().find_map(|o| {
        if o.get("type")?.as_str()? != "PipeWire:Interface:Node" {
            return None;
        }
        let props = o.get("info")?.get("props")?;
        (props.get("node.name")?.as_str()? == name)
            .then(|| o.get("id")?.as_u64().map(|i| i as u32))?
    })
}

/// Strips a WAV header from the front of a PCM stream. `pw-record` writes WAV to a pipe through
/// libsndfile; a header of unknown length on a pipe still ends at the `data` chunk's size field.
pub struct WavStrip {
    head: Vec<u8>,
    done: bool,
}

/// No real header is anywhere near this long; past it the stream is passed on as it came.
const MAX_HEADER: usize = 4096;

impl WavStrip {
    pub fn new() -> Self {
        WavStrip {
            head: Vec::new(),
            done: false,
        }
    }

    /// Feeds bytes in; returns the PCM to pass on (empty while the header is still incomplete).
    pub fn push(&mut self, buf: &[u8]) -> Vec<u8> {
        if self.done {
            return buf.to_vec();
        }
        self.head.extend_from_slice(buf);
        if self.head.len() < 12 {
            return Vec::new();
        }
        if &self.head[0..4] != b"RIFF" || &self.head[8..12] != b"WAVE" {
            self.done = true;
            return std::mem::take(&mut self.head);
        }
        // Walk the chunks after "RIFF....WAVE" to the "data" chunk.
        let mut at = 12;
        while at + 8 <= self.head.len() {
            let id = &self.head[at..at + 4];
            let len = u32::from_le_bytes(self.head[at + 4..at + 8].try_into().unwrap()) as usize;
            if id == b"data" {
                self.done = true;
                return self.head.split_off(at + 8);
            }
            // Chunks are padded to an even length.
            at += 8 + len + (len & 1);
        }
        if self.head.len() > MAX_HEADER {
            self.done = true;
            return std::mem::take(&mut self.head);
        }
        Vec::new()
    }
}

impl Default for WavStrip {
    fn default() -> Self {
        Self::new()
    }
}

fn run_tool(argv: &[String]) -> Result<String> {
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::not_found(format!("{}: {e}", argv[0])))?;
    if !out.status.success() {
        return Err(Error::internal(format!(
            "{} exited with {}: {}",
            argv[0],
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn dump() -> Result<String> {
    run_tool(&["pw-dump".to_string()])
}

/// Waits until the guest's PipeWire answers `pw-dump`.
fn wait_for_pipewire(timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        match dump() {
            Ok(d) => return Ok(d),
            Err(e) if Instant::now() > deadline => {
                return Err(Error::not_found(format!(
                    "PipeWire did not answer within {timeout:?}: {e}"
                )))
            }
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// The sink, and whether this run created it (and so removes it).
fn ensure_sink(o: &Opts, timeout: Duration) -> Result<(u32, bool)> {
    let d = wait_for_pipewire(timeout)?;
    if let Some(id) = node_id(&d, &o.sink) {
        eprintln!("audio-out: reusing sink {} ({id})", o.sink);
        return Ok((id, false));
    }
    run_tool(&create_sink_argv(o))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(id) = node_id(&dump()?, &o.sink) {
            eprintln!("audio-out: created sink {} ({id})", o.sink);
            return Ok((id, true));
        }
        if Instant::now() > deadline {
            return Err(Error::internal(format!(
                "sink {} did not appear after pw-cli create-node",
                o.sink
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Cleanup {
    sink: Option<u32>,
    recorder: Option<Child>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(mut c) = self.recorder.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(id) = self.sink.take() {
            let _ = run_tool(&["pw-cli".into(), "destroy".into(), id.to_string()]);
        }
    }
}

/// Copies the recorder's PCM to `out` until either side stops. Returns the recorder's exit code, or
/// 0 when `out` closed first.
fn pump(mut child: Child, out: &mut dyn Write, cleanup: &mut Cleanup) -> Result<i32> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::internal("pw-record has no stdout"))?;
    cleanup.recorder = Some(child);
    let mut strip = WavStrip::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::internal(format!("reading pw-record: {e}"))),
        };
        let pcm = strip.push(&buf[..n]);
        if pcm.is_empty() {
            continue;
        }
        if out.write_all(&pcm).and_then(|_| out.flush()).is_err() {
            // The host stopped reading: a normal stop.
            return Ok(0);
        }
    }
    let status = cleanup
        .recorder
        .take()
        .map(|mut c| c.wait())
        .transpose()
        .map_err(|e| Error::internal(format!("pw-record: {e}")))?;
    Ok(status.and_then(|s| s.code()).unwrap_or(1))
}

pub fn run(o: &Opts) -> Result<i32> {
    // A closed stdout must end the run with an error from write, not kill us before cleanup.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let (id, ours) = ensure_sink(o, o.timeout)?;
    let mut cleanup = Cleanup {
        sink: ours.then_some(id),
        recorder: None,
    };
    if let Err(e) = run_tool(&default_sink_argv(o)) {
        eprintln!(
            "audio-out: warn: could not make {} the default sink: {e}",
            o.sink
        );
    }
    let argv = record_argv(o, has_raw());
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Error::not_found(format!("pw-record: {e}")))?;
    eprintln!(
        "audio-out: streaming {} as s16le {} Hz x{}",
        o.sink, o.rate, o.channels
    );
    let mut out = std::io::stdout().lock();
    pump(child, &mut out, &mut cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_header(extra_chunk: bool) -> Vec<u8> {
        let mut h = b"RIFF\xff\xff\xff\xffWAVE".to_vec();
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&[1, 0, 2, 0]);
        h.extend_from_slice(&48_000u32.to_le_bytes());
        h.extend_from_slice(&192_000u32.to_le_bytes());
        h.extend_from_slice(&[4, 0, 16, 0]);
        if extra_chunk {
            h.extend_from_slice(b"LIST");
            h.extend_from_slice(&3u32.to_le_bytes());
            h.extend_from_slice(b"abc\0");
        }
        h.extend_from_slice(b"data");
        h.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        h
    }

    #[test]
    fn a_wav_header_is_stripped_even_when_split_across_reads() {
        for extra in [false, true] {
            let mut stream = wav_header(extra);
            stream.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
            for split in 1..stream.len() {
                let mut s = WavStrip::new();
                let mut got = s.push(&stream[..split]);
                got.extend(s.push(&stream[split..]));
                assert_eq!(got, [1, 2, 3, 4, 5, 6], "split at {split}, extra {extra}");
            }
        }
    }

    #[test]
    fn raw_pcm_passes_through() {
        let mut s = WavStrip::new();
        assert!(s.push(&[9, 9]).is_empty());
        let got = s.push(&[8; 20]);
        assert_eq!(got.len(), 22);
        assert_eq!(s.push(&[7]), [7]);
    }

    #[test]
    fn node_ids_are_found_by_name() {
        let dump = r#"[
          {"id":0,"type":"PipeWire:Interface:Core","info":{"props":{}}},
          {"id":31,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"other"}}},
          {"id":42,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"selkie-out"}}},
          {"id":43,"type":"PipeWire:Interface:Port","info":{"props":{"node.name":"selkie-out"}}}
        ]"#;
        assert_eq!(node_id(dump, "selkie-out"), Some(42));
        assert_eq!(node_id(dump, "missing"), None);
        assert_eq!(node_id("not json", "selkie-out"), None);
    }

    #[test]
    fn argv_carry_the_sink_and_format() {
        let o = Opts::default();
        let create = create_sink_argv(&o).join(" ");
        assert!(create.contains("node.name=selkie-out"));
        assert!(create.contains("media.class=Audio/Sink"));
        assert!(create.contains("audio.position=[ FL FR ]"));
        let rec = record_argv(&o, false);
        assert_eq!(rec[0], "pw-record");
        assert_eq!(rec.last().unwrap(), "-");
        assert!(!rec.contains(&"--raw".to_string()));
        let raw = record_argv(&o, true);
        assert_eq!(&raw[raw.len() - 2..], ["--raw", "-"]);
        assert!(rec.windows(2).any(|w| w == ["--target", "selkie-out"]));
        assert!(rec.windows(2).any(|w| w == ["--format", "s16"]));
        assert!(default_sink_argv(&o)[3].contains("\"selkie-out\""));
    }
}

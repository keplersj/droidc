//! `droidc audio-out` against a real PipeWire: a tone played into the default sink comes out of
//! stdout as S16LE PCM, with no WAV header. Needs `pipewire`,
//! `wireplumber`, the PipeWire tools and `dbus-run-session`; skipped without them (CI installs them).

use std::path::Path;
use std::process::{Command, Stdio};

fn have(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// One second of a 440 Hz tone at half scale, 48 kHz stereo, as a WAV file.
fn write_tone(p: &Path) {
    let rate = 48_000u32;
    let mut pcm = Vec::new();
    for i in 0..rate {
        let s = ((i as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin() * 16_000.0) as i16;
        pcm.extend_from_slice(&s.to_le_bytes());
        pcm.extend_from_slice(&s.to_le_bytes());
    }
    let mut w = b"RIFF".to_vec();
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&[1, 0, 2, 0]);
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 4).to_le_bytes());
    w.extend_from_slice(&[4, 0, 16, 0]);
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(&pcm);
    std::fs::write(p, w).unwrap();
}

#[test]
fn a_tone_played_in_the_guest_comes_out_as_pcm() {
    let tools = [
        "pipewire",
        "wireplumber",
        "pw-play",
        "pw-record",
        "pw-cli",
        "pw-dump",
        "pw-metadata",
        "dbus-run-session",
    ];
    if let Some(missing) = tools.iter().find(|t| !have(t)) {
        eprintln!("skipped: no {missing}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path().join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let tone = dir.path().join("tone.wav");
    write_tone(&tone);
    let pcm = dir.path().join("out.pcm");
    // pipewire and wireplumber as the guest runs them; audio-out until the tone has played.
    let script = r#"
        pipewire & PW=$!
        sleep 1
        wireplumber & WP=$!
        "$DROIDC" audio-out --timeout 20 > "$PCM" 2> "$PCM.log" & A=$!
        for i in $(seq 1 100); do pw-dump 2>/dev/null | grep -q '"node.name": "selkie-out"' && break; sleep 0.1; done
        sleep 1
        pw-play --target selkie-out "$TONE"
        sleep 0.5
        kill -TERM $A; wait $A
        kill $WP $PW; wait
        cat "$PCM.log" >&2
    "#;
    let out = Command::new("dbus-run-session")
        .args(["--", "sh", "-c", script])
        .env("XDG_RUNTIME_DIR", &run)
        .env("DROIDC", env!("CARGO_BIN_EXE_droidc"))
        .env("PCM", &pcm)
        .env("TONE", &tone)
        .env_remove("PIPEWIRE_REMOTE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    let bytes = std::fs::read(&pcm).unwrap_or_default();
    assert!(
        bytes.len() > 48_000,
        "only {} bytes of PCM; log:\n{log}",
        bytes.len()
    );
    assert!(
        !bytes.starts_with(b"RIFF"),
        "a WAV header got through; log:\n{log}"
    );
    let loud = bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs())
        .filter(|&s| s > 8_000)
        .count();
    assert!(
        loud > 10_000,
        "the tone did not come through ({loud} loud samples); log:\n{log}"
    );
}

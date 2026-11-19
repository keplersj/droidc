//! `droidc supervise` as a child process: orphans must be re-parented to it.
use std::process::Command;

#[test]
fn orphans_are_reparented_to_the_supervisor() {
    // The inner shell backgrounds `sleep` and exits, orphaning it. The outer shell then reads the
    // orphan's parent: it must be the supervisor, which is the outer shell's own parent ($PPID).
    let d = tempfile::tempdir().unwrap();
    let script = format!(
        "sh -c 'sleep 5 & echo $! > {pf}'; sleep 0.3; \
         p=$(cat {pf}); set -- $(cat /proc/$p/stat | sed 's/.*) //'); \
         echo \"$2 $PPID\"; kill $p",
        pf = d.path().join("pid").display()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_droidc"))
        .args(["supervise", "--", "sh", "-c", &script])
        .env("XDG_RUNTIME_DIR", d.path())
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let f: Vec<&str> = text.split_whitespace().collect();
    assert_eq!(f.len(), 2, "stdout {text:?} stderr {:?}", out.stderr);
    assert_eq!(f[0], f[1], "orphan's ppid vs supervisor pid");
}

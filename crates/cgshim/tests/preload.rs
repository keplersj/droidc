//! Loads the built shim into ordinary programs (`sh`, `cat`, `stat`, `mkdir`) with `LD_PRELOAD` and checks
//! what they see. `CGSHIM_ROOT` moves the "cgroup tree" to a temporary directory, so nothing here needs
//! `/sys/fs/cgroup` or any privilege.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The built shim. `cargo test` does not build a cdylib for an integration test, so build it here (once);
/// otherwise the tests would load a stale one, or none.
fn shim() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let mut build =
                Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
            build.args([
                "build",
                "-p",
                "cgshim",
                "--manifest-path",
                concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            ]);
            if !cfg!(debug_assertions) {
                build.arg("--release");
            }
            assert!(
                build.status().expect("run cargo").success(),
                "cargo build -p cgshim failed"
            );
            // The test binary sits in target/<profile>/deps (older cargo) or target/<profile>/build/<pkg>/<hash>/out
            // (newer); the cdylib is in target/<profile>. Walk up until it turns up.
            let exe = std::env::current_exe().unwrap();
            exe.ancestors()
                .map(|dir| dir.join("libcgshim.so"))
                .find(|so| so.is_file())
                .unwrap_or_else(|| panic!("libcgshim.so not found above {}", exe.display()))
        })
        .clone()
}

struct Tree(PathBuf);
impl Tree {
    fn new(name: &str) -> Tree {
        let p = std::env::temp_dir().join(format!("cgshim-it-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tree(p)
    }
    fn sh(&self, script: &str) -> (String, String) {
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("LD_PRELOAD", shim())
            .env("CGSHIM_ROOT", &self.0)
            .env("CGSHIM_TRACE", "1")
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
    fn p(&self, rel: &str) -> String {
        self.0.join(rel).display().to_string()
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Child half of the statfs tests: `statfs`/`fstatfs` the paths in `CGSHIM_HELPER_PATHS`, as systemd does,
/// and print the filesystem types. (`stat -f` cannot be used: it goes through `statvfs`, whose
/// internals are not interposable.) Does nothing when run as an ordinary test.
#[test]
fn statfs_helper() {
    let Some(paths) = std::env::var_os("CGSHIM_HELPER_PATHS") else {
        return;
    };
    for p in std::env::split_paths(&paths) {
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
        let mut b: libc::statfs = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::statfs(c.as_ptr(), &mut b) }, 0);
        let by_path = b.f_type as i64;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
        assert!(fd >= 0);
        let mut f: libc::statfs = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstatfs(fd, &mut f) }, 0);
        println!("TYPES {:x} {:x}", by_path, f.f_type as i64);
    }
}

fn statfs_types(t: &Tree, paths: &[String]) -> Vec<(String, String)> {
    let joined = std::env::join_paths(paths).unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "statfs_helper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LD_PRELOAD", shim())
        .env("CGSHIM_ROOT", &t.0)
        .env("CGSHIM_HELPER_PATHS", joined)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    text.lines()
        .filter_map(|l| l.split_once("TYPES ").map(|(_, rest)| rest))
        .map(|l| {
            let mut it = l.split(' ');
            (
                it.next().unwrap().to_string(),
                it.next().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn the_tree_reports_cgroup2_by_path_and_by_fd_and_nothing_else_does() {
    let t = Tree::new("statfs");
    std::fs::create_dir(t.0.join("sub")).unwrap();
    let got = statfs_types(&t, &[t.p(""), t.p("sub"), "/".to_string()]);
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[0], ("63677270".into(), "63677270".into()));
    assert_eq!(got[1], ("63677270".into(), "63677270".into()));
    assert_ne!(got[2].0, "63677270");
    assert_ne!(got[2].1, "63677270");
}

#[test]
fn mkdir_makes_the_cgroup_files_and_rmdir_removes_them() {
    let t = Tree::new("mkdir");
    let (out, err) = t.sh(&format!(
        "mkdir {a} && ls {a} && rmdir {a} && echo gone",
        a = t.p("a")
    ));
    for f in [
        "cgroup.procs",
        "cgroup.events",
        "cgroup.kill",
        "cgroup.max.descendants",
    ] {
        assert!(
            out.lines().any(|l| l == f),
            "{f} missing in {out:?} ({err})"
        );
    }
    assert!(out.trim_end().ends_with("gone"), "{out:?} {err}");
    assert!(!Path::new(&t.p("a")).exists());
    let (ev, _) = t.sh(&format!("mkdir {a} && cat {a}/cgroup.events", a = t.p("b")));
    assert_eq!(ev, "populated 1\nfrozen 0\n");
}

#[test]
fn a_directory_without_its_files_is_repaired_on_first_open() {
    let t = Tree::new("repair");
    std::fs::create_dir(t.0.join("init.scope")).unwrap(); // made where the shim was not looking
    let (out, err) = t.sh(&format!("cat {}", t.p("init.scope/cgroup.events")));
    assert_eq!(out, "populated 1\nfrozen 0\n", "{err}");
    assert!(err.contains("repaired"), "{err}");
    // a file in a directory that does not exist is still an error
    let (out, _) = t.sh(&format!(
        "cat {} 2>&1 || echo failed",
        t.p("nope/cgroup.events")
    ));
    assert!(out.contains("failed"), "{out}");
}

#[test]
fn proc_cgroup_names_the_tree_directory_that_lists_the_pid() {
    let t = Tree::new("proc");
    let (out, _) = t.sh("read x < /proc/self/cgroup; echo \"$x\"");
    assert_eq!(out.trim(), "0::/");
    let script = format!(
        "mkdir -p {d} && echo $$ >> {d}/cgroup.procs && read x < /proc/self/cgroup && echo \"$x\"",
        d = t.p("session.slice")
    );
    let (out, err) = t.sh(&script);
    assert_eq!(out.trim(), "0::/session.slice", "{err}");
}

#[test]
fn programs_outside_the_tree_are_untouched() {
    let t = Tree::new("outside");
    let other = std::env::temp_dir().join(format!("cgshim-it-{}-other", std::process::id()));
    let _ = std::fs::remove_dir_all(&other);
    let (out, _) = t.sh(&format!(
        "mkdir {o} && ls -A {o} | wc -l && rmdir {o}",
        o = other.display()
    ));
    assert_eq!(out.trim(), "0");
    let _ = std::fs::remove_dir_all(&other);
}

#[test]
fn directory_listings_still_work_and_dt_lnk_is_hidden_from_callers() {
    let t = Tree::new("readdir");
    std::fs::write(t.0.join("f"), "x").unwrap();
    std::os::unix::fs::symlink("f", t.0.join("l")).unwrap();
    let (out, _) = t.sh(&format!("ls -1 {}", t.p("")));
    assert_eq!(out.lines().collect::<Vec<_>>(), ["f", "l"]);
}

/// Child half of the seccomp test: with an unreadable filter pointer the real calls fail with EFAULT, so
/// success can only mean the shim answered without calling them.
#[test]
fn seccomp_helper() {
    if std::env::var_os("CGSHIM_HELPER_SECCOMP").is_none() {
        return;
    }
    let via_prctl = unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, std::ptr::null::<u8>(), 0, 0) };
    let via_syscall = unsafe { libc::syscall(libc::SYS_seccomp, 1, 0, std::ptr::null::<u8>()) };
    // Strict mode and other prctl options are not touched: an invalid mode still fails.
    let other = unsafe { libc::prctl(libc::PR_SET_SECCOMP, 99, 0, 0, 0) };
    println!("SECCOMP {via_prctl} {via_syscall} {other}");
}

fn seccomp_results(preload: bool) -> (i64, i64, i64) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "seccomp_helper",
        "--nocapture",
        "--test-threads=1",
    ])
    .env("CGSHIM_HELPER_SECCOMP", "1");
    if preload {
        cmd.env("LD_PRELOAD", shim());
    }
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let line = text
        .split_once("SECCOMP ")
        .unwrap_or_else(|| panic!("{text}"))
        .1
        .lines()
        .next()
        .unwrap()
        .to_string();
    let v: Vec<i64> = line.split(' ').map(|n| n.parse().unwrap()).collect();
    (v[0], v[1], v[2])
}

#[test]
fn installing_a_seccomp_filter_is_reported_as_done_and_nothing_else_changes() {
    let (p, s, o) = seccomp_results(true);
    assert_eq!((p, s), (0, 0), "the shim answers for filter installs");
    assert_eq!(o, -1, "other seccomp modes still reach the kernel");
    let (p, s, _) = seccomp_results(false);
    assert_eq!(
        (p, s),
        (-1, -1),
        "without the shim the same calls fail (EFAULT): the test proves something"
    );
}

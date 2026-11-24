//! Puts the git revision into the version: `droidc 0.1.0 (9cf69ff)`, with `-dirty` when the tree has
//! uncommitted changes. CI or a packager without a git checkout can set `DROIDC_GIT_REV` instead.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=DROIDC_GIT_REV");
    let rev = match std::env::var("DROIDC_GIT_REV") {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => git(&["rev-parse", "--short=7", "HEAD"])
            .filter(|s| !s.is_empty())
            .map(|sha| {
                // Tracked files only: build output and editor files don't make a build dirty.
                let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                    .is_some_and(|s| !s.is_empty());
                if dirty {
                    format!("{sha}-dirty")
                } else {
                    sha
                }
            }),
    };
    // Rebuild when HEAD moves or the index changes (a commit, a checkout, staging an edit).
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        for f in ["HEAD", "index", "packed-refs"] {
            println!("cargo:rerun-if-changed={dir}/{f}");
        }
        if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{head}");
        }
    }
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap();
    let version = match &rev {
        Some(r) => format!("{pkg} ({r})"),
        None => pkg,
    };
    println!("cargo:rustc-env=DROIDC_VERSION={version}");
    println!("cargo:rustc-env=DROIDC_GIT_REV={}", rev.unwrap_or_default());
}

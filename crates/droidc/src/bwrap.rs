//! `bwrap` in the guest: Flatpak's sandbox without user namespaces.
//!
//! Flatpak runs every app, every install trigger and its D-Bus proxy under bubblewrap, which builds a
//! new mount namespace from its argv. Android gives apps no user namespaces, so the real `bwrap`
//! fails before it starts. proot can build the same view (a root and a list of binds) without any
//! namespace, but a process already traced by proot cannot start another proot. So, as with `sudo`
//! (`spawn.rs`), the guest's `/usr/bin/bwrap` is droidc (named by `argv[0]`): it parses bubblewrap's
//! argv, resolves every source path in the guest, reads the data it is handed through descriptors,
//! and asks the host spawn-server to start a sibling proot with that view, as the user (no `-0`).
//!
//! The host translates guest paths to host paths through its own launcher's `-r`/`-b` list, so a
//! sandbox only ever sees what the guest already could; a bind of a guest directory carries the
//! launcher's binds below it, as a recursive bind would.
//!
//! What a sandbox here is not: isolated. There are no namespaces, so the app shares the guest's pids,
//! network and IPC, `--ro-*` binds are writable (proot has no read-only binds) and the seccomp filter
//! is not applied. It gets the filesystem view Flatpak asked for, which is what apps need to run.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Where the shim is bound in the guest (`shell-spec`).
pub const SHIM: &str = "/usr/bin/bwrap";

/// What a sandbox's filesystem is made of, in bubblewrap's order. Sources are guest paths, already
/// resolved in the guest; destinations are paths inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// `--bind`, `--ro-bind`, `--dev-bind` (and their `-try` forms, dropped in the guest when the
    /// source is missing), `--bind-fd`, `--proc` and `--dev` (the guest's own `/proc` and `/dev`).
    Bind {
        src: String,
        dest: String,
    },
    /// An empty directory of the sandbox's own.
    Tmpfs {
        dest: String,
    },
    Dir {
        dest: String,
    },
    Symlink {
        target: String,
        dest: String,
    },
    /// `--file`, `--bind-data`, `--ro-bind-data`: the bytes read from the descriptor.
    Data {
        dest: String,
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
}

mod b64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)
    }
}

/// What a `bwrap` request adds to a spawner request (`spawn::Request::sandbox`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sandbox {
    pub ops: Vec<Op>,
    /// The numbers the descriptors sent after stdin, stdout and stderr have in the sandbox, in order.
    pub fds: Vec<i32>,
    /// The caller was (proot's) root: so is the sandbox.
    pub root: bool,
}

/// At most this many descriptors (besides stdin, stdout and stderr) go to a sandbox.
pub const MAX_FDS: usize = 61;

/// A parsed bubblewrap command line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Args {
    pub ops: Vec<Op>,
    /// Environment changes, in order: `Some` sets, `None` unsets.
    pub env: Vec<(String, Option<String>)>,
    pub clearenv: bool,
    pub chdir: Option<String>,
    pub info_fd: Option<i32>,
    pub json_status_fd: Option<i32>,
    pub sync_fd: Option<i32>,
    pub block_fd: Option<i32>,
    /// Descriptors bubblewrap reads itself (`--args`, data, seccomp): not passed to the command.
    pub consumed_fds: Vec<i32>,
    pub command: Vec<String>,
    pub version: bool,
    pub warnings: Vec<String>,
}

/// How the shim reads what descriptors hold; tests give it a table instead of real descriptors.
pub trait FdSource {
    fn read_all(&mut self, fd: i32) -> Result<Vec<u8>>;
    /// The guest path an `O_PATH` descriptor (`--bind-fd`) refers to.
    fn path_of(&mut self, fd: i32) -> Result<String>;
}

/// Real descriptors.
pub struct ProcessFds;

impl FdSource for ProcessFds {
    fn read_all(&mut self, fd: i32) -> Result<Vec<u8>> {
        use std::io::Read;
        use std::os::unix::io::FromRawFd;
        // SAFETY: bubblewrap's contract: the descriptor is ours to read and close.
        let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .map_err(|e| Error::invalid(format!("bwrap: reading fd {fd}: {e}")))?;
        Ok(buf)
    }
    fn path_of(&mut self, fd: i32) -> Result<String> {
        std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .map(|p| p.display().to_string())
            .map_err(|e| Error::invalid(format!("bwrap: fd {fd}: {e}")))
    }
}

fn parse_fd(s: &str) -> Result<i32> {
    s.parse::<i32>()
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| Error::invalid(format!("bwrap: invalid fd {s:?}")))
}

/// Lexically normalized absolute path (`.` and `..` resolved, no trailing slash), or `None` when it is
/// not absolute.
pub fn normalize(p: &str) -> Option<String> {
    let path = Path::new(p);
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    Some(out.display().to_string())
}

/// Parses bubblewrap's argv (without `argv[0]`), expanding `--args`, reading data descriptors and
/// resolving bind sources with `resolve` (the guest's `realpath`; `None` when missing).
pub fn parse(
    argv: &[String],
    fds: &mut dyn FdSource,
    resolve: &dyn Fn(&str) -> Option<String>,
) -> Result<Args> {
    let mut a = Args::default();
    let mut queue: std::collections::VecDeque<String> = argv.iter().cloned().collect();
    let next = |q: &mut std::collections::VecDeque<String>, opt: &str| -> Result<String> {
        q.pop_front()
            .ok_or_else(|| Error::invalid(format!("bwrap: {opt} takes an argument")))
    };
    while let Some(opt) = queue.pop_front() {
        if !opt.starts_with("--") {
            a.command.push(opt);
            a.command.extend(queue.drain(..));
            break;
        }
        match opt.as_str() {
            "--" => {
                a.command.extend(queue.drain(..));
                break;
            }
            "--version" => a.version = true,
            "--args" => {
                let fd = parse_fd(&next(&mut queue, &opt)?)?;
                let data = fds.read_all(fd)?;
                a.consumed_fds.push(fd);
                let more: Vec<String> = data
                    .split(|b| *b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect();
                for (i, m) in more.into_iter().enumerate() {
                    queue.insert(i, m);
                }
            }
            "--bind" | "--ro-bind" | "--dev-bind" | "--bind-try" | "--ro-bind-try"
            | "--dev-bind-try" => {
                let src = next(&mut queue, &opt)?;
                let dest = next(&mut queue, &opt)?;
                match resolve(&src) {
                    Some(src) => a.ops.push(Op::Bind { src, dest }),
                    None if opt.ends_with("-try") => {}
                    None => {
                        return Err(Error::not_found(format!(
                            "bwrap: Can't find source path {src}"
                        )))
                    }
                }
            }
            "--bind-fd" | "--ro-bind-fd" => {
                let fd = parse_fd(&next(&mut queue, &opt)?)?;
                let dest = next(&mut queue, &opt)?;
                let src = fds.path_of(fd)?;
                a.consumed_fds.push(fd);
                let src = resolve(&src)
                    .ok_or_else(|| Error::not_found(format!("bwrap: fd {fd}: {src} is gone")))?;
                a.ops.push(Op::Bind { src, dest });
            }
            "--proc" | "--dev" => {
                let dest = next(&mut queue, &opt)?;
                let src = if opt == "--proc" { "/proc" } else { "/dev" };
                a.ops.push(Op::Bind {
                    src: src.into(),
                    dest,
                });
            }
            "--tmpfs" | "--mqueue" => {
                let dest = next(&mut queue, &opt)?;
                a.ops.push(Op::Tmpfs { dest });
            }
            "--dir" => {
                let dest = next(&mut queue, &opt)?;
                a.ops.push(Op::Dir { dest });
            }
            "--symlink" => {
                let target = next(&mut queue, &opt)?;
                let dest = next(&mut queue, &opt)?;
                a.ops.push(Op::Symlink { target, dest });
            }
            "--file" | "--bind-data" | "--ro-bind-data" => {
                let fd = parse_fd(&next(&mut queue, &opt)?)?;
                let dest = next(&mut queue, &opt)?;
                let data = fds.read_all(fd)?;
                a.consumed_fds.push(fd);
                a.ops.push(Op::Data { dest, data });
            }
            "--seccomp" | "--add-seccomp-fd" => {
                let fd = parse_fd(&next(&mut queue, &opt)?)?;
                let _ = fds.read_all(fd);
                a.consumed_fds.push(fd);
                a.warnings
                    .push("seccomp filters are not applied under proot".into());
            }
            "--setenv" => {
                let k = next(&mut queue, &opt)?;
                let v = next(&mut queue, &opt)?;
                a.env.push((k, Some(v)));
            }
            "--unsetenv" => {
                let k = next(&mut queue, &opt)?;
                a.env.push((k, None));
            }
            "--clearenv" => {
                a.clearenv = true;
                a.env.clear();
            }
            "--chdir" => a.chdir = Some(next(&mut queue, &opt)?),
            "--info-fd" => a.info_fd = Some(parse_fd(&next(&mut queue, &opt)?)?),
            "--json-status-fd" => a.json_status_fd = Some(parse_fd(&next(&mut queue, &opt)?)?),
            "--sync-fd" => a.sync_fd = Some(parse_fd(&next(&mut queue, &opt)?)?),
            "--block-fd" => a.block_fd = Some(parse_fd(&next(&mut queue, &opt)?)?),
            "--userns" | "--userns2" | "--pidns" => {
                let fd = next(&mut queue, &opt)?;
                a.warnings.push(format!(
                    "{opt} {fd}: joining another sandbox is not possible under proot; starting a new one"
                ));
            }
            // Consumed and ignored: what they set up needs namespaces, labels or a real tmpfs.
            "--perms" | "--size" | "--remount-ro" | "--lock-file" | "--uid" | "--gid"
            | "--hostname" | "--exec-label" | "--file-label" | "--cap-add" | "--cap-drop"
            | "--userns-block-fd" | "--argv0" => {
                next(&mut queue, &opt)?;
            }
            "--chmod" => {
                next(&mut queue, &opt)?;
                next(&mut queue, &opt)?;
            }
            "--unshare-all"
            | "--unshare-user"
            | "--unshare-user-try"
            | "--unshare-ipc"
            | "--unshare-pid"
            | "--unshare-net"
            | "--unshare-uts"
            | "--unshare-cgroup"
            | "--unshare-cgroup-try"
            | "--share-net"
            | "--die-with-parent"
            | "--as-pid-1"
            | "--new-session"
            | "--disable-userns"
            | "--assert-userns-disabled"
            | "--level-prefix" => {}
            o if o.starts_with("--overlay") || o == "--tmp-overlay" || o == "--ro-overlay" => {
                return Err(Error::invalid(format!(
                    "bwrap: {o} is not supported under proot"
                )))
            }
            o => return Err(Error::invalid(format!("bwrap: Unknown option {o}"))),
        }
    }
    Ok(a)
}

/// The host's own launcher (`spawn-server -- proot ...`), taken apart: its rootfs and binds translate
/// guest paths to host paths, and its remaining flags start a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher {
    pub program: String,
    /// Flags kept for a sandbox (`--link2symlink`, `--kill-on-exit`, ...), without `-0`, `-r`, `-b`,
    /// `-w` and their values.
    pub flags: Vec<String>,
    pub rootfs: PathBuf,
    /// `(host, guest)`, in the launcher's order.
    pub binds: Vec<(PathBuf, String)>,
}

impl Launcher {
    pub fn parse(argv: &[String]) -> Result<Launcher> {
        let mut it = argv.iter();
        let program = it
            .next()
            .cloned()
            .ok_or_else(|| Error::invalid("empty launcher"))?;
        let mut flags = Vec::new();
        let mut rootfs = None;
        let mut binds = Vec::new();
        let bind = |spec: &str| -> (PathBuf, String) {
            match spec.split_once(':') {
                Some((h, g)) => (PathBuf::from(h), normalize(g).unwrap_or_else(|| g.into())),
                None => (
                    PathBuf::from(spec),
                    normalize(spec).unwrap_or_else(|| spec.into()),
                ),
            }
        };
        while let Some(a) = it.next() {
            match a.as_str() {
                "-0" | "--root-id" => {}
                "-r" | "--rootfs" => rootfs = it.next().map(PathBuf::from),
                "-b" | "--bind" | "-m" | "--mount" => {
                    if let Some(s) = it.next() {
                        binds.push(bind(s));
                    }
                }
                "-w" | "--pwd" | "--cwd" | "-i" | "--change-id" => {
                    it.next();
                }
                a if a.starts_with("--rootfs=") => {
                    rootfs = Some(PathBuf::from(&a["--rootfs=".len()..]))
                }
                a if a.starts_with("--bind=") => binds.push(bind(&a["--bind=".len()..])),
                a if a.starts_with("--pwd=") || a.starts_with("--cwd=") => {}
                a => flags.push(a.to_string()),
            }
        }
        let rootfs = rootfs.ok_or_else(|| Error::invalid("launcher has no -r rootfs"))?;
        Ok(Launcher {
            program,
            flags,
            rootfs,
            binds,
        })
    }

    /// The host path behind a (normalized) guest path: the innermost launcher bind, else the rootfs.
    pub fn to_host(&self, guest: &str) -> PathBuf {
        let mut best: Option<(&PathBuf, &str)> = None;
        for (h, g) in &self.binds {
            if under(guest, g) && best.is_none_or(|(_, bg)| g.len() >= bg.len()) {
                best = Some((h, g));
            }
        }
        match best {
            Some((h, g)) => join(h, rel(guest, g)),
            None => join(&self.rootfs, guest.trim_start_matches('/')),
        }
    }

    /// The launcher binds strictly below `guest`, as `(host, path relative to guest)`: a bind of a guest
    /// directory brings them along, as a recursive bind would.
    fn below(&self, guest: &str) -> Vec<(PathBuf, String)> {
        self.binds
            .iter()
            .filter(|(_, g)| g != guest && under(g, guest))
            .map(|(h, g)| (h.clone(), rel(g, guest).to_string()))
            .collect()
    }
}

/// `path` is `dir` or below it (both normalized).
fn under(path: &str, dir: &str) -> bool {
    dir == "/" || path == dir || path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

/// `path` relative to `dir` (`path` under `dir`), without a leading slash.
fn rel<'a>(path: &'a str, dir: &str) -> &'a str {
    if dir == "/" {
        path.trim_start_matches('/')
    } else {
        path[dir.len()..].trim_start_matches('/')
    }
}

fn join(base: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        base.to_path_buf()
    } else {
        base.join(rel)
    }
}

/// A sandbox ready to start: proot's `-b` list over `root`, built in `staging`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    pub root: PathBuf,
    /// `(host, sandbox path)`, outermost first.
    pub binds: Vec<(PathBuf, String)>,
}

impl Plan {
    /// The host path a sandbox path ends up at.
    pub fn host(&self, dest: &str) -> PathBuf {
        let inner = self
            .binds
            .iter()
            .filter(|(_, d)| under(dest, d))
            .max_by_key(|(_, d)| d.len());
        match inner {
            Some((h, d)) => join(h, rel(dest, d)),
            None => join(&self.root, dest.trim_start_matches('/')),
        }
    }

    /// proot's argv for the sandbox, up to and including `-w cwd` (the command follows).
    pub fn argv(&self, launcher: &Launcher, cwd: &str, root: bool) -> Vec<String> {
        let mut argv = vec![launcher.program.clone()];
        argv.extend(launcher.flags.iter().cloned());
        if root {
            argv.push("-0".into());
        }
        argv.push("-r".into());
        argv.push(self.root.display().to_string());
        for (h, g) in &self.binds {
            argv.push("-b".into());
            argv.push(format!("{}:{g}", h.display()));
        }
        argv.push("-w".into());
        argv.push(cwd.into());
        argv
    }
}

/// Lays the sandbox out under `staging` (its root, tmpfs directories and data files) and returns the
/// binds proot needs. `allowed` are the host directories a bind may resolve into (the guest's own
/// view); a source that resolves anywhere else is refused.
pub fn plan(launcher: &Launcher, ops: &[Op], staging: &Path, allowed: &[PathBuf]) -> Result<Plan> {
    use std::fs;
    let io = |what: &Path, e: std::io::Error| Error::internal(format!("{}: {e}", what.display()));
    let root = staging.join("root");
    fs::create_dir_all(&root).map_err(|e| io(&root, e))?;
    // sandbox path -> host path; a later mount hides what an earlier one put at or below it.
    let mut mounts: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    // Host directories that are the sandbox's own (its root, tmpfs): created into freely.
    let mut owned: Vec<PathBuf> = vec![root.clone()];
    let mut n = 0;
    let mut fresh = |kind: &str| -> PathBuf {
        n += 1;
        staging.join(format!("{kind}-{n}"))
    };
    let mount = |mounts: &mut BTreeMap<String, PathBuf>,
                 order: &mut Vec<String>,
                 dest: &str,
                 host: PathBuf| {
        mounts.retain(|d, _| !under(d, dest));
        order.retain(|d| !under(d, dest));
        mounts.insert(dest.to_string(), host);
        order.push(dest.to_string());
    };
    // Where a sandbox path lives on the host now: under the innermost mount, else in the root.
    let host_of = |mounts: &BTreeMap<String, PathBuf>, dest: &str| -> PathBuf {
        let inner = mounts
            .iter()
            .filter(|(d, _)| under(dest, d))
            .max_by_key(|(d, _)| d.len());
        match inner {
            Some((d, h)) => join(h, rel(dest, d)),
            None => join(&root, dest.trim_start_matches('/')),
        }
    };
    let allowed: Vec<PathBuf> = allowed
        .iter()
        .map(|p| fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let check = |host: &Path| -> Result<()> {
        // Resolve on the host too: a guest symlink must not lead a bind out of the guest's view.
        let real = fs::canonicalize(host).unwrap_or_else(|_| host.to_path_buf());
        if allowed.iter().any(|a| real.starts_with(a)) {
            Ok(())
        } else {
            Err(Error::invalid(format!(
                "bwrap: {} is outside the guest",
                host.display()
            )))
        }
    };
    // Like bubblewrap, a mount point the sandbox owns is created before the mount (an empty directory,
    // or an empty file for a file), so the launcher never has to invent one. Only through real
    // directories: a symlink on the way could lead anywhere on the host.
    let point = |mounts: &BTreeMap<String, PathBuf>,
                 owned: &[PathBuf],
                 dest: &str,
                 dir: bool|
     -> Result<()> {
        let host = host_of(mounts, dest);
        let Some(base) = owned
            .iter()
            .filter(|o| host.starts_with(o))
            .max_by_key(|o| o.as_os_str().len())
        else {
            return Ok(());
        };
        if fs::symlink_metadata(&host).is_ok() {
            return Ok(());
        }
        let mut at = base.clone();
        let rest: Vec<_> = host
            .strip_prefix(base)
            .unwrap_or(Path::new(""))
            .components()
            .collect();
        for (i, c) in rest.iter().enumerate() {
            at.push(c);
            match fs::symlink_metadata(&at) {
                Ok(m) if m.is_dir() => continue,
                Ok(_) => return Ok(()),
                Err(_) if i + 1 < rest.len() => fs::create_dir(&at).map_err(|e| io(&at, e))?,
                Err(_) if dir => fs::create_dir(&at).map_err(|e| io(&at, e))?,
                Err(_) => {
                    fs::File::create(&at).map_err(|e| io(&at, e))?;
                }
            }
        }
        Ok(())
    };
    for op in ops {
        let dest_of = |d: &str| {
            normalize(d).ok_or_else(|| Error::invalid(format!("bwrap: {d:?} is not absolute")))
        };
        match op {
            Op::Bind { src, dest } => {
                let src = normalize(src)
                    .ok_or_else(|| Error::invalid(format!("bwrap: {src:?} is not absolute")))?;
                let dest = dest_of(dest)?;
                let host = launcher.to_host(&src);
                check(&host)?;
                point(&mounts, &owned, &dest, host.is_dir())?;
                mount(&mut mounts, &mut order, &dest, host);
                for (h, r) in launcher.below(&src) {
                    let d = if dest == "/" {
                        format!("/{r}")
                    } else {
                        format!("{dest}/{r}")
                    };
                    mount(&mut mounts, &mut order, &d, h);
                }
            }
            Op::Tmpfs { dest } => {
                let dest = dest_of(dest)?;
                let dir = fresh("tmpfs");
                fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
                point(&mounts, &owned, &dest, true)?;
                owned.push(dir.clone());
                mount(&mut mounts, &mut order, &dest, dir);
            }
            Op::Dir { dest } => {
                let dest = dest_of(dest)?;
                let host = host_of(&mounts, &dest);
                if host.is_dir() {
                    continue;
                }
                if owned.iter().any(|o| host.starts_with(o)) {
                    fs::create_dir_all(&host).map_err(|e| io(&host, e))?;
                } else {
                    // Inside a bind of a real directory, which must not change: an empty one over it.
                    let dir = fresh("dir");
                    fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
                    owned.push(dir.clone());
                    mount(&mut mounts, &mut order, &dest, dir);
                }
            }
            Op::Symlink { target, dest } => {
                let dest = dest_of(dest)?;
                let host = host_of(&mounts, &dest);
                if !owned.iter().any(|o| host.starts_with(o)) {
                    // bubblewrap would fail here too unless the link is already there.
                    if fs::symlink_metadata(&host).is_err() {
                        eprintln!("bwrap: warn: cannot create {dest} inside a bind");
                    }
                    continue;
                }
                if let Some(p) = host.parent() {
                    fs::create_dir_all(p).map_err(|e| io(p, e))?;
                }
                match fs::read_link(&host) {
                    Ok(t) if t == Path::new(target) => {}
                    _ => std::os::unix::fs::symlink(target, &host).map_err(|e| io(&host, e))?,
                }
            }
            Op::Data { dest, data } => {
                let dest = dest_of(dest)?;
                let file = fresh("data");
                fs::write(&file, data).map_err(|e| io(&file, e))?;
                point(&mounts, &owned, &dest, false)?;
                mount(&mut mounts, &mut order, &dest, file);
            }
        }
    }
    let mut binds: Vec<(PathBuf, String)> = l2s_targets(launcher)
        .into_iter()
        .filter(|(_, g)| !mounts.contains_key(g))
        .collect();
    binds.extend(order.iter().map(|d| (mounts[d].clone(), d.clone())));
    Ok(Plan { root, binds })
}

/// Under `--link2symlink` a hard link is a symlink to an absolute host path next to the first link of
/// its set, which can be anywhere in the guest (a bootc image's `/usr` links into
/// `/sysroot/ostree/repo`, a Flatpak checkout into `/var/lib/flatpak/repo`). The guest resolves them
/// because they sit under its rootfs; a sandbox that binds only `/usr` would not. So the guest's own
/// trees are also bound at their host paths, where nothing in a sandbox looks.
fn l2s_targets(launcher: &Launcher) -> Vec<(PathBuf, String)> {
    if !launcher
        .flags
        .iter()
        .any(|f| f == "--link2symlink" || f == "-l")
    {
        return Vec::new();
    }
    let system = |p: &Path| {
        p == Path::new("/") || ["/dev", "/proc", "/sys"].iter().any(|s| p.starts_with(s))
    };
    let mut dirs: Vec<PathBuf> = std::iter::once(&launcher.rootfs)
        .chain(launcher.binds.iter().map(|(h, _)| h))
        .filter_map(|h| std::fs::canonicalize(h).ok())
        .filter(|h| h.is_dir() && !system(h))
        .collect();
    dirs.sort();
    dirs.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        if !out.iter().any(|o| d.starts_with(o)) {
            out.push(d);
        }
    }
    out.into_iter()
        .filter_map(|d| {
            let g = d.to_str()?.to_string();
            Some((d, g))
        })
        .collect()
}

/// The descriptors this process has open above stderr.
fn open_fds() -> Vec<i32> {
    let mut fds: Vec<i32> = std::fs::read_dir("/proc/self/fd")
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .filter(|fd| *fd > 2)
                .collect()
        })
        .unwrap_or_default();
    fds.sort_unstable();
    // read_dir's own descriptor is gone by now.
    fds.retain(|fd| {
        rustix::io::fcntl_getfd(unsafe { rustix::fd::BorrowedFd::borrow_raw(*fd) }).is_ok()
    });
    fds
}

fn write_fd(fd: i32, text: &str) {
    use std::io::Write;
    use std::os::unix::io::FromRawFd;
    // SAFETY: bubblewrap's contract: the descriptor is ours to write and close.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    let _ = f.write_all(text.as_bytes());
}

/// `/usr/bin/bwrap` in the guest: parse, then have the host spawner start the sandbox and exit with its
/// status.
pub fn client(args: &[String]) -> Result<i32> {
    let inherited = open_fds();
    let resolve = |p: &str| -> Option<String> {
        std::fs::canonicalize(p)
            .ok()
            .map(|p| p.display().to_string())
    };
    let a = parse(args, &mut ProcessFds, &resolve)?;
    if a.version {
        println!("bubblewrap 0.11.0 (droidc {})", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if std::env::var_os("DROIDC_BWRAP_VERBOSE").is_some() {
        for w in &a.warnings {
            eprintln!("bwrap: warn: {w}");
        }
    }
    if a.command.is_empty() {
        return Err(Error::invalid("bwrap: No program specified"));
    }
    if let Some(fd) = a.block_fd {
        // Wait for a byte or for the other end to close, as bubblewrap does.
        use std::io::Read;
        use std::os::unix::io::FromRawFd;
        let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
        let _ = f.read(&mut [0u8; 1]);
    }
    let mut env: BTreeMap<String, String> = if a.clearenv {
        BTreeMap::new()
    } else {
        std::env::vars()
            .filter(|(k, _)| k != crate::spawn::SOCKET_ENV)
            .collect()
    };
    for (k, v) in &a.env {
        match v {
            Some(v) => env.insert(k.clone(), v.clone()),
            None => env.remove(k),
        };
    }
    let own: Vec<i32> = a
        .consumed_fds
        .iter()
        .copied()
        .chain(
            [a.info_fd, a.json_status_fd, a.sync_fd, a.block_fd]
                .into_iter()
                .flatten(),
        )
        .collect();
    let mut fds: Vec<i32> = inherited
        .into_iter()
        .filter(|fd| !own.contains(fd))
        .collect();
    if fds.len() > MAX_FDS {
        eprintln!(
            "bwrap: warn: passing only {MAX_FDS} of {} open descriptors",
            fds.len()
        );
        fds.truncate(MAX_FDS);
    }
    let cwd = a
        .chdir
        .clone()
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| "/".into());
    let sandbox = Sandbox {
        ops: a.ops,
        fds: fds.clone(),
        root: rustix::process::getuid().is_root(),
    };
    let req = crate::spawn::Request {
        argv: a.command,
        cwd,
        env,
        sandbox: Some(sandbox),
        via: None,
    };
    let (info_fd, json_fd) = (a.info_fd, a.json_status_fd);
    let status = crate::spawn::request("bwrap", &req, &fds, |pid| {
        if let Some(fd) = info_fd {
            write_fd(fd, &format!("{{\n    \"child-pid\": {pid}\n}}\n"));
        }
        if let Some(fd) = json_fd {
            // Kept open for the exit code.
            let _ = rustix::io::write(
                unsafe { rustix::fd::BorrowedFd::borrow_raw(fd) },
                format!("{{ \"child-pid\": {pid} }}\n").as_bytes(),
            );
        }
    })?;
    if let Some(fd) = json_fd {
        write_fd(fd, &format!("{{ \"exit-code\": {status} }}\n"));
    }
    // --sync-fd stays open until here: the caller waits for it to close.
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Table(HashMap<i32, Vec<u8>>);
    impl FdSource for Table {
        fn read_all(&mut self, fd: i32) -> Result<Vec<u8>> {
            self.0
                .remove(&fd)
                .ok_or_else(|| Error::invalid(format!("no fd {fd}")))
        }
        fn path_of(&mut self, fd: i32) -> Result<String> {
            Ok(format!("/fd/{fd}"))
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn exists_except_missing(p: &str) -> Option<String> {
        (!p.contains("missing")).then(|| p.to_string())
    }

    #[test]
    fn parses_what_flatpak_run_passes() {
        let args_fd =
            b"--ro-bind\0/var/lib/flatpak/runtime/x/files\0/usr\0--symlink\0usr/lib\0/lib\0\
--ro-bind-data\08\0/.flatpak-info\0--setenv\0FOO\0bar\0"
                .to_vec();
        let mut fds = Table(HashMap::from([
            (5, args_fd),
            (8, b"[Application]\nname=x\n".to_vec()),
            (9, vec![1, 2, 3]),
        ]));
        let a = parse(
            &s(&[
                "--args",
                "5",
                "--unshare-pid",
                "--die-with-parent",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--tmpfs",
                "/tmp",
                "--dir",
                "/run/user/1000",
                "--bind-try",
                "/missing",
                "/x",
                "--seccomp",
                "9",
                "--info-fd",
                "11",
                "--sync-fd",
                "12",
                "--chdir",
                "/app",
                "--unsetenv",
                "LD_PRELOAD",
                "--perms",
                "0700",
                "--lock-file",
                "/usr/.ref",
                "--",
                "/app/bin/x",
                "--flag",
            ]),
            &mut fds,
            &exists_except_missing,
        )
        .unwrap();
        assert_eq!(
            a.ops,
            vec![
                Op::Bind {
                    src: "/var/lib/flatpak/runtime/x/files".into(),
                    dest: "/usr".into()
                },
                Op::Symlink {
                    target: "usr/lib".into(),
                    dest: "/lib".into()
                },
                Op::Data {
                    dest: "/.flatpak-info".into(),
                    data: b"[Application]\nname=x\n".to_vec()
                },
                Op::Bind {
                    src: "/dev".into(),
                    dest: "/dev".into()
                },
                Op::Bind {
                    src: "/proc".into(),
                    dest: "/proc".into()
                },
                Op::Tmpfs {
                    dest: "/tmp".into()
                },
                Op::Dir {
                    dest: "/run/user/1000".into()
                },
            ]
        );
        assert_eq!(
            a.env,
            vec![
                ("FOO".to_string(), Some("bar".to_string())),
                ("LD_PRELOAD".to_string(), None)
            ]
        );
        assert_eq!(a.consumed_fds, vec![5, 8, 9]);
        assert_eq!((a.info_fd, a.sync_fd), (Some(11), Some(12)));
        assert_eq!(a.chdir.as_deref(), Some("/app"));
        assert_eq!(a.command, s(&["/app/bin/x", "--flag"]));
    }

    #[test]
    fn refuses_what_it_cannot_do() {
        let mut fds = Table(HashMap::new());
        assert!(parse(
            &s(&["--bind", "/missing", "/x", "true"]),
            &mut fds,
            &exists_except_missing
        )
        .is_err());
        assert!(parse(
            &s(&["--overlay-src", "/a", "true"]),
            &mut fds,
            &exists_except_missing
        )
        .is_err());
        assert!(parse(&s(&["--no-such-option"]), &mut fds, &exists_except_missing).is_err());
        assert!(parse(&s(&["--bind", "/a"]), &mut fds, &exists_except_missing).is_err());
        let a = parse(&s(&["--version"]), &mut fds, &exists_except_missing).unwrap();
        assert!(a.version);
        // the first word that is not an option starts the command
        let a = parse(
            &s(&["--clearenv", "sh", "--tmpfs", "/x"]),
            &mut fds,
            &exists_except_missing,
        )
        .unwrap();
        assert!(a.clearenv);
        assert_eq!(a.command, s(&["sh", "--tmpfs", "/x"]));
    }

    fn launcher(rootfs: &Path, extra: &Path) -> Launcher {
        Launcher::parse(&s(&[
            "/lib/libproot.so",
            "--kill-on-exit",
            "--link2symlink",
            "-0",
            "-r",
            rootfs.to_str().unwrap(),
            "-w",
            "/root",
            "-b",
            "/dev",
            "-b",
            "/proc",
            "-b",
            &format!("{}/var:/var", extra.display()),
            "-b",
            &format!("{}/run:/run", extra.display()),
            "-b",
            &format!(
                "{}/maxns:/proc/sys/user/max_user_namespaces",
                extra.display()
            ),
        ]))
        .unwrap()
    }

    #[test]
    fn the_launcher_translates_guest_paths() {
        let l = launcher(Path::new("/r/rootfs"), Path::new("/c"));
        assert_eq!(l.program, "/lib/libproot.so");
        assert_eq!(l.flags, s(&["--kill-on-exit", "--link2symlink"]));
        assert_eq!(
            l.to_host("/usr/bin/sh"),
            PathBuf::from("/r/rootfs/usr/bin/sh")
        );
        assert_eq!(
            l.to_host("/var/lib/flatpak"),
            PathBuf::from("/c/var/lib/flatpak")
        );
        assert_eq!(l.to_host("/var"), PathBuf::from("/c/var"));
        assert_eq!(l.to_host("/variable"), PathBuf::from("/r/rootfs/variable"));
        assert_eq!(l.to_host("/proc/self"), PathBuf::from("/proc/self"));
        assert_eq!(
            l.to_host("/proc/sys/user/max_user_namespaces"),
            PathBuf::from("/c/maxns")
        );
        assert_eq!(l.to_host("/"), PathBuf::from("/r/rootfs"));
        assert_eq!(normalize("/a/../b/./c/"), Some("/b/c".into()));
        assert_eq!(normalize("/../.."), Some("/".into()));
        assert_eq!(normalize("a"), None);
    }

    #[test]
    fn plans_a_sandbox_like_bubblewrap() {
        let d = tempfile::tempdir().unwrap();
        let rootfs = d.path().join("rootfs");
        let c = d.path().join("c");
        for p in [
            rootfs.join("usr/bin"),
            c.join("var/lib/flatpak/runtime/x/files/bin"),
            c.join("run/user/1000"),
        ] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(c.join("maxns"), "0\n").unwrap();
        let l = launcher(&rootfs, &c);
        let staging = d.path().join("staging");
        let allowed = vec![
            rootfs.clone(),
            c.clone(),
            PathBuf::from("/dev"),
            PathBuf::from("/proc"),
        ];
        let ops = vec![
            Op::Bind {
                src: "/var/lib/flatpak/runtime/x/files".into(),
                dest: "/usr".into(),
            },
            Op::Symlink {
                target: "usr/bin".into(),
                dest: "/bin".into(),
            },
            Op::Tmpfs {
                dest: "/tmp".into(),
            },
            Op::Dir {
                dest: "/tmp/sub".into(),
            },
            Op::Dir {
                dest: "/usr/new".into(),
            },
            Op::Data {
                dest: "/.flatpak-info".into(),
                data: b"x".to_vec(),
            },
            Op::Bind {
                src: "/proc".into(),
                dest: "/proc".into(),
            },
            Op::Bind {
                src: "/run/user/1000".into(),
                dest: "/run/user/1000".into(),
            },
            // a later mount hides an earlier one at or below it
            Op::Tmpfs {
                dest: "/run".into(),
            },
        ];
        let p = plan(&l, &ops, &staging, &allowed).unwrap();
        assert_eq!(p.root, staging.join("root"));
        // --link2symlink: the guest's trees at their host paths first, for the chains
        let canon = |p: PathBuf| std::fs::canonicalize(p).unwrap();
        let l2s = [
            canon(c.join("run")),
            canon(c.join("var")),
            canon(rootfs.clone()),
        ];
        let n = l2s.len();
        for (b, want) in p.binds[..n].iter().zip(&l2s) {
            assert_eq!((&b.0, b.1.as_str()), (want, want.to_str().unwrap()));
        }
        let dests: Vec<&str> = p.binds[n..].iter().map(|(_, g)| g.as_str()).collect();
        assert_eq!(
            dests,
            [
                "/usr",
                "/tmp",
                "/usr/new",
                "/.flatpak-info",
                "/proc",
                "/proc/sys/user/max_user_namespaces",
                "/run"
            ]
        );
        assert_eq!(p.binds[n].0, c.join("var/lib/flatpak/runtime/x/files"));
        assert_eq!(p.binds[n + 4].0, PathBuf::from("/proc"));
        assert_eq!(p.binds[n + 5].0, c.join("maxns"));
        assert_eq!(
            std::fs::read_link(staging.join("root/bin")).unwrap(),
            PathBuf::from("usr/bin")
        );
        assert!(
            p.binds[n + 1].0.join("sub").is_dir(),
            "--dir inside a tmpfs"
        );
        // mount points are made in the sandbox's own root, as bubblewrap makes them
        assert!(staging.join("root/usr").is_dir());
        assert!(staging.join("root/proc").is_dir());
        assert!(staging.join("root/.flatpak-info").is_file());
        assert!(staging.join("root/run/user/1000").is_dir());
        assert_eq!(std::fs::read(&p.binds[n + 3].0).unwrap(), b"x");
        // the runtime itself was not touched
        assert!(!c.join("var/lib/flatpak/runtime/x/files/new").exists());
        let argv = p.argv(&l, "/app", false);
        assert_eq!(
            p.host("/usr/bin/env"),
            c.join("var/lib/flatpak/runtime/x/files/bin/env")
        );
        assert_eq!(p.host("/etc/passwd"), staging.join("root/etc/passwd"));
        assert_eq!(
            argv[..3],
            s(&["/lib/libproot.so", "--kill-on-exit", "--link2symlink"])
        );
        assert!(argv.windows(2).any(|w| w[0] == "-w" && w[1] == "/app"));
        assert!(!argv.contains(&"-0".to_string()));

        // whole-root binds bring the launcher's binds along
        let p = plan(
            &l,
            &[Op::Bind {
                src: "/".into(),
                dest: "/".into(),
            }],
            &d.path().join("staging2"),
            &allowed,
        )
        .unwrap();
        let dests: Vec<&str> = p.binds[n..].iter().map(|(_, g)| g.as_str()).collect();
        assert_eq!(
            dests,
            [
                "/",
                "/dev",
                "/proc",
                "/var",
                "/run",
                "/proc/sys/user/max_user_namespaces"
            ]
        );
    }

    #[test]
    fn a_bind_cannot_leave_the_guest() {
        let d = tempfile::tempdir().unwrap();
        let rootfs = d.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("tmp")).unwrap();
        let secret = d.path().join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        // a guest symlink with a host target
        std::os::unix::fs::symlink(&secret, rootfs.join("tmp/x")).unwrap();
        let l = Launcher::parse(&s(&["proot", "-r", rootfs.to_str().unwrap()])).unwrap();
        let err = plan(
            &l,
            &[Op::Bind {
                src: "/tmp/x".into(),
                dest: "/x".into(),
            }],
            &d.path().join("st"),
            std::slice::from_ref(&rootfs),
        )
        .unwrap_err();
        assert!(err.to_string().contains("outside the guest"), "{err}");
    }
}

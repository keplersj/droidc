//! Process accounting for the fake cgroup tree (`shim/cgshim.c`).
//!
//! `systemd --user` runs on a plain directory that pretends to be cgroup2. The kernel would keep
//! `cgroup.procs` and `cgroup.events` current; here nothing does, so a transient scope or a
//! forking service never looks finished and `KillMode=control-group` has nothing to kill. This
//! helper reproduces the three behaviours systemd relies on:
//!
//! * membership: pids systemd appends to a `cgroup.procs` are owned by that directory (a later
//!   append elsewhere moves them, like the kernel), their children inherit it, and orphans that were
//!   re-parented to us are matched to a directory by the `INVOCATION_ID` systemd put in their
//!   environment, which survives double forks and `setsid`;
//! * `populated`: `cgroup.events` flips between `populated 1` and `populated 0`, written in place so
//!   systemd's inotify watch fires, for the directory and all its ancestors;
//! * `cgroup.kill`: writing `1` kills everything owned below that directory.
//!
//! It polls `/proc`: simple and enough for a desktop session's few hundred processes. Limits: a
//! daemon that clears its environment *and* double forks escapes (the same hole as with any
//! process-group scheme), and `cgroup.procs` is rewritten by rename, so an append racing the rewrite
//! can be lost for one tick (systemd appends right after opening, so the window is tiny).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Which processes to account: our descendants (the normal case), or any (tests).
#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Descendants(i32),
    Any,
}

#[derive(Debug, Clone)]
struct Proc {
    ppid: i32,
    live: bool,
}

#[derive(Debug)]
pub struct Cgroups {
    root: PathBuf,
    scope: Scope,
    /// pid -> cgroup directory it was last written into.
    owner: HashMap<i32, PathBuf>,
    /// pids already seen in a directory's `cgroup.procs`, so a re-read does not re-claim them.
    seen: HashMap<PathBuf, HashSet<i32>>,
    /// `INVOCATION_ID` -> directory, learned from owned processes.
    invocations: HashMap<String, PathBuf>,
    /// Last `populated` value written per directory.
    populated: HashMap<PathBuf, bool>,
    /// Directories that have held a process, so a fresh one is not reported empty before its first
    /// process is attached (the kernel only fires an event on a change).
    ever_populated: HashSet<PathBuf>,
    /// Directories that looked empty on the previous tick: a process forked just now may not be
    /// attributed yet, so emptiness has to last two ticks.
    looked_empty: HashSet<PathBuf>,
    /// `INVOCATION_ID` of processes that have one, read once (a syscall is slow under proot). A process
    /// without one is read again: during `exec` its environment is not yet the final one.
    env_ids: HashMap<i32, String>,
}

fn read_proc(pid: i32) -> Option<Proc> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // pid (comm) S ppid ...; comm may contain spaces and parentheses
    let rest = &stat[stat.rfind(')')? + 2..];
    let mut f = rest.split_whitespace();
    let state = f.next()?;
    let ppid = f.next()?.parse().ok()?;
    Some(Proc {
        ppid,
        live: !matches!(state, "Z" | "X"),
    })
}

fn invocation_id(pid: i32) -> Option<String> {
    let env = fs::read(format!("/proc/{pid}/environ")).ok()?;
    env.split(|&b| b == 0)
        .find_map(|kv| kv.strip_prefix(b"INVOCATION_ID="))
        .map(|v| String::from_utf8_lossy(v).into_owned())
}

fn cached_invocation_id(cache: &mut HashMap<i32, String>, pid: i32) -> Option<String> {
    if let Some(id) = cache.get(&pid) {
        return Some(id.clone());
    }
    let id = invocation_id(pid)?;
    cache.insert(pid, id.clone());
    Some(id)
}

fn all_pids() -> Vec<i32> {
    fs::read_dir("/proc")
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn cgroup_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(e.path());
            }
        }
        out.push(d);
    }
    out
}

impl Cgroups {
    pub fn new(root: impl Into<PathBuf>, scope: Scope) -> Cgroups {
        Cgroups {
            root: root.into(),
            scope,
            owner: HashMap::new(),
            seen: HashMap::new(),
            invocations: HashMap::new(),
            populated: HashMap::new(),
            ever_populated: HashSet::new(),
            looked_empty: HashSet::new(),
            env_ids: HashMap::new(),
        }
    }

    /// Poll forever (the thread ends with the process).
    pub fn run(mut self, every: Duration) {
        loop {
            self.tick();
            std::thread::sleep(every);
        }
    }

    pub fn tick(&mut self) {
        let started = std::time::Instant::now();
        self.tick_inner();
        if std::env::var_os("DROIDC_CG_DEBUG").is_some() {
            eprintln!(
                "cg: tick {}ms owners={} invocations={} cgroups={}",
                started.elapsed().as_millis(),
                self.owner.len(),
                self.invocations.len(),
                self.populated.len()
            );
        }
    }

    fn tick_inner(&mut self) {
        let procs: HashMap<i32, Proc> = all_pids()
            .into_iter()
            .filter_map(|p| read_proc(p).map(|i| (p, i)))
            .collect();
        let dirs = cgroup_dirs(&self.root);
        self.claim_written_pids(&dirs, &procs);
        self.kill_requested(&dirs);
        let members = self.assign(&procs);
        self.write_procs(&dirs, &members);
        self.write_events(&dirs, &members);
    }

    fn in_scope(&self, pid: i32, procs: &HashMap<i32, Proc>) -> bool {
        match self.scope {
            Scope::Any => true,
            Scope::Descendants(top) => {
                let mut p = pid;
                for _ in 0..64 {
                    if p == top {
                        return true;
                    }
                    match procs.get(&p) {
                        Some(i) if i.ppid > 1 => p = i.ppid,
                        _ => return false,
                    }
                }
                false
            }
        }
    }

    /// pids newly appended to some `cgroup.procs` move into that directory.
    fn claim_written_pids(&mut self, dirs: &[PathBuf], procs: &HashMap<i32, Proc>) {
        for d in dirs {
            let Ok(text) = fs::read_to_string(d.join("cgroup.procs")) else {
                continue;
            };
            let now: HashSet<i32> = text
                .split_whitespace()
                .filter_map(|t| t.parse().ok())
                .collect();
            let seen = self.seen.entry(d.clone()).or_default();
            for &pid in now.difference(&seen.clone()) {
                let known = procs.contains_key(&pid);
                if std::env::var_os("DROIDC_CG_DEBUG").is_some() {
                    eprintln!("cg: {} lists pid {pid} (in /proc: {known})", d.display());
                }
                if known {
                    self.owner.insert(pid, d.clone());
                }
            }
            *seen = now;
        }
        self.owner
            .retain(|p, _| procs.get(p).is_some_and(|i| i.live));
    }

    fn kill_requested(&mut self, dirs: &[PathBuf]) {
        for d in dirs {
            let f = d.join("cgroup.kill");
            if fs::read_to_string(&f)
                .map(|s| s.trim() == "1")
                .unwrap_or(false)
            {
                for (pid, dir) in &self.owner {
                    if dir.starts_with(d) {
                        if let Some(p) = rustix::process::Pid::from_raw(*pid) {
                            let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
                        }
                    }
                }
                let _ = fs::write(&f, "");
            }
        }
    }

    /// Live pid -> directory: owned, inherited from an owned ancestor, or matched by `INVOCATION_ID`.
    fn assign(&mut self, procs: &HashMap<i32, Proc>) -> HashMap<i32, PathBuf> {
        // Only a service's processes carry an id of its own. A scope's (or the manager's, in
        // init.scope) environment is whatever its caller had, which names some other unit.
        self.env_ids.retain(|p, _| procs.contains_key(p));
        for (pid, dir) in &self.owner {
            if dir.extension().is_some_and(|e| e == "service") {
                if let Some(id) = cached_invocation_id(&mut self.env_ids, *pid) {
                    self.invocations.insert(id, dir.clone());
                }
            }
        }
        let mut members: HashMap<i32, PathBuf> = HashMap::new();
        let mut pids: Vec<i32> = procs.keys().copied().collect();
        pids.sort_unstable();
        for pid in pids {
            if !procs[&pid].live || !self.in_scope(pid, procs) {
                continue;
            }
            if let Some(dir) = self.owner.get(&pid) {
                members.insert(pid, dir.clone());
                continue;
            }
            // Nearest owned ancestor, except the manager itself (init.scope): everything it starts is
            // appended explicitly, so a process that merely hangs under it was re-parented to it.
            let mut p = procs[&pid].ppid;
            let mut found = None;
            for _ in 0..64 {
                if let Some(d) = self.owner.get(&p) {
                    if d.file_name().is_none_or(|n| n != "init.scope") {
                        found = Some(d.clone());
                    }
                    break;
                }
                match procs.get(&p) {
                    Some(i) if i.ppid > 1 => p = i.ppid,
                    _ => break,
                }
            }
            // an orphan: the environment still names its unit
            let found = found
                .or_else(|| invocation_id(pid).and_then(|id| self.invocations.get(&id).cloned()));
            if let Some(d) = found {
                members.insert(pid, d);
            }
        }
        members
    }

    fn write_procs(&self, dirs: &[PathBuf], members: &HashMap<i32, PathBuf>) {
        for d in dirs {
            let mut mine: Vec<i32> = members
                .iter()
                .filter(|(_, dir)| *dir == d)
                .map(|(p, _)| *p)
                .collect();
            mine.sort_unstable();
            let body: String = mine.iter().map(|p| format!("{p}\n")).collect();
            let path = d.join("cgroup.procs");
            if !path.exists() || fs::read_to_string(&path).is_ok_and(|c| c == body) {
                continue;
            }
            let tmp = d.join(".cgroup.procs.new");
            if fs::write(&tmp, &body).is_ok() && fs::rename(&tmp, &path).is_err() {
                let _ = fs::remove_file(&tmp);
            }
        }
    }

    fn write_events(&mut self, dirs: &[PathBuf], members: &HashMap<i32, PathBuf>) {
        let mut empty_now = HashSet::new();
        for d in dirs {
            if d == &self.root {
                continue;
            }
            let mut populated = members.values().any(|m| m.starts_with(d));
            if populated {
                self.ever_populated.insert(d.clone());
                self.looked_empty.remove(d);
            } else if !self.ever_populated.contains(d) {
                continue; // never held a process yet: nothing to report
            } else if self.populated.get(d) != Some(&false) {
                // newly empty: believe it only once it stayed empty for a whole tick
                if self.looked_empty.contains(d) {
                    empty_now.insert(d.clone());
                } else {
                    self.looked_empty.insert(d.clone());
                    populated = true;
                }
            }
            if self.populated.get(d) == Some(&populated) {
                continue;
            }
            let events = d.join("cgroup.events");
            if !events.exists() {
                continue;
            }
            // Same length either way, written in place: systemd watches the inode.
            let body = format!("populated {}\nfrozen 0\n", populated as u8);
            use std::io::{Seek, Write};
            let wrote = fs::OpenOptions::new()
                .write(true)
                .open(&events)
                .and_then(|mut f| {
                    f.seek(std::io::SeekFrom::Start(0))?;
                    f.write_all(body.as_bytes())
                });
            if wrote.is_ok() {
                self.populated.insert(d.clone(), populated);
            }
        }
        for d in empty_now {
            self.looked_empty.remove(&d);
        }
        self.populated.retain(|d, _| dirs.contains(d));
        self.ever_populated.retain(|d| dirs.contains(d));
        self.looked_empty.retain(|d| dirs.contains(d));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    fn cgroup(root: &Path, name: &str) -> PathBuf {
        let d = root.join(name);
        fs::create_dir_all(&d).unwrap();
        for f in ["cgroup.procs", "cgroup.kill"] {
            fs::write(d.join(f), "").unwrap();
        }
        fs::write(d.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        d
    }

    fn append(d: &Path, pid: u32) {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(d.join("cgroup.procs"))
            .unwrap();
        writeln!(f, "{pid}").unwrap();
    }

    fn populated(d: &Path) -> bool {
        fs::read_to_string(d.join("cgroup.events"))
            .unwrap()
            .starts_with("populated 1")
    }

    fn sleeper(inv: Option<&str>) -> Child {
        let mut c = Command::new("sleep");
        c.arg("30");
        // an outer systemd unit's own INVOCATION_ID must not leak in and claim the whole session
        match inv {
            Some(i) => c.env("INVOCATION_ID", i),
            None => c.env_remove("INVOCATION_ID"),
        };
        c.spawn().unwrap()
    }

    fn reap(mut c: Child) {
        let _ = c.kill();
        let _ = c.wait();
    }

    #[test]
    fn a_new_cgroup_is_not_reported_empty_before_its_first_process() {
        let _lock = crate::supervise::CHILD_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let r = tempfile::tempdir().unwrap();
        let a = cgroup(r.path(), "a.slice");
        let mut cg = Cgroups::new(r.path(), Scope::Any);
        for _ in 0..3 {
            cg.tick();
        }
        assert!(populated(&a), "still the initial 'populated 1'");
        let c = sleeper(None);
        append(&a, c.id());
        cg.tick();
        assert!(populated(&a));
        reap(c);
    }

    #[test]
    fn populated_follows_the_processes() {
        let _lock = crate::supervise::CHILD_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let r = tempfile::tempdir().unwrap();
        let a = cgroup(r.path(), "a.slice");
        let mut cg = Cgroups::new(r.path(), Scope::Any);
        let c = sleeper(None);
        append(&a, c.id());
        cg.tick();
        assert!(populated(&a));
        assert_eq!(
            fs::read_to_string(a.join("cgroup.procs")).unwrap(),
            format!("{}\n", c.id())
        );
        reap(c);
        cg.tick();
        assert!(populated(&a), "one empty tick is not believed");
        cg.tick();
        assert!(!populated(&a));
        assert_eq!(fs::read_to_string(a.join("cgroup.procs")).unwrap(), "");
    }

    #[test]
    fn children_inherit_and_nested_cgroups_count_for_their_parents() {
        let _lock = crate::supervise::CHILD_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let r = tempfile::tempdir().unwrap();
        let parent = cgroup(r.path(), "app.slice");
        let child = cgroup(&parent, "app-x.scope");
        let mut cg = Cgroups::new(r.path(), Scope::Any);
        // a shell (owned) whose child `sleep` was never written anywhere
        let sh = Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .env_remove("INVOCATION_ID")
            .spawn()
            .unwrap();
        append(&child, sh.id());
        std::thread::sleep(Duration::from_millis(200));
        cg.tick();
        let procs = fs::read_to_string(child.join("cgroup.procs")).unwrap();
        assert_eq!(procs.lines().count(), 2, "shell and its sleep: {procs:?}");
        assert!(populated(&child) && populated(&parent));
        // kill the whole tree through cgroup.kill
        fs::write(parent.join("app-x.scope/cgroup.kill"), "1").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        reap(sh);
        for _ in 0..20 {
            cg.tick();
            if !populated(&child) && !populated(&parent) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!populated(&child) && !populated(&parent));
    }

    #[test]
    fn orphans_are_found_by_invocation_id() {
        let _lock = crate::supervise::CHILD_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let r = tempfile::tempdir().unwrap();
        let unit = cgroup(r.path(), "unit.service");
        let mut cg = Cgroups::new(r.path(), Scope::Any);
        let id = "0123456789abcdef0123456789abcdef";
        let main = sleeper(Some(id));
        append(&unit, main.id());
        cg.tick();
        // a double fork: the middle shell exits at once, the daemon is re-parented away
        let pidfile = r.path().join("daemon.pid");
        let sh = Command::new("sh")
            .args(["-c", &format!("sleep 30 & echo $! > {}", pidfile.display())])
            .env("INVOCATION_ID", id)
            .process_group(0)
            .spawn()
            .unwrap();
        let mut sh = sh;
        sh.wait().unwrap();
        let daemon: i32 = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        cg.tick();
        let procs = fs::read_to_string(unit.join("cgroup.procs")).unwrap();
        assert!(
            procs.lines().any(|l| l == daemon.to_string()),
            "orphan {daemon} missing from {procs:?}"
        );
        // the main process going away does not end the unit while the daemon lives
        reap(main);
        cg.tick();
        assert!(populated(&unit));
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(daemon).unwrap(),
            rustix::process::Signal::KILL,
        )
        .unwrap();
        // the kernel needs a moment to turn the killed process into a zombie
        for _ in 0..40 {
            cg.tick();
            if !populated(&unit) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!populated(&unit));
    }

    #[test]
    fn orphans_reparented_to_the_manager_are_not_the_managers() {
        let _lock = crate::supervise::CHILD_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let r = tempfile::tempdir().unwrap();
        let init = cgroup(r.path(), "init.scope");
        let unit = cgroup(r.path(), "unit.service");
        let mut cg = Cgroups::new(r.path(), Scope::Any);
        let id = "fedcba9876543210fedcba9876543210";
        // the "manager": owned by init.scope, and it has the same stale id in its environment
        // as the outer world; its child is an orphan of `unit.service`
        let manager = Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .env("INVOCATION_ID", "outer")
            .spawn()
            .unwrap();
        append(&init, manager.id());
        let main = sleeper(Some(id));
        append(&unit, main.id());
        let orphan = Command::new("sleep")
            .arg("30")
            .env("INVOCATION_ID", id)
            .spawn()
            .unwrap();
        // pretend the orphan hangs under the manager: only the id says it belongs to the unit
        cg.tick();
        let u = fs::read_to_string(unit.join("cgroup.procs")).unwrap();
        assert!(u.lines().any(|l| l == orphan.id().to_string()), "{u:?}");
        let i = fs::read_to_string(init.join("cgroup.procs")).unwrap();
        assert!(!i.lines().any(|l| l == orphan.id().to_string()), "{i:?}");
        reap(orphan);
        reap(main);
        reap(manager);
    }

    #[test]
    fn a_pid_written_elsewhere_moves() {
        let _lock = crate::supervise::CHILD_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let r = tempfile::tempdir().unwrap();
        let a = cgroup(r.path(), "a");
        let b = cgroup(r.path(), "b");
        let mut cg = Cgroups::new(r.path(), Scope::Any);
        let c = sleeper(None);
        append(&a, c.id());
        cg.tick();
        append(&b, c.id());
        cg.tick();
        cg.tick();
        assert!(!populated(&a) && populated(&b));
        reap(c);
    }
}

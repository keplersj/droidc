//! `Condition*=` and `Assert*=` from `[Unit]` (`systemd.unit(5)`, "Conditions and Asserts").
//!
//! Each assignment may start with `|` (a triggering condition) and then `!` (negated), in that order.
//! An empty assignment resets the list of that kind. A unit's conditions pass when every
//! non-triggering one passes and, if there are triggering ones, at least one of them does. A failed
//! condition skips the unit quietly; a failed assert fails the start.
//!
//! The checks are evaluated against a [`Host`], which the manager fills in from the guest. Checks the
//! stand-in can't answer under droidc pass, with a [`Notice`], so a unit isn't skipped for a reason
//! that has nothing to do with it.

use std::fs;
use std::path::Path;

use crate::loader::LoadedUnit;
use crate::options::Notice;

/// Facts conditions are checked against.
#[derive(Clone, Debug, Default)]
pub struct Host {
    pub user_name: Option<String>,
    pub uid: Option<u32>,
    /// Names and ids of the groups the manager runs as.
    pub groups: Vec<String>,
    pub gids: Vec<u32>,
    pub environment: Vec<(String, String)>,
    pub kernel_command_line: Option<String>,
    pub first_boot: bool,
    pub needs_update: bool,
    pub architecture: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub key: String,
    pub value: String,
    pub trigger: bool,
    pub negate: bool,
    pub assert: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    /// A condition failed: the unit is skipped. Holds the check that decided it.
    ConditionFailed(Check),
    /// An assert failed: the start fails.
    AssertFailed(Check),
}

/// The checks of `unit`, after empty assignments reset their kind.
pub fn checks(unit: &LoadedUnit) -> Vec<Check> {
    let mut keys: Vec<&str> = Vec::new();
    for a in &unit.assignments {
        if a.section == "Unit"
            && (a.key.starts_with("Condition") || a.key.starts_with("Assert"))
            && !keys.contains(&a.key.as_str())
        {
            keys.push(&a.key);
        }
    }
    let mut out = Vec::new();
    for key in keys {
        for a in unit.list("Unit", key) {
            let assert = key.starts_with("Assert");
            let mut v = a.value.as_str();
            let trigger = v.starts_with('|');
            if trigger {
                v = v[1..].trim_start();
            }
            let negate = v.starts_with('!');
            if negate {
                v = v[1..].trim_start();
            }
            out.push(Check {
                key: key.to_string(),
                value: v.to_string(),
                trigger,
                negate,
                assert,
            });
        }
    }
    out
}

/// Evaluates `unit`'s conditions and asserts. Notices are added for checks that couldn't be answered.
pub fn evaluate(unit: &LoadedUnit, host: &Host, notices: &mut Vec<Notice>) -> Outcome {
    let all = checks(unit);
    for (assert, fail) in [
        (false, Outcome::ConditionFailed as fn(Check) -> Outcome),
        (true, Outcome::AssertFailed),
    ] {
        let group: Vec<&Check> = all.iter().filter(|c| c.assert == assert).collect();
        let mut any_trigger = None;
        let mut first_failed_trigger = None;
        for c in &group {
            let ok = test(c, host, unit, notices) != c.negate;
            if c.trigger {
                if ok {
                    any_trigger = Some(true);
                } else {
                    any_trigger.get_or_insert(false);
                    first_failed_trigger.get_or_insert((*c).clone());
                }
            } else if !ok {
                return fail((*c).clone());
            }
        }
        if any_trigger == Some(false) {
            return fail(first_failed_trigger.expect("a failed trigger was recorded"));
        }
    }
    Outcome::Pass
}

/// The check's raw result, before negation.
fn test(c: &Check, host: &Host, unit: &LoadedUnit, notices: &mut Vec<Notice>) -> bool {
    let kind = c
        .key
        .trim_start_matches("Condition")
        .trim_start_matches("Assert");
    let v = c.value.as_str();
    let p = Path::new(v);
    match kind {
        "PathExists" => fs::symlink_metadata(p).is_ok(),
        "PathExistsGlob" => glob_any(v),
        "PathIsDirectory" => p.is_dir(),
        "PathIsSymbolicLink" => fs::symlink_metadata(p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "PathIsMountPoint" => is_mount_point(p),
        "PathIsReadWrite" => is_read_write(p),
        "DirectoryNotEmpty" => fs::read_dir(p)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false),
        "FileNotEmpty" => fs::metadata(p)
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false),
        "FileIsExecutable" => is_executable(p),
        "User" => match v {
            "@system" => host.uid.is_some_and(|u| u < 1000),
            _ => {
                host.user_name.as_deref() == Some(v)
                    || v.parse::<u32>().is_ok_and(|u| host.uid == Some(u))
            }
        },
        "Group" => {
            host.groups.iter().any(|g| g == v)
                || v.parse().is_ok_and(|g: u32| host.gids.contains(&g))
        }
        "Environment" => match v.split_once('=') {
            Some((k, val)) => host.environment.iter().any(|(ek, ev)| ek == k && ev == val),
            None => host.environment.iter().any(|(ek, _)| ek == v),
        },
        "KernelCommandLine" => {
            let cmdline = host.kernel_command_line.as_deref().unwrap_or("");
            cmdline.split_whitespace().any(|w| {
                if v.contains('=') {
                    w == v
                } else {
                    w == v || w.split_once('=').is_some_and(|(k, _)| k == v)
                }
            })
        }
        "FirstBoot" => crate::syntax::parse_bool(v).is_some_and(|b| b == host.first_boot),
        "NeedsUpdate" => host.needs_update,
        "Architecture" => host.architecture.as_deref() == Some(v),
        "Virtualization" => virtualization(v),
        // No LSMs, capabilities or cgroup controllers are visible to the guest under droidc.
        "Security" | "Capability" | "ControlGroupController" => false,
        "Host" | "KernelVersion" | "Firmware" | "CPUs" | "CPUFeature" | "Memory" | "OSRelease"
        | "ACPower" | "Credential" | "MemoryPressure" | "CPUPressure" | "IOPressure" => {
            notices.push(Notice::new(
                &unit.id,
                &c.key,
                v,
                "not checked under droidc, assuming it holds",
            ));
            !c.negate
        }
        _ => {
            notices.push(Notice::new(
                &unit.id,
                &c.key,
                v,
                "not recognised by droidc, assuming it holds",
            ));
            !c.negate
        }
    }
}

/// `ConditionVirtualization=` as droidc answers it: the guest is a container that systemd has no name
/// for (`container-other`), never a VM.
fn virtualization(v: &str) -> bool {
    match v {
        "container" | "container-other" => true,
        "vm" | "private-users" => false,
        other => crate::syntax::parse_bool(other).unwrap_or(false),
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn is_read_write(p: &Path) -> bool {
    // Under proot the guest's mounts aren't real; a path counts as read-write when it can be written.
    fs::metadata(p)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false)
}

fn is_mount_point(p: &Path) -> bool {
    let Ok(mounts) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let target = p.display().to_string();
    mounts
        .lines()
        .any(|l| l.split(' ').nth(4) == Some(target.as_str()))
}

/// A small `*`/`?` glob over one directory level per component, enough for unit conditions.
fn glob_any(pattern: &str) -> bool {
    fn matches(pat: &[u8], s: &[u8]) -> bool {
        match (pat.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => matches(&pat[1..], s) || (!s.is_empty() && matches(pat, &s[1..])),
            (Some(b'?'), Some(_)) => matches(&pat[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => matches(&pat[1..], &s[1..]),
            _ => false,
        }
    }
    let mut paths = vec![std::path::PathBuf::from("/")];
    for comp in pattern.split('/').filter(|c| !c.is_empty()) {
        let mut next = Vec::new();
        for base in &paths {
            if comp.contains(['*', '?']) {
                let Ok(rd) = fs::read_dir(base) else { continue };
                for e in rd.flatten() {
                    if matches(comp.as_bytes(), e.file_name().as_encoded_bytes()) {
                        next.push(e.path());
                    }
                }
            } else {
                let p = base.join(comp);
                if fs::symlink_metadata(&p).is_ok() {
                    next.push(p);
                }
            }
        }
        paths = next;
    }
    !paths.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::testdir::TempDir;
    use crate::loader::Loader;
    use crate::paths::SearchPath;

    fn unit_with(t: &TempDir, body: &str) -> LoadedUnit {
        t.write(
            "units/a.service",
            &format!("[Unit]\n{body}\n[Service]\nExecStart=/x\n"),
        );
        Loader::new(SearchPath::from_dirs([t.path().join("units")]))
            .load_str("a.service")
            .unwrap()
    }

    #[test]
    fn prefixes_and_resets() {
        let t = TempDir::new();
        let u = unit_with(&t, "ConditionPathExists=|!/a\nConditionPathExists=\nConditionPathExists=| ! /b\nAssertUser=k");
        let c = checks(&u);
        assert_eq!(c.len(), 2);
        assert_eq!(
            (c[0].value.as_str(), c[0].trigger, c[0].negate, c[0].assert),
            ("/b", true, true, false)
        );
        assert!(c[1].assert);
    }

    #[test]
    fn paths() {
        let t = TempDir::new();
        let f = t.write("f", "x");
        let empty = t.write("e", "");
        let d = t.path().join("d");
        fs::create_dir(&d).unwrap();
        let u = unit_with(
            &t,
            &format!(
                "ConditionPathExists={f}\nConditionPathIsDirectory={d}\nConditionFileNotEmpty={f}\nConditionPathExists=!{}/nope\nConditionDirectoryNotEmpty=!{d}",
                t.path().display(),
                f = f.display(),
                d = d.display()
            ),
        );
        assert_eq!(evaluate(&u, &Host::default(), &mut vec![]), Outcome::Pass);

        let u = unit_with(&t, &format!("ConditionFileNotEmpty={}", empty.display()));
        assert!(
            matches!(evaluate(&u, &Host::default(), &mut vec![]), Outcome::ConditionFailed(c) if c.key == "ConditionFileNotEmpty")
        );
    }

    #[test]
    fn triggers_need_one() {
        let t = TempDir::new();
        let f = t.write("f", "x");
        let u = unit_with(
            &t,
            &format!(
                "ConditionPathExists=|/nope\nConditionPathExists=|{}",
                f.display()
            ),
        );
        assert_eq!(evaluate(&u, &Host::default(), &mut vec![]), Outcome::Pass);
        let u = unit_with(
            &t,
            "ConditionPathExists=|/nope\nConditionPathExists=|/nope2",
        );
        assert!(
            matches!(evaluate(&u, &Host::default(), &mut vec![]), Outcome::ConditionFailed(c) if c.value == "/nope")
        );
    }

    #[test]
    fn asserts_fail_the_start() {
        let t = TempDir::new();
        let u = unit_with(&t, "AssertPathExists=/nope");
        assert!(matches!(
            evaluate(&u, &Host::default(), &mut vec![]),
            Outcome::AssertFailed(_)
        ));
        // A failed condition wins over a failed assert: the unit is skipped, not failed.
        let u = unit_with(&t, "AssertPathExists=/nope\nConditionPathExists=/nope");
        assert!(matches!(
            evaluate(&u, &Host::default(), &mut vec![]),
            Outcome::ConditionFailed(_)
        ));
    }

    #[test]
    fn host_facts() {
        let t = TempDir::new();
        let host = Host {
            user_name: Some("k".into()),
            uid: Some(10123),
            groups: vec!["wheel".into()],
            environment: vec![("container".into(), "droidc".into())],
            kernel_command_line: Some("quiet root=/dev/x rw".into()),
            first_boot: true,
            ..Default::default()
        };
        let u = unit_with(
            &t,
            "ConditionUser=k\nConditionUser=!@system\nConditionGroup=wheel\nConditionEnvironment=container=droidc\nConditionEnvironment=container\nConditionKernelCommandLine=root\nConditionKernelCommandLine=quiet\nConditionKernelCommandLine=!root=/dev/y\nConditionFirstBoot=yes\nConditionNeedsUpdate=!/etc",
        );
        assert_eq!(evaluate(&u, &host, &mut vec![]), Outcome::Pass);
    }

    #[test]
    fn virtualization_is_an_unnamed_container() {
        let t = TempDir::new();
        let u = unit_with(
            &t,
            "ConditionVirtualization=container\nConditionVirtualization=yes\nConditionVirtualization=!vm\nConditionVirtualization=!docker",
        );
        assert_eq!(evaluate(&u, &Host::default(), &mut vec![]), Outcome::Pass);
    }

    #[test]
    fn no_lsm_caps_or_cgroups() {
        let t = TempDir::new();
        let u = unit_with(&t, "ConditionSecurity=!selinux\nConditionCapability=!CAP_SYS_ADMIN\nConditionControlGroupController=!memory");
        assert_eq!(evaluate(&u, &Host::default(), &mut vec![]), Outcome::Pass);
    }

    #[test]
    fn unanswerable_checks_pass_with_a_notice() {
        let t = TempDir::new();
        let u = unit_with(
            &t,
            "ConditionACPower=true\nConditionKernelVersion=!<4.0\nConditionMadeUp=1",
        );
        let mut notices = Vec::new();
        assert_eq!(evaluate(&u, &Host::default(), &mut notices), Outcome::Pass);
        assert_eq!(notices.len(), 3);
        assert_eq!(
            notices[0].to_string(),
            "a.service: ignoring ConditionACPower=true (not checked under droidc, assuming it holds)"
        );
        assert_eq!(
            notices[2].to_string(),
            "a.service: ignoring ConditionMadeUp=1 (not recognised by droidc, assuming it holds)"
        );
    }

    /// Whether `key=value`, alone in a unit, passes on `host`.
    fn holds(t: &TempDir, key: &str, value: &str, host: &Host) -> bool {
        let u = unit_with(t, &format!("{key}={value}"));
        evaluate(&u, host, &mut vec![]) == Outcome::Pass
    }

    /// Every kind of check can fail. A kind the stand-in forgot would pass, as unknown kinds do.
    #[test]
    fn each_kind_can_fail() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new();
        let root = t.path().display().to_string();
        let file = t.write("f", "x");
        let file = file.display().to_string();
        let empty_dir = t.path().join("empty");
        fs::create_dir(&empty_dir).unwrap();
        let empty_dir = empty_dir.display().to_string();
        let link = t.link("l", &file).display().to_string();
        let ro = t.write("ro", "x");
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o444)).unwrap();
        let ro = ro.display().to_string();
        let tool = t.write("bin/tool", "#!/bin/sh\n");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let plain = t.write("bin/plain", "x");
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
        let bin = format!("{root}/bin");
        let host = Host::default();

        let cases: &[(&str, &str, bool)] = &[
            ("ConditionPathExistsGlob", &format!("{bin}/t??l"), true),
            ("ConditionPathExistsGlob", &format!("{bin}/x*"), false),
            ("ConditionPathExistsGlob", &format!("{bin}/*z"), false),
            ("ConditionPathExistsGlob", &format!("{bin}/tool?"), false),
            ("ConditionPathExistsGlob", &format!("{root}/nope*/x"), false),
            ("ConditionPathIsDirectory", &root, true),
            ("ConditionPathIsDirectory", &file, false),
            ("ConditionPathIsSymbolicLink", &link, true),
            ("ConditionPathIsSymbolicLink", &file, false),
            ("ConditionPathIsMountPoint", "/", true),
            ("ConditionPathIsMountPoint", &empty_dir, false),
            ("ConditionPathIsReadWrite", &file, true),
            ("ConditionPathIsReadWrite", &ro, false),
            ("ConditionPathIsReadWrite", &format!("{root}/nope"), false),
            ("ConditionDirectoryNotEmpty", &root, true),
            ("ConditionDirectoryNotEmpty", &empty_dir, false),
            (
                "ConditionFileIsExecutable",
                &tool.display().to_string(),
                true,
            ),
            (
                "ConditionFileIsExecutable",
                &plain.display().to_string(),
                false,
            ),
            ("ConditionFileIsExecutable", &bin, false),
            ("ConditionNeedsUpdate", "/etc", false),
            ("ConditionFirstBoot", "yes", false),
            ("ConditionFirstBoot", "no", true),
            ("ConditionArchitecture", "x86-64", false),
            ("ConditionVirtualization", "container-other", true),
            ("ConditionVirtualization", "vm", false),
            ("ConditionVirtualization", "private-users", false),
            ("ConditionVirtualization", "no", false),
            ("ConditionSecurity", "selinux", false),
            ("ConditionCapability", "CAP_SYS_ADMIN", false),
            ("ConditionControlGroupController", "memory", false),
            ("ConditionUser", "k", false),
            ("ConditionGroup", "wheel", false),
            ("ConditionEnvironment", "container", false),
            ("ConditionKernelCommandLine", "quiet", false),
        ];
        for (key, value, want) in cases {
            assert_eq!(holds(&t, key, value, &host), *want, "{key}={value}");
        }
    }

    #[test]
    fn host_facts_can_fail() {
        let t = TempDir::new();
        let host = Host {
            user_name: Some("k".into()),
            uid: Some(10123),
            groups: vec!["wheel".into()],
            gids: vec![10],
            environment: vec![("container".into(), "droidc".into())],
            kernel_command_line: Some("quiet root=/dev/x rw".into()),
            needs_update: true,
            architecture: Some("arm64".into()),
            ..Default::default()
        };
        let cases: &[(&str, &str, bool)] = &[
            ("ConditionUser", "j", false),
            ("ConditionUser", "10123", true),
            ("ConditionUser", "10124", false),
            ("ConditionUser", "@system", false),
            ("ConditionGroup", "audio", false),
            ("ConditionGroup", "10", true),
            ("ConditionGroup", "11", false),
            ("ConditionEnvironment", "container=other", false),
            ("ConditionEnvironment", "nope", false),
            ("ConditionKernelCommandLine", "splash", false),
            ("ConditionKernelCommandLine", "root=/dev/x", true),
            ("ConditionKernelCommandLine", "root=/dev/y", false),
            ("ConditionNeedsUpdate", "/etc", true),
            ("ConditionArchitecture", "arm64", true),
            ("ConditionArchitecture", "x86-64", false),
        ];
        for (key, value, want) in cases {
            assert_eq!(holds(&t, key, value, &host), *want, "{key}={value}");
        }

        // System users are the ones below 1000.
        for (uid, want) in [(0, true), (999, true), (1000, false)] {
            let h = Host {
                uid: Some(uid),
                ..Default::default()
            };
            assert_eq!(holds(&t, "ConditionUser", "@system", &h), want, "uid {uid}");
        }
    }

    #[test]
    fn executable_and_glob() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new();
        let f = t.write("bin/tool", "#!/bin/sh\n");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
        let g = format!("{}/bin/to*", t.path().display());
        let u = unit_with(&t, &format!("ConditionFileIsExecutable={}\nConditionPathExistsGlob={g}\nConditionPathExistsGlob=!{}/bin/x?", f.display(), t.path().display()));
        assert_eq!(evaluate(&u, &Host::default(), &mut vec![]), Outcome::Pass);
    }
}

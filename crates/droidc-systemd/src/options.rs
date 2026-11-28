//! Which unit settings the stand-in honours.
//!
//! The design doc's rule is that nothing is dropped silently: every setting is either supported,
//! planned, or ignored with a reason, and the last two are logged as a [`Notice`] when a unit is
//! loaded. Most ignored settings are sandboxing and resource control that need kernel features an
//! Android app can't use (mount namespaces, seccomp filters installed by the manager, capabilities,
//! cgroups). Ignoring them runs the service with fewer protections than the image asked for, which is
//! why the notice names each one.

use std::fmt;

use crate::loader::LoadedUnit;
use crate::name::{UnitName, UnitType};

/// What the stand-in does with a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Supported,
    /// Not done yet, but meant to be. Logged until it is.
    Planned,
    /// Can't be done under droidc. Logged with the reason.
    Ignored(&'static str),
    /// Not a setting this table knows.
    Unknown,
}

/// A logged line about a setting the unit asked for and won't get.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub unit: String,
    pub key: String,
    pub value: String,
    pub reason: String,
}

impl Notice {
    pub fn new(unit: &UnitName, key: &str, value: &str, reason: &str) -> Notice {
        Notice {
            unit: unit.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            reason: reason.to_string(),
        }
    }
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: ignoring {}={} ({})",
            self.unit, self.key, self.value, self.reason
        )
    }
}

const NO_MOUNT_NS: &str = "no mount namespaces under droidc";
const NO_SECCOMP: &str = "no seccomp or LSM policy under droidc";
const NO_CAPS: &str = "no capabilities under droidc";
const NO_CGROUPS: &str = "no cgroups under droidc";
const NO_USERS: &str = "no user switching under droidc";
const PLANNED: &str = "not implemented yet";

const UNIT_KEYS: &[&str] = &[
    "Description",
    "Documentation",
    "Wants",
    "Requires",
    "Requisite",
    "BindsTo",
    "PartOf",
    "Upholds",
    "Conflicts",
    "Before",
    "After",
    "OnFailure",
    "OnSuccess",
    "PropagatesReloadTo",
    "ReloadPropagatedFrom",
    "PropagatesStopTo",
    "StopPropagatedFrom",
    "JoinsNamespaceOf",
    "RequiresMountsFor",
    "WantsMountsFor",
    "OnFailureJobMode",
    "IgnoreOnIsolate",
    "StopWhenUnneeded",
    "RefuseManualStart",
    "RefuseManualStop",
    "AllowIsolate",
    "DefaultDependencies",
    "SurviveFinalKillSignal",
    "CollectMode",
    "FailureAction",
    "SuccessAction",
    "FailureActionExitStatus",
    "SuccessActionExitStatus",
    "JobTimeoutSec",
    "JobRunningTimeoutSec",
    "JobTimeoutAction",
    "JobTimeoutRebootArgument",
    "StartLimitIntervalSec",
    "StartLimitBurst",
    "StartLimitAction",
    "RebootArgument",
    "SourcePath",
];

const INSTALL_KEYS: &[&str] = &[
    "Alias",
    "WantedBy",
    "RequiredBy",
    "UpheldBy",
    "Also",
    "DefaultInstance",
];

const SERVICE_KEYS: &[&str] = &[
    "Type",
    "ExitType",
    "RemainAfterExit",
    "GuessMainPID",
    "PIDFile",
    "BusName",
    "ExecStart",
    "ExecStartPre",
    "ExecStartPost",
    "ExecCondition",
    "ExecReload",
    "ExecStop",
    "ExecStopPost",
    "RestartSec",
    "RestartSteps",
    "RestartMaxDelaySec",
    "TimeoutStartSec",
    "TimeoutStopSec",
    "TimeoutAbortSec",
    "TimeoutSec",
    "TimeoutStartFailureMode",
    "TimeoutStopFailureMode",
    "RuntimeMaxSec",
    "RuntimeRandomizedExtraSec",
    "WatchdogSec",
    "Restart",
    "RestartMode",
    "SuccessExitStatus",
    "RestartPreventExitStatus",
    "RestartForceExitStatus",
    "RootDirectoryStartOnly",
    "NonBlocking",
    "NotifyAccess",
    "Sockets",
    "FileDescriptorStoreMax",
    "FileDescriptorStorePreserve",
    "OOMPolicy",
    "ReloadSignal",
];

/// `[Service]`, `[Socket]` and the other exec sections share these (`systemd.exec(5)`, `systemd.kill(5)`).
const EXEC_KEYS: &[&str] = &[
    "WorkingDirectory",
    "User",
    "Group",
    "SupplementaryGroups",
    "UMask",
    "Environment",
    "EnvironmentFile",
    "PassEnvironment",
    "UnsetEnvironment",
    "StandardInput",
    "StandardOutput",
    "StandardError",
    "StandardInputText",
    "StandardInputData",
    "SyslogIdentifier",
    "SyslogFacility",
    "SyslogLevel",
    "SyslogLevelPrefix",
    "LogLevelMax",
    "LogExtraFields",
    "TTYPath",
    "TTYReset",
    "TTYVHangup",
    "TTYVTDisallocate",
    "NoNewPrivileges",
    "RuntimeDirectory",
    "StateDirectory",
    "CacheDirectory",
    "LogsDirectory",
    "ConfigurationDirectory",
    "RuntimeDirectoryMode",
    "StateDirectoryMode",
    "CacheDirectoryMode",
    "LogsDirectoryMode",
    "ConfigurationDirectoryMode",
    "RuntimeDirectoryPreserve",
    "SetCredential",
    "SetCredentialEncrypted",
    "LoadCredential",
    "LoadCredentialEncrypted",
    "ImportCredential",
    "KillMode",
    "KillSignal",
    "RestartKillSignal",
    "SendSIGHUP",
    "SendSIGKILL",
    "FinalKillSignal",
    "WatchdogSignal",
    "TimerSlackNSec",
    "Personality",
    "IgnoreSIGPIPE",
];

const EXEC_PLANNED_PREFIXES: &[&str] = &["Limit"];
const EXEC_PLANNED: &[&str] = &[
    "Nice",
    "OOMScoreAdjust",
    "CPUSchedulingPolicy",
    "CPUSchedulingPriority",
    "IOSchedulingClass",
    "IOSchedulingPriority",
];

const MOUNT_NS_PREFIXES: &[&str] = &["Protect", "Private", "Bind", "Root", "Mount"];
const MOUNT_NS: &[&str] = &[
    "ReadWritePaths",
    "ReadOnlyPaths",
    "InaccessiblePaths",
    "ExecPaths",
    "NoExecPaths",
    "TemporaryFileSystem",
    "ExtensionDirectories",
    "ExtensionImages",
    "NetworkNamespacePath",
    "IPCNamespacePath",
    "JoinsNamespaceOf",
    "ProcSubset",
    "PrivateMounts",
];
const SECCOMP_PREFIXES: &[&str] = &["SystemCall", "Restrict", "AppArmor", "SELinux", "Smack"];
const SECCOMP: &[&str] = &["MemoryDenyWriteExecute", "LockPersonality", "KeyringMode"];
const CAPS: &[&str] = &["CapabilityBoundingSet", "AmbientCapabilities", "SecureBits"];
const CGROUP_PREFIXES: &[&str] = &[
    "Memory",
    "CPU",
    "IO",
    "Tasks",
    "Startup",
    "Device",
    "IPAddress",
    "ManagedOOM",
    "Socket",
];
const CGROUP: &[&str] = &[
    "Delegate",
    "DelegateSubgroup",
    "Slice",
    "BlockIOAccounting",
    "AllowedCPUs",
    "AllowedMemoryNodes",
    "DisableControllers",
    "CoredumpReceive",
    "BPFProgram",
    "NUMAPolicy",
    "NUMAMask",
];

fn starts(key: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| key.starts_with(p))
}

/// How `section`/`key` is treated in a unit of type `ty`.
pub fn classify(ty: UnitType, section: &str, key: &str) -> Support {
    match section {
        "Unit" => {
            if UNIT_KEYS.contains(&key) || key.starts_with("Condition") || key.starts_with("Assert")
            {
                Support::Supported
            } else {
                Support::Unknown
            }
        }
        "Install" if INSTALL_KEYS.contains(&key) => Support::Supported,
        "Install" => Support::Unknown,
        "Service" if ty == UnitType::Service => exec_key(key, SERVICE_KEYS),
        "Socket" | "Timer" | "Path" if section == ty.section().unwrap_or("") => Support::Planned,
        "Mount" | "Automount" | "Swap" => Support::Ignored("droidc doesn't mount filesystems"),
        "Slice" | "Scope" => Support::Ignored(NO_CGROUPS),
        _ => Support::Unknown,
    }
}

fn exec_key(key: &str, own: &[&str]) -> Support {
    if own.contains(&key) || EXEC_KEYS.contains(&key) {
        return match key {
            // The stand-in runs everything as the app's uid (design doc, "Decisions").
            "User" | "Group" | "SupplementaryGroups" => Support::Ignored(NO_USERS),
            "RootDirectoryStartOnly" => Support::Ignored(NO_MOUNT_NS),
            _ => Support::Supported,
        };
    }
    if EXEC_PLANNED.contains(&key) || starts(key, EXEC_PLANNED_PREFIXES) {
        return Support::Planned;
    }
    if key == "DynamicUser" {
        return Support::Ignored(NO_USERS);
    }
    if CAPS.contains(&key) {
        return Support::Ignored(NO_CAPS);
    }
    if SECCOMP.contains(&key) || starts(key, SECCOMP_PREFIXES) {
        return Support::Ignored(NO_SECCOMP);
    }
    if MOUNT_NS.contains(&key) || starts(key, MOUNT_NS_PREFIXES) {
        return Support::Ignored(NO_MOUNT_NS);
    }
    if CGROUP.contains(&key) || starts(key, CGROUP_PREFIXES) {
        return Support::Ignored(NO_CGROUPS);
    }
    Support::Unknown
}

/// A notice for every setting in `unit` that won't be honoured. `X-` sections are vendor extensions
/// and are skipped without one, as systemd does.
pub fn notices(unit: &LoadedUnit) -> Vec<Notice> {
    let mut out = Vec::new();
    for a in &unit.assignments {
        if a.section.starts_with("X-") {
            continue;
        }
        let reason = match classify(unit.id.ty(), &a.section, &a.key) {
            Support::Supported => continue,
            // An empty assignment only resets; there is nothing to ignore.
            _ if a.value.is_empty() => continue,
            Support::Planned => PLANNED,
            Support::Ignored(r) => r,
            Support::Unknown => "not recognised by droidc",
        };
        out.push(Notice::new(&unit.id, &a.key, &a.value, reason));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::testdir::TempDir;
    use crate::loader::Loader;
    use crate::paths::SearchPath;

    #[test]
    fn classification() {
        let s = UnitType::Service;
        assert_eq!(classify(s, "Unit", "After"), Support::Supported);
        assert_eq!(
            classify(s, "Unit", "ConditionPathExists"),
            Support::Supported
        );
        assert_eq!(classify(s, "Install", "WantedBy"), Support::Supported);
        assert_eq!(classify(s, "Service", "ExecStart"), Support::Supported);
        assert_eq!(classify(s, "Service", "StateDirectory"), Support::Supported);
        assert_eq!(classify(s, "Service", "LimitNOFILE"), Support::Planned);
        assert_eq!(
            classify(s, "Service", "ProtectSystem"),
            Support::Ignored(NO_MOUNT_NS)
        );
        assert_eq!(
            classify(s, "Service", "PrivateTmp"),
            Support::Ignored(NO_MOUNT_NS)
        );
        assert_eq!(
            classify(s, "Service", "SystemCallFilter"),
            Support::Ignored(NO_SECCOMP)
        );
        assert_eq!(
            classify(s, "Service", "RestrictNamespaces"),
            Support::Ignored(NO_SECCOMP)
        );
        assert_eq!(
            classify(s, "Service", "CapabilityBoundingSet"),
            Support::Ignored(NO_CAPS)
        );
        assert_eq!(
            classify(s, "Service", "MemoryMax"),
            Support::Ignored(NO_CGROUPS)
        );
        assert_eq!(
            classify(s, "Service", "Slice"),
            Support::Ignored(NO_CGROUPS)
        );
        assert_eq!(
            classify(s, "Service", "DynamicUser"),
            Support::Ignored(NO_USERS)
        );
        assert_eq!(classify(s, "Service", "User"), Support::Ignored(NO_USERS));
        assert_eq!(classify(s, "Service", "Bogus"), Support::Unknown);
        assert_eq!(
            classify(UnitType::Socket, "Socket", "ListenStream"),
            Support::Planned
        );
        assert_eq!(
            classify(UnitType::Service, "Socket", "ListenStream"),
            Support::Unknown
        );
        assert!(matches!(
            classify(UnitType::Mount, "Mount", "What"),
            Support::Ignored(_)
        ));
    }

    #[test]
    fn notices_for_a_sandboxed_service() {
        let t = TempDir::new();
        t.write(
            "u/a.service",
            "[Unit]\nAfter=b.target\n[Service]\nExecStart=/x\nProtectSystem=strict\nPrivateTmp=\nMemoryMax=1G\nFrobnicate=yes\n[X-Vendor]\nThing=1\n",
        );
        let u = Loader::new(SearchPath::from_dirs([t.path().join("u")]))
            .load_str("a.service")
            .unwrap();
        let lines: Vec<String> = notices(&u).iter().map(|n| n.to_string()).collect();
        assert_eq!(
            lines,
            vec![
                "a.service: ignoring ProtectSystem=strict (no mount namespaces under droidc)",
                "a.service: ignoring MemoryMax=1G (no cgroups under droidc)",
                "a.service: ignoring Frobnicate=yes (not recognised by droidc)",
            ]
        );
    }
}

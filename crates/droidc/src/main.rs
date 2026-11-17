use clap::{Parser, Subcommand};
use droidc::deploy::{self, SwitchOpts};
use droidc::error::{Error, Result};
use droidc::events::Emitter;
use droidc::link::{LinkKind, DEFAULT_L2S_PREFIX};
use droidc::platform::Platform;
use droidc::registry::HttpConfig;
use droidc::{status, supervise};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "droidc",
    version,
    about = "Run bootc images on Android under proot"
)]
struct Cli {
    /// Root directory for blobs, deployments and persistent state
    #[arg(long, global = true, env = "DROIDC_ROOT")]
    root: Option<PathBuf>,
    /// NDJSON events on stdout (last line is always a `result`)
    #[arg(long, global = true)]
    json: bool,
    /// Comma separated DNS server IPs (default: /etc/resolv.conf, then 1.1.1.1,8.8.8.8)
    #[arg(long, global = true, env = "DROIDC_DNS")]
    dns: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pull, verify, unpack and activate an image
    Switch {
        reference: String,
        /// Do not require a valid cosign signature (recorded as insecure)
        #[arg(long)]
        insecure_allow_unsigned: bool,
        /// os/arch, default from uname(2)
        #[arg(long)]
        platform: Option<String>,
        /// auto|hardlink|link2symlink|copy
        #[arg(long, default_value = "auto")]
        links: String,
        /// Resolve and verify the signature only; download and change nothing
        #[arg(long)]
        verify_only: bool,
    },
    /// Re-resolve the current ref; no-op if the digest is unchanged
    Upgrade,
    /// Swap current and previous
    Rollback,
    /// Show current, previous and staged
    Status,
    /// Remove unreferenced deployments and blobs
    Gc,
    /// How to enter the current deployment
    ShellSpec,
    /// Detected platform
    Platform,
    /// (inside the guest) start session services, then run a command
    Supervise {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
}

fn default_root() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(x).join("droidc");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/droidc")
}

fn human(cmd: &str, d: &Value) {
    let s = |v: &Value| v.as_str().unwrap_or("-").to_string();
    match cmd {
        "switch" | "upgrade" if d["changed"] == true => {
            println!(
                "deployed {} ({}), previous: {}",
                s(&d["id"]),
                s(&d["deployment"]["image_ref"]),
                s(&d["previous"])
            )
        }
        "switch" | "upgrade" if d["verify_only"] == true => println!(
            "verified {} ({})",
            s(&d["manifest_digest"]),
            s(&d["signature"]["mode"])
        ),
        "switch" | "upgrade" => println!("already up to date: {}", s(&d["id"])),
        "rollback" => println!(
            "current is now {}, previous {}",
            s(&d["current"]),
            s(&d["previous"])
        ),
        _ => println!("{}", serde_json::to_string_pretty(d).unwrap_or_default()),
    }
}

fn run(cli: &Cli, em: &Emitter) -> Result<(&'static str, Value)> {
    let root = cli.root.clone().unwrap_or_else(default_root);
    let http = HttpConfig::from_env(cli.dns.clone());
    let l2s = std::env::var("DROIDC_L2S_PREFIX").unwrap_or_else(|_| DEFAULT_L2S_PREFIX.to_string());
    match &cli.cmd {
        Cmd::Switch {
            reference,
            insecure_allow_unsigned,
            platform,
            links,
            verify_only,
        } => {
            let mut o = SwitchOpts::new(reference);
            o.insecure = *insecure_allow_unsigned;
            o.platform = platform.as_deref().map(Platform::parse).transpose()?;
            o.links = LinkKind::parse(links)
                .ok_or_else(|| Error::invalid(format!("unknown --links '{links}'")))?;
            o.http = http;
            o.l2s_prefix = l2s;
            o.verify_only = *verify_only;
            Ok(("switch", deploy::switch(&root, em, &o)?))
        }
        Cmd::Upgrade => Ok(("upgrade", deploy::upgrade(&root, em, http, &l2s)?)),
        Cmd::Rollback => Ok(("rollback", deploy::rollback(&root, em)?)),
        Cmd::Status => Ok(("status", status::status(&root)?)),
        Cmd::Gc => Ok(("gc", deploy::gc(&root, em)?)),
        Cmd::ShellSpec => Ok(("shell-spec", status::shell_spec(&root)?)),
        Cmd::Platform => Ok(("platform", serde_json::to_value(Platform::detect()?)?)),
        Cmd::Supervise { .. } => unreachable!(),
    }
}

fn main() {
    let cli = Cli::parse();
    if let Cmd::Supervise { cmd } = &cli.cmd {
        match supervise::supervise(cmd) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("droidc supervise: {e}");
                std::process::exit(127);
            }
        }
    }
    let em = Emitter::stdout(cli.json);
    match run(&cli, &em) {
        Ok((name, data)) => {
            if cli.json {
                em.result_ok(&data);
            } else {
                human(name, &data);
            }
        }
        Err(e) => {
            em.result_err(&e);
            std::process::exit(1);
        }
    }
}

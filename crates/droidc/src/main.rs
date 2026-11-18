use clap::{Parser, Subcommand};
use droidc::deploy::{self, SwitchOpts};
use droidc::error::{Error, Result};
use droidc::events::Emitter;
use droidc::layout::Root;
use droidc::link::{LinkKind, DEFAULT_L2S_PREFIX};
use droidc::platform::Platform;
use droidc::registry::HttpConfig;
use droidc::session::{self, Renderer, SessionSpec};
use droidc::{bridge, enter, logcat, login1, status, supervise};
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
    ShellSpec {
        /// proot|chroot (chroot needs real root)
        #[arg(long, default_value = "proot")]
        launcher: String,
    },
    /// Detected platform
    Platform,
    /// (as root) run a command in the current deployment: namespaces + chroot, no proot
    Enter {
        /// Extra bind after the deployment's, HOST:/GUEST (repeatable)
        #[arg(long = "bind", value_name = "HOST:GUEST")]
        binds: Vec<String>,
        /// Extra environment, KEY=VALUE (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Hostname in the new UTS namespace
        #[arg(long)]
        hostname: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// PID 1 of `enter`'s namespace (internal)
    #[command(name = "__enter-init", hide = true)]
    EnterInit {
        plan: String,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// (inside the guest) start session services, then run a command or the graphical session
    Supervise {
        /// Start the deployment's graphical session (see `droidc session`)
        #[arg(long)]
        session: bool,
        /// Session width in pixels (`{width}`)
        #[arg(long, default_value_t = 1280)]
        width: u32,
        /// Session height in pixels (`{height}`)
        #[arg(long, default_value_t = 800)]
        height: u32,
        /// Session scale factor (`{scale}`)
        #[arg(long, default_value_t = 1.0)]
        scale: f64,
        /// Run the image's `systemd --user` on the cgroup shim (DROIDC_CGSHIM, default
        /// /usr/lib/droidc/cgshim.so) instead of a bare dbus-daemon
        #[arg(long, value_enum, default_value_t = supervise::SystemdMode::Off)]
        systemd: supervise::SystemdMode,
        /// Command to run. With --session it is optional and replaces the screencast bridge (or runs as an
        /// extra client of a nested session); the session ends when it exits
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            required_unless_present = "session"
        )]
        cmd: Vec<String>,
    },
    /// (inside the guest) show a headless session compositor on the host compositor
    Bridge {
        #[arg(long, default_value_t = 1280)]
        width: u32,
        #[arg(long, default_value_t = 800)]
        height: u32,
        /// Monitor to record
        #[arg(long, default_value = "Meta-0")]
        connector: String,
        /// Write one frame as PNG and exit (no host compositor needed)
        #[arg(long)]
        dump_frame: Option<PathBuf>,
        /// Seconds to wait for the session compositor's ScreenCast service
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Write one message to Android's log (logcat), from the guest or the host
    Log {
        /// debug, info, warn or error
        #[arg(short, long, default_value = "info")]
        priority: String,
        #[arg(short, long, default_value = "droidc")]
        tag: String,
        #[arg(required = true, trailing_var_arg = true)]
        message: Vec<String>,
    },
    /// (inside the guest) answer org.freedesktop.login1 on the system bus (stand-in for logind)
    Login1,
    /// Show or override how the graphical session starts
    Session {
        #[command(subcommand)]
        action: SessionCmd,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// The resolved session (override, else image labels)
    Show,
    /// Write `<root>/session.json`, which takes precedence over the image labels
    Set {
        /// wayland|screencast
        #[arg(long)]
        renderer: String,
        /// Session command as a JSON array, e.g. '["gamescope","-W","{width}","-H","{height}"]'
        #[arg(long)]
        exec: String,
        /// Commands started first, as a JSON array of arrays, e.g. '[["pipewire"]]'
        #[arg(long)]
        services: Option<String>,
        /// Display name (exported as XDG_CURRENT_DESKTOP)
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove the override
    Clear,
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

fn session_cmd(root: &std::path::Path, action: &SessionCmd) -> Result<Value> {
    let r = Root::new(root);
    match action {
        SessionCmd::Show => {}
        SessionCmd::Set {
            renderer,
            exec,
            services,
            name,
        } => {
            let spec = SessionSpec {
                name: name.clone(),
                renderer: Renderer::parse(renderer).ok_or_else(|| {
                    Error::invalid(format!(
                        "unknown --renderer '{renderer}' (wayland|screencast)"
                    ))
                })?,
                exec: session::parse_argv(exec)
                    .map_err(|e| Error::invalid(format!("--exec: {e}")))?,
                services: match services {
                    Some(s) => session::parse_services(s)
                        .map_err(|e| Error::invalid(format!("--services: {e}")))?,
                    None => Vec::new(),
                },
            };
            session::write_override(&r, &spec)?;
        }
        SessionCmd::Clear => {
            session::clear_override(&r)?;
        }
    }
    let label = r
        .current_id()
        .and_then(|id| r.read_record(&id).ok())
        .and_then(|d| d.session_label);
    Ok(session::describe(
        &session::resolve(root)?,
        label.as_deref(),
    ))
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
        Cmd::ShellSpec { launcher } => {
            let l = status::Launcher::parse(launcher)
                .ok_or_else(|| Error::invalid(format!("unknown --launcher '{launcher}'")))?;
            Ok((
                "shell-spec",
                serde_json::to_value(status::shell_spec(&root, l)?)?,
            ))
        }
        Cmd::Platform => Ok(("platform", serde_json::to_value(Platform::detect()?)?)),
        Cmd::Session { action } => Ok(("session", session_cmd(&root, action)?)),
        Cmd::Supervise { .. }
        | Cmd::Bridge { .. }
        | Cmd::Login1
        | Cmd::Log { .. }
        | Cmd::Enter { .. }
        | Cmd::EnterInit { .. } => unreachable!(),
    }
}

fn main() {
    let cli = Cli::parse();
    // These own stdio (it belongs to the command they run): no NDJSON, errors on stderr, exit 127.
    let direct = match &cli.cmd {
        Cmd::Supervise {
            session,
            width,
            height,
            scale,
            systemd,
            cmd,
        } => {
            let opts = supervise::Opts {
                session: session.then(|| supervise::SessionOpts {
                    root: cli.root.clone().unwrap_or_else(default_root),
                    params: session::Params {
                        width: *width,
                        height: *height,
                        scale: *scale,
                        ..Default::default()
                    },
                }),
                systemd: *systemd,
            };
            Some(("supervise", supervise::supervise_with(cmd, &opts)))
        }
        Cmd::Enter {
            binds,
            env,
            hostname,
            cmd,
        } => {
            let opts = enter::EnterOpts {
                binds: binds.clone(),
                env: env.clone(),
                hostname: hostname.clone(),
            };
            let root = cli.root.clone().unwrap_or_else(default_root);
            Some(("enter", enter::enter(&root, &opts, cmd)))
        }
        Cmd::EnterInit { plan, cmd } => Some(("enter", enter::init(plan, cmd))),
        _ => None,
    };
    if let Some((name, r)) = direct {
        match r {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("droidc {name}: {e}");
                std::process::exit(127);
            }
        }
    }
    if let Cmd::Bridge {
        width,
        height,
        connector,
        dump_frame,
        timeout,
    } = &cli.cmd
    {
        let opts = bridge::Opts {
            width: *width,
            height: *height,
            connector: connector.clone(),
            dump_frame: dump_frame.clone(),
            timeout: std::time::Duration::from_secs(*timeout),
        };
        match bridge::run(&opts) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("droidc bridge: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Cmd::Log {
        priority,
        tag,
        message,
    } = &cli.cmd
    {
        std::process::exit(match logcat::write(priority, tag, &message.join(" ")) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("droidc log: {e}");
                1
            }
        });
    }
    if let Cmd::Login1 = &cli.cmd {
        match login1::run() {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("droidc login1: {e}");
                std::process::exit(1);
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

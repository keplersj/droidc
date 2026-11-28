use clap::{Parser, Subcommand};
use droidc::deploy::{self, SwitchOpts};
use droidc::error::{Error, Result};
use droidc::events::Emitter;
use droidc::layout::{Root, DEFAULT_CONTAINER};
use droidc::link::{LinkKind, DEFAULT_L2S_PREFIX};
use droidc::platform::Platform;
use droidc::registry::HttpConfig;
use droidc::session::{self, Renderer, SessionSpec};
use droidc::{apps, bridge, enter, logcat, login1, ownership, polkit, status, supervise};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "droidc",
    version = droidc::VERSION,
    about = "Run bootc images on Android under proot"
)]
struct Cli {
    /// Root directory for blobs, deployments and persistent state
    #[arg(long, global = true, env = "DROIDC_ROOT")]
    root: Option<PathBuf>,
    /// Container to act on. Containers share downloaded images; each has its own deployments,
    /// /var, /home and session. `default` lives at the top of the root, as before containers
    #[arg(long, global = true, env = "DROIDC_CONTAINER", default_value = DEFAULT_CONTAINER)]
    container: String,
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
        /// (root) Apply the image's uid/gid/mode/xattrs, as fix-ownership does
        #[arg(long)]
        apply_ownership: bool,
    },
    /// Re-resolve the current ref; no-op if the digest is unchanged
    Upgrade,
    /// Swap current and previous
    Rollback,
    /// Show current, previous and staged
    Status,
    /// Applications in the current deployment, from its desktop entries
    Apps,
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
    /// (as root) apply the image's uid/gid/mode/xattrs to the current deployment, or with --undo
    /// give the whole root back to the app uid
    FixOwnership {
        #[arg(long)]
        undo: bool,
    },
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
    /// (inside the guest) keep the container's session bus or `systemd --user` for its supervisors
    #[command(name = "user-services", hide = true)]
    UserServices {
        /// As `supervise --systemd`; the first to start the container's services decides
        #[arg(long, value_enum, default_value_t = supervise::SystemdMode::Off)]
        systemd: supervise::SystemdMode,
        /// Keep running with no supervisors (the host stops it), instead of stopping after
        /// DROIDC_USER_LINGER seconds (default 10)
        #[arg(long)]
        stay: bool,
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
        /// Monitor scale factor (e.g. 1.5 for 150%)
        #[arg(long, default_value_t = 0.0)]
        scale: f64,
        /// Seconds to wait for the session compositor's ScreenCast service
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// (inside the guest) stream the guest's sound as raw S16LE PCM on stdout, from a PipeWire sink
    /// made the default; status goes to stderr
    #[command(name = "audio-out")]
    AudioOut {
        /// `node.name` of the sink to create (or reuse)
        #[arg(long, default_value = "selkie-out")]
        sink: String,
        #[arg(long, default_value_t = 48_000)]
        rate: u32,
        /// 1, 2, 4 or 6
        #[arg(long, default_value_t = 2)]
        channels: u32,
        /// Recorder latency, as pw-record takes it
        #[arg(long, default_value = "20ms")]
        latency: String,
        /// Seconds to wait for the guest's PipeWire
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// (inside the guest) the guest's MPRIS media players as JSON lines on stdout, one each time
    /// they change; commands such as `play-pause <player>` on stdin
    Media {
        /// Seconds to wait for the session bus
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Milliseconds between re-reads when no signal arrives
        #[arg(long, default_value_t = 2000)]
        poll: u64,
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
    /// (inside the guest) the desktop's channel to Android: JSON lines, Android state in on stdin
    /// (appearance), requests out on stdout; status goes to stderr
    #[command(name = "host-link")]
    HostLink,
    /// (inside the guest) the container's adb, connected to this Android device; `droidc adb pair`
    /// pairs it with Wireless debugging from a notification
    #[command(disable_help_flag = true)]
    Adb {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// (inside the guest) open a web, mail, phone, sms or map link on Android, through the host link
    Open {
        /// The link, e.g. https://example.org or mailto:someone@example.org
        uri: String,
    },
    /// (inside the guest) answer org.freedesktop.PolicyKit1 on the system bus (stand-in for polkitd)
    Polkit,
    /// (on the host) run commands as fake root for the guest's sudo/su/pkexec shims
    #[command(name = "spawn-server", hide = true)]
    SpawnServer {
        /// Abstract socket name the guest shims connect to (DROIDC_SPAWNER)
        #[arg(long)]
        socket: String,
        /// Only serve this uid (default: our own)
        #[arg(long)]
        uid: Option<u32>,
        /// The launcher to run, e.g. the proot argv with -0; the command is appended
        #[arg(last = true, required = true)]
        launcher: Vec<String>,
    },
    /// (inside the guest) ask the host spawner to run a command as root
    #[command(name = "spawn", hide = true)]
    Spawn {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Show or override how the graphical session starts (`show` when no subcommand is given)
    Session {
        #[command(subcommand)]
        action: Option<SessionCmd>,
    },
    /// List or remove containers (`--container` picks the one other commands act on)
    Container {
        #[command(subcommand)]
        action: ContainerCmd,
    },
}

#[derive(Subcommand)]
enum ContainerCmd {
    /// Every container under the root, with its current and previous deployment
    List,
    /// Delete a container's deployments, /var, /home and session, then GC unused images.
    /// For `default` only its own state goes; the root and the other containers stay.
    Rm { name: String },
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

fn session_cmd(root: &Root, action: &SessionCmd) -> Result<Value> {
    let r = root;
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
            session::write_override(r, &spec)?;
        }
        SessionCmd::Clear => {
            session::clear_override(r)?;
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

fn target(cli: &Cli) -> Result<Root> {
    Root::with_container(
        cli.root.clone().unwrap_or_else(default_root),
        &cli.container,
    )
}

fn run(cli: &Cli, em: &Emitter) -> Result<(&'static str, Value)> {
    let root = target(cli)?;
    let http = HttpConfig::from_env(cli.dns.clone());
    let l2s = std::env::var("DROIDC_L2S_PREFIX").unwrap_or_else(|_| DEFAULT_L2S_PREFIX.to_string());
    match &cli.cmd {
        Cmd::Switch {
            reference,
            insecure_allow_unsigned,
            platform,
            links,
            verify_only,
            apply_ownership,
        } => {
            let mut o = SwitchOpts::new(reference);
            o.insecure = *insecure_allow_unsigned;
            o.platform = platform.as_deref().map(Platform::parse).transpose()?;
            o.links = LinkKind::parse(links)
                .ok_or_else(|| Error::invalid(format!("unknown --links '{links}'")))?;
            o.http = http;
            o.l2s_prefix = l2s;
            o.verify_only = *verify_only;
            o.apply_ownership = *apply_ownership;
            Ok(("switch", deploy::switch(&root, em, &o)?))
        }
        Cmd::Upgrade => Ok(("upgrade", deploy::upgrade(&root, em, http, &l2s)?)),
        Cmd::Rollback => Ok(("rollback", deploy::rollback(&root, em)?)),
        Cmd::Status => Ok(("status", status::status(&root)?)),
        Cmd::Apps => Ok(("apps", apps::list(&root)?)),
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
        Cmd::Session { action } => Ok((
            "session",
            session_cmd(&root, action.as_ref().unwrap_or(&SessionCmd::Show))?,
        )),
        Cmd::FixOwnership { undo: false } => Ok(("fix-ownership", ownership::fix(&root, &l2s)?)),
        Cmd::FixOwnership { undo: true } => Ok(("fix-ownership", ownership::undo(&root)?)),
        Cmd::Container {
            action: ContainerCmd::List,
        } => Ok(("container", deploy::list_containers(&root))),
        Cmd::Container {
            action: ContainerCmd::Rm { name },
        } => Ok((
            "container",
            deploy::remove_container(&Root::with_container(&root.path, name)?, em)?,
        )),
        Cmd::Supervise { .. }
        | Cmd::UserServices { .. }
        | Cmd::SpawnServer { .. }
        | Cmd::Spawn { .. }
        | Cmd::Bridge { .. }
        | Cmd::AudioOut { .. }
        | Cmd::Media { .. }
        | Cmd::Login1
        | Cmd::HostLink
        | Cmd::Adb { .. }
        | Cmd::Open { .. }
        | Cmd::Polkit
        | Cmd::Log { .. }
        | Cmd::Enter { .. }
        | Cmd::EnterInit { .. } => unreachable!(),
    }
}

/// Run a guest sudo/su/pkexec/spawn shim and exit with the root command's status; 127 if it could
/// not run. `name` is how the user invoked it, for the error.
fn run_shim(name: &str, args: &[String]) -> i32 {
    let done = if name == "bwrap" {
        droidc::bwrap::client(args)
    } else {
        droidc::spawn::client(name, args)
    };
    match done {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{name}: {e}");
            127
        }
    }
}

fn main() {
    // Bound into the guest as /usr/bin/{sudo,su,pkexec} (shell-spec), so argv[0] names the shim; clap
    // never sees these, and their own flags (-u, -c, ...) stay out of droidc's parser.
    if let Some(shim) = std::env::args().next().and_then(|a0| {
        std::path::Path::new(&a0)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| droidc::spawn::SHIMS.iter().any(|s| s.ends_with(*n)))
            .map(String::from)
    }) {
        let args: Vec<String> = std::env::args().skip(1).collect();
        std::process::exit(run_shim(&shim, &args));
    }
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
                    root: target(&cli).unwrap_or_else(|e| {
                        eprintln!("droidc supervise: {e}");
                        std::process::exit(127)
                    }),
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
        Cmd::UserServices { systemd, stay } => {
            let root = target(&cli).ok();
            Some((
                "user-services",
                droidc::usersvc::run(*systemd, root.as_ref(), *stay),
            ))
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
            Some((
                "enter",
                target(&cli).and_then(|root| enter::enter(&root, &opts, cmd)),
            ))
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
        scale,
        timeout,
    } = &cli.cmd
    {
        let opts = bridge::Opts {
            width: *width,
            height: *height,
            connector: connector.clone(),
            dump_frame: dump_frame.clone(),
            scale: *scale,
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
    if let Cmd::AudioOut {
        sink,
        rate,
        channels,
        latency,
        timeout,
    } = &cli.cmd
    {
        let opts = droidc::audio::Opts {
            sink: sink.clone(),
            rate: *rate,
            channels: *channels,
            latency: latency.clone(),
            timeout: std::time::Duration::from_secs(*timeout),
        };
        match droidc::audio::run(&opts) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("droidc audio-out: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Cmd::Media { timeout, poll } = &cli.cmd {
        let opts = droidc::media::Opts {
            timeout: std::time::Duration::from_secs(*timeout),
            poll: std::time::Duration::from_millis(*poll),
        };
        match droidc::media::run(&opts) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("droidc media: {e}");
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
    if let Cmd::SpawnServer {
        socket,
        uid,
        launcher,
    } = &cli.cmd
    {
        match droidc::spawn::serve(socket, *uid, launcher) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("droidc spawn-server: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Cmd::Spawn { cmd } = &cli.cmd {
        std::process::exit(run_shim("spawn", cmd));
    }
    if let Cmd::Open { uri } = &cli.cmd {
        match droidc::open::open(uri) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("droidc open: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Cmd::Adb { args } = &cli.cmd {
        match droidc::adb::run(args) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("droidc adb: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Cmd::HostLink = &cli.cmd {
        match droidc::host_link::run() {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("droidc host-link: {e}");
                std::process::exit(1);
            }
        }
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
    if let Cmd::Polkit = &cli.cmd {
        match polkit::run() {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("droidc polkit: {e}");
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

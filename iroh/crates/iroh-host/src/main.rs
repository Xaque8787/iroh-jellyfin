use std::net::{TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use iroh::{EndpointAddr, SecretKey};
use iroh_host::config::{
    Cli, Command, DataArgs, HostConfig, InviteArgs, RevokeArgs, RunArgs, parse_relay,
};
use iroh_host::fsutil::{DataDirLock, LockError, ensure_private_dir};
use iroh_host::identity::{self, IDENTITY_FILE};
use iroh_host::serve;
use iroh_host::state::{HostState, STATE_FILE, now_secs};
use iroh_transport::invite::InviteLink;
use iroh_transport::net::RelayChoice;
use tracing_subscriber::EnvFilter;

const ORIGIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Identity(args) => identity_command(&args),
        Command::Doctor(args) => return doctor(&args),
        Command::Serve(args) => serve_command(&args),
        Command::Invite(args) => invite_command(&args),
        Command::Devices(args) => devices_command(&args),
        Command::Revoke(args) => revoke_command(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn prepare(data: &DataArgs) -> Result<SecretKey, String> {
    ensure_private_dir(&data.data_dir)
        .map_err(|error| format!("cannot prepare {}: {error}", data.data_dir.display()))?;
    identity::load_or_create(&data.data_dir.join(IDENTITY_FILE)).map_err(|error| error.to_string())
}

fn open_state(data: &DataArgs) -> Result<HostState, String> {
    HostState::open(&data.data_dir.join(STATE_FILE), None).map_err(|error| error.to_string())
}

fn serve_command(args: &RunArgs) -> Result<(), String> {
    let config = args.validate().map_err(|problems| problems.join("; "))?;
    let secret_key = prepare(&args.data)?;
    let _lock = DataDirLock::acquire(&config.data_dir).map_err(|error| error.to_string())?;
    let state =
        HostState::open(&config.state_path(), config.policy).map_err(|error| error.to_string())?;
    // Target filters match by prefix, so `iroh=warn` alone would also silence iroh_host and iroh_transport.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,iroh=warn,noq=warn,iroh_host=info,iroh_transport=info")
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start async runtime: {error}"))?;
    runtime.block_on(serve::run(config, secret_key, Arc::new(state)))
}

fn invite_command(args: &InviteArgs) -> Result<(), String> {
    let secret_key = prepare(&args.data)?;
    let relay = parse_relay(args.relay_url.as_deref())?;
    let host_running = matches!(
        DataDirLock::acquire(&args.data.data_dir),
        Err(LockError::InUse(_))
    );
    let (addr, source) = match serve::published_addr(&args.data.data_dir, &secret_key) {
        Some(addr) if host_running => (addr, "running host"),
        Some(addr) => (addr, "last run of the host (it is not running now)"),
        None => match relay {
            RelayChoice::Disabled => {
                return Err(
                    "relays are disabled, so the link needs the host's direct addresses; start `iroh-host serve` first".into(),
                );
            }
            RelayChoice::Custom(url) => (
                EndpointAddr::new(secret_key.public()).with_relay_url(url),
                "endpoint id and configured relay",
            ),
            RelayChoice::Default => (
                EndpointAddr::new(secret_key.public()),
                "endpoint id only (found through n0 DNS address lookup)",
            ),
        },
    };
    let state = open_state(&args.data)?;
    let (invitation, token) = state
        .create_invitation(&args.label, args.ttl_secs, now_secs())
        .map_err(|error| error.to_string())?;
    println!("{}", InviteLink::new(addr, token).encode());
    eprintln!(
        "invitation {} for \"{}\" expires in {} minutes; address from {source}",
        invitation.id,
        invitation.label,
        invitation.expires_at.saturating_sub(invitation.created_at) / 60
    );
    eprintln!("the link works once; treat it like a password until it is used");
    Ok(())
}

fn devices_command(args: &DataArgs) -> Result<(), String> {
    ensure_private_dir(&args.data_dir)
        .map_err(|error| format!("cannot prepare {}: {error}", args.data_dir.display()))?;
    let snapshot = open_state(args)?.snapshot();
    if snapshot.devices.is_empty() {
        println!("no paired devices");
    }
    let now = now_secs();
    for device in &snapshot.devices {
        let status = match device.revoked_at {
            None => "active ".to_owned(),
            Some(_) => "revoked".to_owned(),
        };
        println!(
            "{status}  {}  {}  paired {} ago",
            device.endpoint_id,
            device.name,
            age(now.saturating_sub(device.paired_at))
        );
    }
    Ok(())
}

fn revoke_command(args: &RevokeArgs) -> Result<(), String> {
    let state = open_state(&args.data)?;
    let endpoint_id = args.endpoint_id.trim();
    if state
        .revoke_device(endpoint_id, now_secs())
        .map_err(|error| error.to_string())?
    {
        println!("revoked {endpoint_id}");
        Ok(())
    } else {
        Err(format!("no active device with endpoint id {endpoint_id}"))
    }
}

fn age(secs: u64) -> String {
    match secs {
        0..120 => format!("{secs}s"),
        120..7200 => format!("{}m", secs / 60),
        7200..172_800 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn identity_command(args: &DataArgs) -> Result<(), String> {
    println!("{}", prepare(args)?.public());
    Ok(())
}

struct Report {
    failed: bool,
}

impl Report {
    fn ok(&self, check: &str, detail: impl std::fmt::Display) {
        println!("[ok]   {check}: {detail}");
    }

    fn warn(&self, check: &str, detail: impl std::fmt::Display) {
        println!("[warn] {check}: {detail}");
    }

    fn fail(&mut self, check: &str, detail: impl std::fmt::Display) {
        self.failed = true;
        println!("[fail] {check}: {detail}");
    }
}

fn doctor(args: &RunArgs) -> ExitCode {
    let mut report = Report { failed: false };
    let config = match args.validate() {
        Ok(config) => {
            report.ok("configuration", "valid");
            config
        }
        Err(problems) => {
            for problem in problems {
                report.fail("configuration", problem);
            }
            return ExitCode::FAILURE;
        }
    };

    check_data_dir(&config, &mut report);
    check_origin(&config, &mut report);

    let policy = config.policy.unwrap_or_default();
    let source = if config.policy.is_some() {
        "configured"
    } else {
        "default unless state.json overrides"
    };
    report.ok(
        "transport policy",
        format!(
            "{} ({source}), enforcement {}",
            policy.as_str(),
            policy.enforcement()
        ),
    );
    if policy.enforcement() == "best_effort_path_gate" {
        report.warn(
            "transport policy",
            "relay_only and p2p_only are best-effort in this build, not guaranteed; see docs/transport-policies.md",
        );
    }
    match &config.relay {
        RelayChoice::Custom(url) => report.ok("relay", format!("custom {url}")),
        RelayChoice::Default => report.ok("relay", "Iroh default relays"),
        RelayChoice::Disabled => report.warn(
            "relay",
            "disabled; devices must reach this host on a direct address",
        ),
    }
    match &config.control {
        Some(control) if control.listen.ip().is_loopback() => {
            report.ok("control API", format!("v1 on {}", control.listen))
        }
        Some(control) => report.warn(
            "control API",
            format!(
                "v1 on {} is reachable from other machines; protect it with a firewall",
                control.listen
            ),
        ),
        None => report.warn(
            "control API",
            "disabled; the plugin cannot manage this host",
        ),
    }

    if report.failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn check_data_dir(config: &HostConfig, report: &mut Report) {
    if let Err(error) = ensure_private_dir(&config.data_dir) {
        report.fail(
            "data directory",
            format!("{}: {error}", config.data_dir.display()),
        );
        return;
    }
    report.ok("data directory", config.data_dir.display());

    let lock = match DataDirLock::acquire(&config.data_dir) {
        Ok(lock) => Some(lock),
        Err(LockError::InUse(_)) => {
            report.warn(
                "data directory",
                "in use by a running iroh-host; skipping state checks",
            );
            None
        }
        Err(error) => {
            report.fail("data directory", error);
            return;
        }
    };

    match identity::load_or_create(&config.identity_path()) {
        Ok(secret) => report.ok("identity", format!("endpoint id {}", secret.public())),
        Err(error) => report.fail("identity", error),
    }

    if lock.is_some() {
        match HostState::open(&config.state_path(), None) {
            Ok(state) => {
                let snapshot = state.snapshot();
                let active = snapshot.devices.iter().filter(|d| d.is_active()).count();
                report.ok(
                    "state",
                    format!(
                        "{active} authorized device(s), {} invitation(s), stored policy {}",
                        snapshot.invitations.len(),
                        snapshot.policy.as_str()
                    ),
                );
            }
            Err(error) => report.fail("state", error),
        }
    }
}

fn check_origin(config: &HostConfig, report: &mut Report) {
    let (host, port) = config.origin.host_port();
    let addrs = match (host.as_str(), port).to_socket_addrs() {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(error) => {
            report.fail("origin", format!("cannot resolve {host}: {error}"));
            return;
        }
    };
    let reachable = addrs
        .iter()
        .any(|addr| TcpStream::connect_timeout(addr, ORIGIN_CONNECT_TIMEOUT).is_ok());
    if reachable {
        report.ok(
            "origin",
            format!("{} accepts connections", config.origin.display()),
        );
    } else {
        report.fail(
            "origin",
            format!("{} is not accepting connections", config.origin.display()),
        );
    }
}

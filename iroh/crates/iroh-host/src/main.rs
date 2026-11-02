use std::net::{TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use iroh_host::config::{Cli, Command, DataArgs, HostConfig, RunArgs};
use iroh_host::fsutil::{DataDirLock, LockError, ensure_private_dir};
use iroh_host::identity::{self, IDENTITY_FILE};
use iroh_host::state::HostState;

const ORIGIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Identity(args) => identity_command(&args),
        Command::Doctor(args) => doctor(&args),
    }
}

fn identity_command(args: &DataArgs) -> ExitCode {
    let result = ensure_private_dir(&args.data_dir)
        .map_err(|error| format!("cannot prepare {}: {error}", args.data_dir.display()))
        .and_then(|()| {
            identity::load_or_create(&args.data_dir.join(IDENTITY_FILE)).map_err(|e| e.to_string())
        });
    match result {
        Ok(secret) => {
            println!("{}", secret.public());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
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
    match &config.relay_url {
        Some(url) => report.ok("relay", format!("custom {url}")),
        None => report.ok("relay", "Iroh default relays"),
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

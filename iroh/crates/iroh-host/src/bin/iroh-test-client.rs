//! Test client for transport v1: pairs with a host, fetches paths, and runs a local HTTP proxy so a
//! browser or media app on this machine can reach the host's origin over Iroh.

use clap::{Args, Parser, Subcommand};
use iroh_client::{Client, ClientConfig};
use iroh_host::config::parse_relay;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Parser)]
#[command(
    name = "iroh-test-client",
    version,
    about = "Test client for jellyfin-iroh hosts"
)]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct Common {
    /// Directory holding this client's identity and the paired host's address.
    #[arg(
        long,
        global = true,
        env = "IROH_CLIENT_DATA_DIR",
        default_value = "./iroh-client-data"
    )]
    data_dir: PathBuf,
    /// Relay URL. Empty uses Iroh's default relays; `none` disables relays.
    #[arg(long, global = true, env = "IROH_RELAY_URL")]
    relay_url: Option<String>,
    /// Name the host shows for this device.
    #[arg(long, global = true, default_value = "iroh-test-client")]
    name: String,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Redeem an invitation link and remember the host.
    Pair { link: String },
    /// Connect and report authorization, policy, path, and round-trip times.
    Status,
    /// Fetch one path and write the body to stdout or a file.
    Fetch {
        /// Origin-form path, for example /System/Info/Public
        path: String,
        /// Range header value, for example bytes=0-1048575
        #[arg(long)]
        range: Option<String>,
        /// Extra header as name:value. Repeatable.
        #[arg(long = "header")]
        headers: Vec<String>,
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Serve the host's origin on a local port, for example to open Jellyfin in a browser.
    Proxy {
        #[arg(long, default_value = "127.0.0.1:8097")]
        listen: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let common = cli.common;
    let client = Client::new(ClientConfig {
        data_dir: common.data_dir,
        name: common.name,
        relay: parse_relay(common.relay_url.as_deref())?,
    })
    .await
    .map_err(|e| e.to_string())?;
    let result = match cli.command {
        Command::Pair { link } => match client.pair(&link).await {
            Ok(paired) => {
                println!(
                    "{:?}: host {} (transport policy {})",
                    paired.outcome, paired.host_id, paired.transport_policy
                );
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
        Command::Status => status(&client).await,
        Command::Fetch {
            path,
            range,
            headers,
            output,
        } => fetch(&client, &path, range, headers, output).await,
        Command::Proxy { listen } => match client.start_proxy(listen, false).await {
            Ok(connection) => {
                eprintln!(
                    "proxying {} to the host; press Ctrl-C to stop",
                    connection.proxy_url
                );
                tokio::signal::ctrl_c().await.map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        },
    };
    client.shutdown().await;
    result
}

async fn status(client: &Client) -> Result<(), String> {
    let session = client.connect().await.map_err(|e| e.to_string())?;
    println!("authorization: {:?}", session.outcome());
    println!(
        "transport policy: {} ({})",
        session.transport_policy().as_str(),
        session.transport_policy().enforcement()
    );
    for attempt in 1..=5 {
        let rtt = session.ping().await.map_err(|error| error.to_string())?;
        let path = session.path();
        println!(
            "ping {attempt}: {:.1} ms over {:?} path (Iroh rtt {:?} ms, {} open path(s), remote {})",
            rtt.as_secs_f64() * 1000.0,
            path.kind,
            path.rtt_ms,
            path.open_paths,
            path.remote_addr.unwrap_or_else(|| "-".into())
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    session.close().await;
    Ok(())
}

async fn fetch(
    client: &Client,
    path: &str,
    range: Option<String>,
    extra: Vec<String>,
    output: Option<PathBuf>,
) -> Result<(), String> {
    let mut headers = Vec::new();
    for header in extra {
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| format!("header '{header}' must be name:value"))?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    if let Some(range) = range {
        headers.push(("range".into(), range));
    }
    let session = client.connect().await.map_err(|e| e.to_string())?;
    let mut response = session
        .request("GET", path, headers, None)
        .await
        .map_err(|error| error.to_string())?;
    eprintln!("status {}", response.head.status);
    for (name, value) in &response.head.headers {
        eprintln!("{name}: {value}");
    }
    let copied = match output {
        Some(file) => {
            let mut file = tokio::fs::File::create(&file)
                .await
                .map_err(|error| format!("{}: {error}", file.display()))?;
            let copied = tokio::io::copy(&mut response.body, &mut file).await;
            file.flush().await.map_err(|error| error.to_string())?;
            copied
        }
        None => tokio::io::copy(&mut response.body, &mut tokio::io::stdout()).await,
    }
    .map_err(|error| format!("body transfer failed: {error}"))?;
    eprintln!(
        "received {copied} bytes over {:?} path",
        session.path().kind
    );
    session.close().await;
    Ok(())
}

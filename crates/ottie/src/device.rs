//! `ottie device`: tokens of their own for other devices (docs/decisions/0052),
//! through the running daemon's API with this computer's token.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use ottie_api::{AddDevice, AddedDevice, ApiError, Device, IdentityChallenge, IdentityProof};

use crate::config::{self, CONFIG_FILE, DaemonArgs, Dirs, FileConfig};
use crate::token;

#[derive(Args, Debug)]
pub struct DeviceArgs {
    #[command(subcommand)]
    command: DeviceCommand,
}

#[derive(Subcommand, Debug)]
enum DeviceCommand {
    /// Give a device a token of its own; prints it and a sign-in link, this once.
    Add {
        /// What to call it: "Phone", "Laptop"…
        name: String,
        /// It may look, play and copy links, but not change anything
        #[arg(long)]
        read_only: bool,
        #[command(flatten)]
        daemon: DaemonArgs,
    },
    /// Every device with a token of its own.
    List {
        #[command(flatten)]
        daemon: DaemonArgs,
    },
    /// Take a device's token away (its ID is in `ottie device list`).
    Remove {
        id: u32,
        #[command(flatten)]
        daemon: DaemonArgs,
    },
}

pub async fn run(args: DeviceArgs) -> Result<()> {
    match args.command {
        DeviceCommand::Add {
            name,
            read_only,
            daemon,
        } => {
            let client = Client::new(&daemon)?;
            let body = AddDevice {
                name,
                read_only: Some(read_only),
            };
            let request = client.http.post(client.url("/access/devices")).json(&body);
            let added: AddedDevice = client.send(request).await?.json().await?;
            let device = &added.device;
            let access = if device.read_only {
                "read-only"
            } else {
                "full access"
            };
            println!(
                "Added \"{}\" (device {}, {access}).",
                device.name, device.id
            );
            println!("Its token, shown this once:\n  {}", added.token);
            println!(
                "Its sign-in link (from another device, use the address it reaches Ottie at):\n  {}",
                config::ui_url(client.addr, &added.token)
            );
            Ok(())
        }
        DeviceCommand::List { daemon } => {
            let client = Client::new(&daemon)?;
            let request = client.http.get(client.url("/access/devices"));
            let devices: Vec<Device> = client.send(request).await?.json().await?;
            if devices.is_empty() {
                println!("No devices yet: `ottie device add <name>` adds one.");
                return Ok(());
            }
            let width = devices
                .iter()
                .map(|device| device.name.chars().count())
                .max()
                .unwrap_or(0)
                .max("Name".len());
            println!("{:>4}  {:width$}  {:10}  Last used", "ID", "Name", "Access");
            let now = millis(SystemTime::now());
            for device in devices {
                let access = if device.read_only {
                    "read-only"
                } else {
                    "full"
                };
                let used = device
                    .last_used
                    .map_or_else(|| "never".to_owned(), |at| ago(now.saturating_sub(at)));
                println!(
                    "{:>4}  {:width$}  {access:10}  {used}",
                    device.id, device.name
                );
            }
            Ok(())
        }
        DeviceCommand::Remove { id, daemon } => {
            let client = Client::new(&daemon)?;
            let request = client
                .http
                .delete(client.url(&format!("/access/devices/{id}")));
            client.send(request).await?;
            println!("Device {id} no longer has access.");
            Ok(())
        }
    }
}

/// A client of the daemon on this computer, with this computer's token.
pub struct Client {
    pub http: reqwest::Client,
    pub addr: SocketAddr,
    token: String,
}

impl Client {
    pub fn new(daemon: &DaemonArgs) -> Result<Client> {
        let dirs = Dirs::resolve(&daemon.dirs)?;
        let file = FileConfig::load(&dirs.config.join(CONFIG_FILE))?;
        let addr = config::listen_addr(daemon.address, daemon.port, &file);
        let http = reqwest::Client::builder()
            // Ottie is on this computer: never through a proxy.
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .context("can't make an HTTP client")?;
        Ok(Client {
            http,
            addr,
            token: token::for_client(&dirs)?,
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}/api/v1{path}", config::local(self.addr))
    }

    /// Sends it with this computer's token, once what answers has proved
    /// it's this computer's Ottie; a refusal becomes the error.
    pub async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        check_identity(&self.http, self.addr, &self.token).await?;
        let response = match request.bearer_auth(&self.token).send().await {
            Ok(response) => response,
            Err(err) if err.is_connect() => bail!(
                "Ottie isn't running at {}: start it with `ottie serve`",
                config::local(self.addr)
            ),
            Err(err) => return Err(err).context("Ottie didn't answer"),
        };
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        match response.json::<ApiError>().await {
            Ok(refused) => bail!("Ottie refused: {}", refused.error),
            Err(_) => bail!("Ottie answered {status}"),
        }
    }
}

/// Makes sure what answers at `addr` is this computer's Ottie before its
/// token goes there (docs/decisions/0058): it must prove it knows the token,
/// without receiving it. Anything else on that port gets nothing.
pub async fn check_identity(http: &reqwest::Client, addr: SocketAddr, token: &str) -> Result<()> {
    let local = config::local(addr);
    let nonce = ottie_api::identity::nonce()
        .map_err(|err| anyhow!("no random numbers available: {err}"))?;
    let challenge = IdentityChallenge {
        nonce: nonce.clone(),
    };
    let url = format!("http://{local}/api/v1/identity");
    let answer = match http.post(url).json(&challenge).send().await {
        Ok(answer) => answer,
        Err(err) if err.is_connect() => {
            bail!("Ottie isn't running at {local}: start it with `ottie serve`")
        }
        Err(err) => return Err(err).context("Ottie didn't answer"),
    };
    let proven = answer.status().is_success()
        && answer
            .json::<IdentityProof>()
            .await
            .is_ok_and(|answer| ottie_api::identity::proven(token, &nonce, &answer.proof));
    if !proven {
        bail!(
            "what answers at {local} isn't your Ottie: it can't prove it knows this computer's token, so the token wasn't sent.              It may be Ottie started with other folders, an Ottie older than this one (restart it), or another program on that port"
        );
    }
    Ok(())
}

fn millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

/// How long ago, roughly.
fn ago(millis: u64) -> String {
    let minutes = millis / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        1..60 => format!("{minutes} min ago"),
        60..2880 => format!("{} h ago", minutes / 60),
        _ => format!("{} days ago", minutes / 1440),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_used_is_rough() {
        assert_eq!(ago(10_000), "just now");
        assert_eq!(ago(5 * 60_000), "5 min ago");
        assert_eq!(ago(3 * 3_600_000), "3 h ago");
        assert_eq!(ago(3 * 86_400_000), "3 days ago");
    }
}

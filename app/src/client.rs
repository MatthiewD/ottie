//! The app as a client of the daemon's HTTP API, like every other front end
//! (CLAUDE.md, Architecture): what the tray, notifications and clipboard
//! monitoring read and do.
//! Only the fields they need are read; the rest of each answer is ignored.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use rustls_platform_verifier::ConfigVerifierExt;
use serde::Deserialize;
use serde_json::json;
use tauri::Url;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// The events stream's protocol, and the prefix of the one carrying the
/// token (docs/decisions/0014).
const EVENTS_PROTOCOL: &str = "ottie.v1";
const TOKEN_PROTOCOL: &str = "ottie.token.";

pub type Events = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub url: String,
    /// Known once the server has answered.
    pub file_name: Option<String>,
    pub state: String,
    pub downloaded: u64,
    /// Its size, if the server said.
    pub total: Option<u64>,
    /// Its file is moving: it can't be paused or resumed until it's done.
    pub moving: bool,
    /// Why it failed, as a code to translate…
    pub problem: Option<Problem>,
    /// …and in English, for codes this app doesn't know.
    pub error: Option<String>,
}

/// Where a download's file is (docs/decisions/0045): `GET /jobs/{id}`, read
/// for opening it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Placed {
    pub state: String,
    pub dest_dir: String,
    /// Known once the server has answered.
    pub file_name: Option<String>,
}

/// Why a download failed (`Problem` in the API), with what the
/// notification says.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "code", rename_all = "camelCase")]
pub enum Problem {
    Network {
        detail: String,
    },
    Redirect {
        detail: String,
    },
    HttpStatus {
        status: u16,
    },
    Stalled,
    Incomplete,
    RemoteChanging,
    Disk {
        detail: String,
    },
    FileExists,
    /// FTP (docs/decisions/0054): the server refused, with its reply code.
    FtpReply {
        status: u16,
        detail: String,
    },
    /// An FTPS server's certificate nobody trusts (its fingerprint is left
    /// out: the page shows it).
    Certificate {
        host: String,
    },
    NoTls,
    Other {
        detail: String,
    },
    /// From a newer daemon.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub global_limit: u64,
}

/// Who the token is (`AccessInfo`): only whether it may change anything.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Access {
    pub read_only: bool,
}

/// Something in a text that looks like a link (`FoundLink` in the API).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Found {
    Link {
        /// Normalized, as it would be added.
        url: String,
        matched: Matched,
        /// A download of this link is on the list already.
        known: bool,
    },
    /// Can't be added (`invalid`), or from a newer daemon.
    #[serde(other)]
    Other,
}

/// The category a link would get; only its extension matters here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Matched {
    pub extension: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Preview {
    links: Vec<Found>,
}

/// A category: only the file types it takes (lowercase, without the dot).
#[derive(Debug, Clone, Deserialize)]
pub struct Category {
    pub extensions: Vec<String>,
}

/// What `POST /jobs/batch` answers: one ID per job sent, null if refused…
#[derive(Debug, Clone, Deserialize)]
pub struct Added {
    pub ids: Vec<Option<String>>,
    /// …and why, in English.
    pub refused: Vec<Refused>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Refused {
    pub error: String,
}

/// A message on the events stream; the others don't matter to the app.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    Changed {
        job: Job,
    },
    Progress {
        id: String,
        downloaded: u64,
        total: Option<u64>,
    },
    Removed {
        id: String,
    },
    SettingsChanged {
        settings: Settings,
    },
    Resync,
    #[serde(other)]
    Other,
}

/// What a daemon says to a token (`Daemon::check`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Accepted,
    /// It's Ottie, but the token isn't (or no longer) one of its own.
    Refused,
    /// Something answers there, but not Ottie.
    NotOttie,
    Unreachable,
}

pub struct Daemon {
    origin: Url,
    token: String,
    http: Client,
    /// The events stream over HTTPS (another computer's Ottie behind
    /// Tailscale or a proxy), checked against the system's certificates.
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl Daemon {
    /// From a signed-in link: `ottie link`'s (`http://127.0.0.1:24068/#token=…`),
    /// or another computer's (docs/decisions/0053).
    pub fn new(link: &Url) -> Result<Daemon> {
        let mut http = Client::builder()
            // Pausing a running download waits until it has stopped.
            .timeout(Duration::from_secs(30));
        // This computer's is never behind a proxy; another one may be.
        if is_loopback(link) {
            http = http.no_proxy();
        }
        let tls = match link.scheme() {
            "https" => Some(Arc::new(
                rustls::ClientConfig::with_platform_verifier()
                    .context("can't use the system's certificates")?,
            )),
            _ => None,
        };
        Ok(Daemon {
            origin: link.join("/")?,
            token: token(link).to_owned(),
            http: http.build()?,
            tls,
        })
    }

    /// Whether it answers and takes the token, within `wait`.
    pub async fn check(&self, wait: Duration) -> Check {
        let Ok(url) = self.url("jobs") else {
            return Check::Unreachable;
        };
        let request = self.http.get(url).bearer_auth(&self.token).timeout(wait);
        match request.send().await {
            Ok(answer) => match answer.status() {
                StatusCode::OK if answer.json::<Vec<Job>>().await.is_ok() => Check::Accepted,
                StatusCode::UNAUTHORIZED => Check::Refused,
                _ => Check::NotOttie,
            },
            Err(_) => Check::Unreachable,
        }
    }

    fn url(&self, path: &str) -> Result<Url> {
        Ok(self.origin.join(&format!("api/v1/{path}"))?)
    }

    /// The events stream. Connect first, then list, so nothing is missed.
    pub async fn events(&self) -> Result<Events> {
        let mut url = self.url("events")?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|()| anyhow::anyhow!("no WebSocket address for {url}"))?;
        let mut request = url.as_str().into_client_request()?;
        let offered = format!("{EVENTS_PROTOCOL}, {TOKEN_PROTOCOL}{}", self.token);
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", offered.parse()?);
        let connector = self.tls.clone().map(Connector::Rustls);
        let (events, _) =
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector)
                .await
                .context("can't follow Ottie's events")?;
        Ok(events)
    }

    pub async fn jobs(&self) -> Result<Vec<Job>> {
        Ok(self.get("jobs").await?.json().await?)
    }

    /// Where one download's file is. `id` must be a job ID (`is_job_id`).
    pub async fn placed(&self, id: &str) -> Result<Placed> {
        Ok(self.get(&format!("jobs/{id}")).await?.json().await?)
    }

    pub async fn settings(&self) -> Result<Settings> {
        Ok(self.get("settings").await?.json().await?)
    }

    pub async fn access(&self) -> Result<Access> {
        Ok(self.get("access").await?.json().await?)
    }

    pub async fn categories(&self) -> Result<Vec<Category>> {
        Ok(self.get("categories").await?.json().await?)
    }

    /// The links in some text, as the daemon reads them.
    pub async fn preview(&self, text: &str) -> Result<Vec<Found>> {
        let preview: Preview = self
            .send(
                self.http
                    .post(self.url("jobs/preview")?)
                    .json(&json!({ "text": text })),
            )
            .await?
            .json()
            .await?;
        Ok(preview.links)
    }

    /// Adds these links, each in its automatic category.
    pub async fn add(&self, urls: &[String]) -> Result<Added> {
        let jobs: Vec<_> = urls.iter().map(|url| json!({ "url": url })).collect();
        Ok(self
            .send(
                self.http
                    .post(self.url("jobs/batch")?)
                    .json(&json!({ "jobs": jobs })),
            )
            .await?
            .json()
            .await?)
    }

    pub async fn pause(&self, id: &str) -> Result<()> {
        self.post(&format!("jobs/{id}/pause")).await
    }

    pub async fn resume(&self, id: &str) -> Result<()> {
        self.post(&format!("jobs/{id}/resume")).await
    }

    /// Bytes per second for all downloads together; 0 = no limit.
    pub async fn set_global_limit(&self, limit: u64) -> Result<()> {
        let change = json!({ "globalLimit": limit });
        self.send(self.http.patch(self.url("settings")?).json(&change))
            .await?;
        Ok(())
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        self.send(self.http.get(self.url(path)?)).await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        Ok(request
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?)
    }

    async fn post(&self, path: &str) -> Result<()> {
        self.send(self.http.post(self.url(path)?)).await?;
        Ok(())
    }
}

/// Whether `id` looks like a job ID (`2026.10.13-14.28.59-1`): digits, dots
/// and dashes only, so it can't lead to another path of the API.
pub fn is_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-')
}

/// Whether the link leads to this computer (`127.0.0.1`, `::1`, `localhost`).
pub fn is_loopback(link: &Url) -> bool {
    let Some(host) = link.host_str() else {
        return false;
    };
    // An IPv6 address comes in brackets.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The token in a signed-in link's fragment (`#token=…`).
pub fn token(link: &Url) -> &str {
    link.fragment()
        .and_then(|fragment| fragment.strip_prefix("token="))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_read_their_code_and_only_the_details_used() {
        let problem = |json: &str| serde_json::from_str::<Problem>(json).unwrap();
        assert_eq!(
            problem(r#"{"code":"httpStatus","status":404}"#),
            Problem::HttpStatus { status: 404 }
        );
        assert_eq!(
            problem(r#"{"code":"stalled","seconds":30}"#),
            Problem::Stalled
        );
        assert_eq!(
            problem(r#"{"code":"fileExists","name":"a.zip"}"#),
            Problem::FileExists
        );
        assert_eq!(problem(r#"{"code":"someday"}"#), Problem::Unknown);
    }

    #[test]
    fn events_read_only_what_the_app_needs() {
        let progress: Event = serde_json::from_str(
            r#"{"type":"progress","id":"a","downloaded":5,"total":null,"ranges":[]}"#,
        )
        .unwrap();
        assert!(matches!(progress, Event::Progress { downloaded: 5, .. }));
        let view: Event = serde_json::from_str(r#"{"type":"viewRemoved","id":3}"#).unwrap();
        assert!(matches!(view, Event::Other));
        let settings: Event = serde_json::from_str(
            r#"{"type":"settingsChanged","settings":{"globalLimit":1024,"maxConcurrent":3}}"#,
        )
        .unwrap();
        assert!(matches!(
            settings,
            Event::SettingsChanged {
                settings: Settings { global_limit: 1024 }
            }
        ));
    }

    #[test]
    fn found_links_read_only_what_the_clipboard_needs() {
        let found: Preview = serde_json::from_str(
            r#"{"links":[
                {"kind":"link","url":"https://x.org/a.zip","name":"a.zip",
                 "matched":{"category":3,"extension":"zip","byFolder":false},"known":false},
                {"kind":"invalid","text":"ftp://x","refused":{"code":"unsupportedScheme"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            found.links,
            [
                Found::Link {
                    url: "https://x.org/a.zip".to_owned(),
                    matched: Matched {
                        extension: Some("zip".to_owned())
                    },
                    known: false
                },
                Found::Other
            ]
        );
    }

    #[test]
    fn only_job_ids_reach_the_api() {
        assert!(is_job_id("2026.10.13-14.28.59-1"));
        assert!(!is_job_id(""));
        assert!(!is_job_id("../settings"));
        assert!(!is_job_id("1/pause"));
    }

    #[test]
    fn only_this_computers_addresses_are_loopback() {
        let loopback = |link: &str| is_loopback(&Url::parse(link).unwrap());
        assert!(loopback("http://127.0.0.1:24068/"));
        assert!(loopback("http://[::1]:24068/"));
        assert!(loopback("http://LOCALHOST:24068/"));
        assert!(!loopback("https://nas.tail1234.ts.net/"));
        assert!(!loopback("http://10.8.0.1:24068/"));
    }

    #[test]
    fn the_api_lives_under_the_daemons_origin() {
        let link = Url::parse("http://127.0.0.1:24068/#token=abc").unwrap();
        let daemon = Daemon::new(&link).unwrap();
        assert_eq!(token(&link), "abc");
        assert_eq!(
            daemon.url("jobs/x/pause").unwrap().as_str(),
            "http://127.0.0.1:24068/api/v1/jobs/x/pause"
        );
    }
}

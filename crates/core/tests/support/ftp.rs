//! In-process FTP server for the FTP backend's tests (docs/decisions/0054):
//! one file, plain or over TLS (explicit or implicit, with the self-signed
//! certificate beside this file), with fault injection. Only what Ottie
//! sends: USER, PASS, FEAT, AUTH TLS, PBSZ, PROT, TYPE, SIZE, MDTM, PASV,
//! EPSV, REST, RETR, QUIT.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Url;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

const CERT: &[u8] = include_bytes!("ftp-cert.pem");
const KEY: &[u8] = include_bytes!("ftp-key.pem");
const CHUNK: usize = 16 * 1024;

/// How the server speaks TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tls {
    /// Plain FTP only.
    None,
    /// `AUTH TLS` offered (and listed in FEAT).
    Explicit,
    /// TLS from the first byte.
    Implicit,
}

/// What the server does. Change it with [`FtpServer::configure`].
pub struct Behavior {
    pub body: Vec<u8>,
    /// What MDTM answers; `None`: MDTM isn't supported.
    pub modified: Option<String>,
    pub size: bool,
    /// `REST STREAM` listed and honoured.
    pub rest: bool,
    pub tls: Tls,
    /// The only account; `None` takes anyone (anonymous too).
    pub account: Option<(String, String)>,
    /// More logins at once than this get 421.
    pub max_logins: Option<usize>,
    /// Close the data connection after this many bytes of a transfer, once.
    pub drop_after: Option<usize>,
    /// Pause before each 16 KiB chunk.
    pub chunk_delay: Option<Duration>,
    /// A command (`"SIZE"`, `"RETR"`…) never answered; the connection
    /// stays open.
    pub silent_on: Option<&'static str>,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            body: Vec::new(),
            modified: Some("20240102030405".to_owned()),
            size: true,
            rest: true,
            tls: Tls::None,
            account: None,
            max_logins: None,
            drop_after: None,
            chunk_delay: None,
            silent_on: None,
        }
    }
}

#[derive(Default)]
struct Shared {
    behavior: Behavior,
    /// Every command received, PASS without its argument.
    commands: Vec<String>,
    logins: usize,
    logged_in_now: usize,
}

pub struct FtpServer {
    addr: SocketAddr,
    shared: Arc<Mutex<Shared>>,
}

impl FtpServer {
    pub async fn start(behavior: Behavior) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Mutex::new(Shared {
            behavior,
            ..Shared::default()
        }));
        let acceptor = acceptor();
        let state = Arc::clone(&shared);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let state = Arc::clone(&state);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = session(socket, state, acceptor).await;
                });
            }
        });
        Self { addr, shared }
    }

    /// `scheme://127.0.0.1:port/path`, with `user:password@` if given.
    pub fn url(&self, scheme: &str, credentials: Option<(&str, &str)>, path: &str) -> Url {
        let login = credentials.map_or(String::new(), |(user, password)| {
            format!("{user}:{password}@")
        });
        Url::parse(&format!("{scheme}://{login}{}{path}", self.addr)).unwrap()
    }

    pub fn configure(&self, change: impl FnOnce(&mut Behavior)) {
        change(&mut self.shared.lock().unwrap().behavior);
    }

    pub fn commands(&self) -> Vec<String> {
        self.shared.lock().unwrap().commands.clone()
    }

    pub fn logins(&self) -> usize {
        self.shared.lock().unwrap().logins
    }

    /// The SHA-256 of the server's certificate, lowercase hex.
    pub fn fingerprint() -> String {
        let cert = CertificateDer::from_pem_slice(CERT).unwrap();
        Sha256::digest(cert.as_ref())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

fn acceptor() -> TlsAcceptor {
    let cert = CertificateDer::from_pem_slice(CERT).unwrap();
    let key = PrivateKeyDer::from_pem_slice(KEY).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

type Boxed = Pin<Box<dyn Stream>>;

async fn reply(control: &mut BufReader<Boxed>, line: &str) -> std::io::Result<()> {
    control
        .get_mut()
        .write_all(format!("{line}\r\n").as_bytes())
        .await?;
    control.get_mut().flush().await
}

async fn session(
    socket: TcpStream,
    state: Arc<Mutex<Shared>>,
    acceptor: TlsAcceptor,
) -> std::io::Result<()> {
    let tls = state.lock().unwrap().behavior.tls;
    let stream: Boxed = if tls == Tls::Implicit {
        Box::pin(acceptor.accept(socket).await?)
    } else {
        Box::pin(socket)
    };
    let mut control = BufReader::new(stream);
    reply(&mut control, "220 Test FTP").await?;
    let mut user = String::new();
    let mut logged_in = false;
    let mut protected = tls == Tls::Implicit;
    let mut passive: Option<TcpListener> = None;
    let mut rest = 0usize;
    let result = async {
        loop {
            let mut line = String::new();
            if control.read_line(&mut line).await? == 0 {
                return Ok(());
            }
            let line = line.trim_end().to_owned();
            let (command, argument) = line.split_once(' ').unwrap_or((&line, ""));
            let command = command.to_ascii_uppercase();
            let shown = if command == "PASS" {
                "PASS".to_owned()
            } else {
                line.clone()
            };
            let silent = {
                let mut shared = state.lock().unwrap();
                shared.commands.push(shown);
                shared.behavior.silent_on == Some(command.as_str())
            };
            if silent {
                std::future::pending::<()>().await;
            }
            match command.as_str() {
                "FEAT" => {
                    let text = {
                        let behavior = &state.lock().unwrap().behavior;
                        let mut lines = vec!["211-Features:".to_owned()];
                        if behavior.tls == Tls::Explicit {
                            lines.push(" AUTH TLS".to_owned());
                            lines.push(" PBSZ".to_owned());
                            lines.push(" PROT".to_owned());
                        }
                        if behavior.size {
                            lines.push(" SIZE".to_owned());
                        }
                        if behavior.modified.is_some() {
                            lines.push(" MDTM".to_owned());
                        }
                        if behavior.rest {
                            lines.push(" REST STREAM".to_owned());
                        }
                        lines.push("211 End".to_owned());
                        lines.join("\r\n")
                    };
                    reply(&mut control, &text).await?;
                }
                "AUTH" if tls == Tls::Explicit => {
                    reply(&mut control, "234 AUTH TLS OK").await?;
                    let plain = std::mem::replace(
                        &mut control,
                        BufReader::new(Box::pin(tokio::io::duplex(1).0)),
                    )
                    .into_inner();
                    let secured = acceptor.accept(plain).await?;
                    control = BufReader::new(Box::pin(secured));
                }
                "AUTH" => reply(&mut control, "502 No TLS here").await?,
                "PBSZ" => reply(&mut control, "200 PBSZ=0").await?,
                "PROT" => {
                    protected = argument.eq_ignore_ascii_case("P");
                    reply(&mut control, "200 PROT ok").await?;
                }
                "USER" => {
                    user = argument.to_owned();
                    reply(&mut control, "331 Password please").await?;
                }
                "PASS" => {
                    let accepted = {
                        let mut shared = state.lock().unwrap();
                        let fits = shared
                            .behavior
                            .max_logins
                            .is_none_or(|max| shared.logged_in_now < max);
                        let account_ok = shared
                            .behavior
                            .account
                            .as_ref()
                            .is_none_or(|(u, p)| *u == user && p == argument);
                        match (fits, account_ok) {
                            (false, _) => Err("421 Too many connections"),
                            (true, false) => Err("530 Login incorrect"),
                            (true, true) => {
                                shared.logins += 1;
                                shared.logged_in_now += 1;
                                Ok(())
                            }
                        }
                    };
                    match accepted {
                        Ok(()) => {
                            logged_in = true;
                            reply(&mut control, "230 Logged in").await?;
                        }
                        Err(answer) => {
                            reply(&mut control, answer).await?;
                            if answer.starts_with("421") {
                                return Ok(());
                            }
                        }
                    }
                }
                _ if !logged_in && command != "QUIT" => {
                    reply(&mut control, "530 Log in first").await?;
                }
                "TYPE" => reply(&mut control, "200 Type set").await?,
                "SIZE" => {
                    let size = {
                        let behavior = &state.lock().unwrap().behavior;
                        behavior.size.then_some(behavior.body.len())
                    };
                    match size {
                        Some(size) if argument == "file.bin" || argument.ends_with("/file.bin") => {
                            reply(&mut control, &format!("213 {size}")).await?;
                        }
                        Some(_) => reply(&mut control, "550 No such file").await?,
                        None => reply(&mut control, "502 No SIZE").await?,
                    }
                }
                "MDTM" => {
                    let modified = state.lock().unwrap().behavior.modified.clone();
                    match modified {
                        Some(time) => reply(&mut control, &format!("213 {time}")).await?,
                        None => reply(&mut control, "502 No MDTM").await?,
                    }
                }
                "PASV" | "EPSV" => {
                    let listener = TcpListener::bind("127.0.0.1:0").await?;
                    let port = listener.local_addr()?.port();
                    passive = Some(listener);
                    let answer = if command == "PASV" {
                        format!(
                            "227 Entering Passive Mode (10,0,0,1,{},{})",
                            port / 256,
                            port % 256
                        )
                    } else {
                        format!("229 Entering Extended Passive Mode (|||{port}|)")
                    };
                    reply(&mut control, &answer).await?;
                }
                "REST" => {
                    let rest_ok = state.lock().unwrap().behavior.rest;
                    match argument.parse() {
                        Ok(offset) if rest_ok => {
                            rest = offset;
                            reply(&mut control, "350 Restarting").await?;
                        }
                        _ => reply(&mut control, "502 No REST").await?,
                    }
                }
                "RETR" => {
                    let Some(listener) = passive.take() else {
                        reply(&mut control, "425 PASV first").await?;
                        continue;
                    };
                    if argument != "file.bin" && !argument.ends_with("/file.bin") {
                        reply(&mut control, "550 No such file").await?;
                        continue;
                    }
                    let offset = std::mem::take(&mut rest);
                    reply(&mut control, "150 Sending").await?;
                    let (socket, _) = listener.accept().await?;
                    let data: Boxed = if protected {
                        Box::pin(acceptor.accept(socket).await?)
                    } else {
                        Box::pin(socket)
                    };
                    let complete = send(data, &state, offset).await;
                    let answer = if complete {
                        "226 Transfer complete"
                    } else {
                        "426 Connection closed; transfer aborted"
                    };
                    reply(&mut control, answer).await?;
                }
                "QUIT" => {
                    reply(&mut control, "221 Bye").await?;
                    return Ok(());
                }
                _ => reply(&mut control, "502 Not implemented").await?,
            }
        }
    }
    .await;
    if logged_in {
        state.lock().unwrap().logged_in_now -= 1;
    }
    result
}

/// Sends the file from `offset`. False if it stopped early (on purpose,
/// or because the client closed the connection).
async fn send(mut data: Boxed, state: &Arc<Mutex<Shared>>, offset: usize) -> bool {
    let (body, delay, drop_after) = {
        let mut shared = state.lock().unwrap();
        let behavior = &mut shared.behavior;
        (
            behavior.body.clone(),
            behavior.chunk_delay,
            behavior.drop_after.take(),
        )
    };
    let rest = body.get(offset..).unwrap_or_default();
    let (rest, complete) = match drop_after {
        Some(after) if after < rest.len() => (&rest[..after], false),
        _ => (rest, true),
    };
    for chunk in rest.chunks(CHUNK) {
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if data.write_all(chunk).await.is_err() {
            return false;
        }
    }
    if complete {
        // TLS: close_notify, so the client sees a clean end.
        let _ = data.shutdown().await;
    }
    // Dropped without it otherwise: the client sees the connection cut.
    complete
}

//! FTP and FTPS downloads (docs/decisions/0054). Links follow FileZilla's
//! names: `ftp://` uses TLS when the server offers it, `ftpes://` requires it
//! (explicit, `AUTH TLS`), `ftps://` speaks TLS from the first byte (implicit,
//! port 990). The user name and password come from the link, else anonymous.
//!
//! A certificate counts if the system trusts it, or if the person trusted
//! that exact one for that server (`Download::trusted`); otherwise the run
//! fails with its fingerprint, so it can be trusted.
//!
//! A file can be resumed and split when the server gives its size and
//! modification time (`SIZE`, `MDTM`) and supports `REST STREAM`: each range
//! is its own login, `REST` to its start,
//! `RETR`, and closed once it reaches its end. The size and modification time
//! (`MDTM`) are asked again before each one and compared, as HTTP's `If-Range`.

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use percent_encoding::percent_decode_str;
use reqwest::Url;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use suppaftp::tokio::{
    AsyncRustlsConnector, AsyncRustlsStream, ImplAsyncFtpStream, TransferStream,
};
use suppaftp::types::FileType;
use suppaftp::{FtpError, Mode, Status};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncReadExt;

use crate::download::{Download, Event, Outcome, RestartReason, Validator};
use crate::error::Error;
use crate::filename;
use crate::segmented::{self, Segmented};
use crate::transfer::{
    self, Body, Env, Same, Source, complete, redact, restarting, single, splittable, started,
};

type Ftp = ImplAsyncFtpStream<AsyncRustlsStream>;

/// Who logs in when the link names nobody.
const ANONYMOUS: (&str, &str) = ("anonymous", "ottie@");
/// How long a range that was refused for too many connections waits before
/// asking once more.
const BUSY_WAIT: Duration = Duration::from_millis(500);
/// Bytes read from the data connection at a time.
const READ_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Security {
    /// `ftp://`: TLS if the server offers it.
    WhenOffered,
    /// `ftpes://`: `AUTH TLS`, or nothing.
    Required,
    /// `ftps://`: TLS from the first byte.
    Implicit,
}

/// Where a link points and how to log in there.
#[derive(Clone, PartialEq, Eq)]
struct Target {
    /// As in the link, lowercase (IPv6 in brackets): what certificates are
    /// trusted for.
    host: String,
    port: u16,
    security: Security,
    user: String,
    password: String,
    /// The file, relative to where the login starts (`ftp://host/%2Fetc/x`
    /// for an absolute one), as RFC 1738 has it.
    path: String,
}

impl Target {
    fn of(url: &Url) -> Result<Self, Error> {
        let security = match url.scheme() {
            "ftp" => Security::WhenOffered,
            "ftpes" => Security::Required,
            "ftps" => Security::Implicit,
            other => return Err(Error::UnsupportedScheme(other.to_owned())),
        };
        let host = url
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or(Error::InvalidUrl)?
            .to_ascii_lowercase();
        let port = url.port().unwrap_or(match security {
            Security::Implicit => 990,
            _ => 21,
        });
        let (user, password) = if url.username().is_empty() {
            (ANONYMOUS.0.to_owned(), ANONYMOUS.1.to_owned())
        } else {
            (
                decoded(url.username())?,
                decoded(url.password().unwrap_or(""))?,
            )
        };
        Ok(Self {
            host,
            port,
            security,
            user,
            password,
            path: path_of(url)?,
        })
    }

    /// The host name to connect to and check certificates against (an IPv6
    /// address without its brackets).
    fn name(&self) -> &str {
        self.host.trim_start_matches('[').trim_end_matches(']')
    }
}

/// Percent-decoded, and nothing that could end a command line.
fn decoded(text: &str) -> Result<String, Error> {
    let text = percent_decode_str(text)
        .decode_utf8()
        .map(Cow::into_owned)
        .map_err(|_| Error::InvalidUrl)?;
    if text.chars().any(char::is_control) {
        return Err(Error::InvalidUrl);
    }
    Ok(text)
}

/// The file's path from the link: without the first `/` and a `;type=…`
/// ending, decoded.
fn path_of(url: &Url) -> Result<String, Error> {
    let raw = url.path();
    let raw = match raw.rsplit_once(';') {
        Some((path, kind)) if kind.to_ascii_lowercase().starts_with("type=") => path,
        _ => raw,
    };
    let path = decoded(raw.strip_prefix('/').unwrap_or(raw))?;
    if path.is_empty() || path.ends_with('/') {
        // A folder: nothing to download.
        return Err(Error::InvalidUrl);
    }
    Ok(path)
}

/// Whether the link can be downloaded: a file, a server, nothing that could
/// end a command line.
pub(crate) fn check(url: &Url) -> Result<(), Error> {
    Target::of(url).map(|_| ())
}

/// The name a link suggests for its file.
pub(crate) fn suggested_name(url: &Url) -> String {
    let path = path_of(url).unwrap_or_default();
    let last = path.rsplit('/').next().unwrap_or("");
    filename::sanitize(last)
}

/// Checks certificates as the system does, but also accepts those trusted by
/// hand, and remembers the fingerprint of one it refused.
#[derive(Debug)]
struct Verifier {
    system: rustls_platform_verifier::Verifier,
    trusted: Vec<String>,
    refused: Mutex<Option<String>>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.system.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(err @ rustls::Error::InvalidCertificate(_)) => {
                let fingerprint = fingerprint(end_entity);
                if self.trusted.contains(&fingerprint) {
                    return Ok(ServerCertVerified::assertion());
                }
                *self.refused.lock().unwrap_or_else(PoisonError::into_inner) = Some(fingerprint);
                Err(err)
            }
            other => other,
        }
    }

    // The handshake's signatures are always checked: a certificate trusted
    // by hand still proves the server holds its key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.system.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.system.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.system.supported_verify_schemes()
    }
}

/// A certificate's SHA-256, lowercase hex.
fn fingerprint(certificate: &CertificateDer<'_>) -> String {
    Sha256::digest(certificate.as_ref())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One download's way to its server: the link, its TLS settings (shared by
/// every connection of a run, so data connections resume the control
/// connection's TLS session, as many servers require) and the stall timeout.
pub(crate) struct Link {
    target: Target,
    connector: tokio_rustls::TlsConnector,
    verifier: Arc<Verifier>,
    stall: Duration,
}

impl Link {
    pub(crate) fn new(dl: &Download, stall: Duration) -> Result<Self, Error> {
        let target = Target::of(&dl.url)?;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = Arc::new(Verifier {
            system: rustls_platform_verifier::Verifier::new(Arc::clone(&provider))
                .map_err(|err| Error::Ftp(format!("TLS can't be set up: {err}")))?,
            trusted: dl.trusted.clone(),
            refused: Mutex::new(None),
        });
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|err| Error::Ftp(format!("TLS can't be set up: {err}")))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::clone(&verifier) as Arc<dyn ServerCertVerifier>)
            .with_no_client_auth();
        Ok(Self {
            target,
            connector: tokio_rustls::TlsConnector::from(Arc::new(config)),
            verifier,
            stall,
        })
    }

    /// Runs one exchange with the server, at most `stall` long.
    async fn step<T>(&self, work: impl Future<Output = Result<T, FtpError>>) -> Result<T, Error> {
        match tokio::time::timeout(self.stall, work).await {
            Ok(result) => result.map_err(|err| self.error(err)),
            Err(_) => Err(Error::Stalled(self.stall)),
        }
    }

    fn error(&self, err: FtpError) -> Error {
        if let FtpError::SecureError(_) = &err
            && let Some(fingerprint) = self
                .verifier
                .refused
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            return Error::UntrustedCertificate {
                host: self.target.host.clone(),
                fingerprint,
            };
        }
        match err {
            FtpError::UnexpectedResponse(response) => Error::FtpReply {
                code: u16::try_from(response.status.code()).unwrap_or(0),
                text: reply_text(&response.body),
            },
            FtpError::ConnectionError(err) => Error::Ftp(err.to_string()),
            FtpError::SecureError(text) => Error::Ftp(format!("TLS: {text}")),
            FtpError::BadResponse => Error::Ftp("the server's answer can't be read".to_owned()),
            other => Error::Ftp(other.to_string()),
        }
    }

    fn tls(&self) -> AsyncRustlsConnector {
        AsyncRustlsConnector::from(self.connector.clone())
    }

    /// Connects (with TLS as the link asks), logs in and switches to binary.
    /// Also tells whether the server can start a transfer where asked.
    async fn connect(&self) -> Result<(Ftp, bool), Error> {
        let target = &self.target;
        let address = self.address().await?;
        let mut ftp = match target.security {
            Security::Implicit => {
                let mut ftp = self
                    .step(Ftp::connect_secure_implicit(
                        address,
                        self.tls(),
                        target.name(),
                    ))
                    .await?;
                // Data connections over TLS too: implicit TLS doesn't say so.
                self.step(ftp.custom_command("PBSZ 0", &[Status::CommandOk]))
                    .await?;
                self.step(ftp.custom_command("PROT P", &[Status::CommandOk]))
                    .await?;
                ftp
            }
            _ => self.step(Ftp::connect(address)).await?,
        };
        // Some servers list their features only once logged in.
        let mut features = self.step(ftp.feat()).await.ok();
        let offered = features.as_ref().is_some_and(|features| {
            features
                .get("AUTH")
                .is_some_and(|value| value.as_deref().unwrap_or("").contains("TLS"))
        });
        ftp = match target.security {
            Security::Implicit => ftp,
            Security::Required | Security::WhenOffered if offered => {
                self.step(ftp.into_secure(self.tls(), target.name()))
                    .await?
            }
            Security::Required => {
                // It may still take `AUTH TLS` without listing it.
                match self.step(ftp.into_secure(self.tls(), target.name())).await {
                    Err(Error::FtpReply { .. }) => return Err(Error::NoTls),
                    other => other?,
                }
            }
            Security::WhenOffered => ftp,
        };
        // A PASV answer may name an address only valid on the server's own
        // network: the data connection goes to the server's address instead.
        // EPSV, which names no address, for IPv6.
        if address.is_ipv6() {
            ftp.set_mode(Mode::ExtendedPassive);
        } else {
            ftp.set_mode(Mode::Passive);
            ftp.set_passive_nat_workaround(true);
        }
        self.step(ftp.login(target.user.as_str(), target.password.as_str()))
            .await?;
        if features.as_ref().is_none_or(|features| features.is_empty()) {
            features = self.step(ftp.feat()).await.ok();
        }
        self.step(ftp.transfer_type(FileType::Binary)).await?;
        let ranges = features
            .as_ref()
            .and_then(|features| features.get("REST"))
            .is_some_and(|value| {
                value
                    .as_deref()
                    .unwrap_or("")
                    .eq_ignore_ascii_case("STREAM")
            });
        Ok((ftp, ranges))
    }

    async fn address(&self) -> Result<SocketAddr, Error> {
        let target = &self.target;
        let found = tokio::time::timeout(
            self.stall,
            tokio::net::lookup_host((target.name(), target.port)),
        )
        .await
        .map_err(|_| Error::Stalled(self.stall))?
        .map_err(|err| Error::Ftp(err.to_string()))?
        .next();
        found.ok_or_else(|| Error::Ftp(format!("no address for {}", target.host)))
    }

    /// The file's size and what proves it's unchanged, if the server says.
    async fn facts(&self, ftp: &mut Ftp, ranges: bool) -> Result<Facts, Error> {
        let path = self.target.path.as_str();
        let size = match self.step(ftp.size(path)).await {
            Ok(size) => Some(size as u64),
            // Not every server has SIZE; RETR will say if the file is missing.
            Err(Error::FtpReply { .. } | Error::Ftp(_)) => None,
            Err(err) => return Err(err),
        };
        let modified = match self
            .step(ftp.custom_command(format!("MDTM {path}"), &[Status::File]))
            .await
        {
            Ok(response) => Some(reply_text(&response.body)),
            Err(Error::FtpReply { .. }) => None,
            Err(err) => return Err(err),
        };
        // Both, or nothing proves the file unchanged: another version may
        // have the same size. Without them, no resume and no splitting:
        // each run starts from zero (docs/decisions/0054).
        let modified = modified.filter(|text| !text.is_empty());
        let validator = size
            .zip(modified)
            .map(|(size, modified)| Validator::Ftp(format!("{size} {modified}")));
        Ok(Facts {
            size,
            ranges: ranges && validator.is_some(),
            validator,
        })
    }

    /// Starts sending the file from `offset`.
    async fn retrieve(&self, mut ftp: Ftp, offset: u64) -> Result<Transfer, Error> {
        if offset > 0 {
            let offset = usize::try_from(offset).map_err(|_| Error::Ftp("file too big".into()))?;
            self.step(ftp.resume_transfer(offset)).await?;
        }
        let data = self
            .step(ftp.retr_as_stream(self.target.path.as_str()))
            .await?;
        Ok(Transfer { data, ftp })
    }

    /// One range of a multi-connection download, from `pos` to wherever
    /// the caller stops reading. `Ok(Err(_))`: the file changed.
    pub(crate) async fn range(
        &self,
        pos: u64,
        same: Same<'_>,
    ) -> Result<Result<Body, RestartReason>, Error> {
        let (ftp, facts) = match connect_for_facts(self).await {
            // Too many connections: servers often still count one just
            // closed (the range before), so once more after a moment.
            Err(Error::FtpReply { code: 421, .. }) => {
                tokio::time::sleep(BUSY_WAIT).await;
                connect_for_facts(self).await?
            }
            other => other?,
        };
        if facts.validator.as_ref() != Some(same.validator) || facts.size != Some(same.total) {
            return Ok(Err(RestartReason::RemoteChanged));
        }
        if !facts.ranges {
            return Ok(Err(RestartReason::RangesIgnored));
        }
        let transfer = self.retrieve(ftp, pos).await?;
        Ok(Ok(Body::Ftp(Box::new(transfer))))
    }
}

/// The text of a server's reply, without its code.
fn reply_text(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    let text = text.get(3..).map_or(text, |rest| {
        if text.as_bytes()[..3].iter().all(u8::is_ascii_digit) {
            rest
        } else {
            text
        }
    });
    text.trim_start_matches(['-', ' ']).trim().to_owned()
}

/// What the server says of the file.
struct Facts {
    size: Option<u64>,
    validator: Option<Validator>,
    /// It has a size and a modification time, and the server can start
    /// where asked.
    ranges: bool,
}

/// A file being sent: its data connection, and the control connection it
/// belongs to (dropped together, which closes both).
pub(crate) struct Transfer {
    data: TransferStream<AsyncRustlsStream>,
    ftp: Ftp,
}

impl Transfer {
    pub(crate) async fn chunk(&mut self) -> Result<Option<Bytes>, Error> {
        let mut buffer = BytesMut::with_capacity(READ_SIZE);
        let read = self
            .data
            .read_buf(&mut buffer)
            .await
            .map_err(|err| Error::Ftp(err.to_string()))?;
        Ok((read > 0).then(|| buffer.freeze()))
    }

    /// The server's verdict once the data connection closed: 226 if it sent
    /// the whole file.
    pub(crate) async fn finish(self, stall: Duration) -> Result<(), Error> {
        let Transfer { data, ftp } = self;
        let finished = tokio::time::timeout(stall, data.finish())
            .await
            .map_err(|_| Error::Stalled(stall))?;
        drop(ftp);
        finished.map_err(|err| match err {
            FtpError::UnexpectedResponse(response) => Error::FtpReply {
                code: u16::try_from(response.status.code()).unwrap_or(0),
                text: reply_text(&response.body),
            },
            other => Error::Ftp(other.to_string()),
        })
    }
}

/// Downloads into `<name>.part.ottie` until the file is complete or `cancel`
/// fires, as [`crate::http::run`] does for HTTP.
pub(crate) async fn run_in(
    env: &Env<'_>,
    dl: &mut Download,
    mut on_event: impl FnMut(Event),
) -> Result<Outcome, Error> {
    let link = Arc::new(Link::new(dl, env.config.stall_timeout)?);
    let (name, part) = match (dl.file_name.clone(), dl.part_path()) {
        (Some(name), Some(part)) => (name, part),
        _ => return fresh(env, &link, dl, &mut on_event).await,
    };

    let on_disk = match fs::metadata(&part).await {
        Ok(meta) => Some(meta.len()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };
    let reason = match on_disk {
        None => RestartReason::PartFileMissing,
        Some(_) if !dl.resumable || dl.validator.is_none() => RestartReason::NotResumable,
        Some(length) if !dl.segments.is_empty() && Some(length) != dl.total => {
            RestartReason::PartFileDamaged
        }
        Some(_) if !dl.segments.is_empty() => {
            dl.downloaded = dl.segments.iter().map(|s| s.done).sum();
            started(dl, &name, &mut on_event);
            let source = Source::Ftp(Arc::clone(&link));
            match segmented::run(env, source, dl, &part, None, &mut on_event).await? {
                Segmented::Finished => return complete(dl, &name).await,
                Segmented::Cancelled => return Ok(Outcome::Paused),
                Segmented::Restart(reason) => reason,
            }
        }
        Some(offset) => {
            let (ftp, facts) = connect_for_facts(&link).await?;
            if facts.validator != dl.validator {
                RestartReason::RemoteChanged
            } else if !facts.ranges {
                RestartReason::RangesIgnored
            } else if dl.total == Some(offset) {
                // Complete: as for HTTP's 416, a file at its full length
                // with no map was written in order (a restart empties it).
                dl.downloaded = offset;
                started(dl, &name, &mut on_event);
                return complete(dl, &name).await;
            } else {
                let transfer = link.retrieve(ftp, offset).await?;
                let file = OpenOptions::new().append(true).open(&part).await?;
                dl.downloaded = offset;
                started(dl, &name, &mut on_event);
                let body = Body::Ftp(Box::new(transfer));
                return single(env, body, file, dl, &name, &mut on_event).await;
            }
        }
    };

    restarting(dl, reason, &mut on_event).await?;
    let (ftp, facts) = connect_for_facts(&link).await?;
    learn(dl, &facts);
    let file = File::create(&part).await?;
    transfer(env, &link, ftp, facts.ranges, file, dl, name, &mut on_event).await
}

/// The first run of a download: learns about the file, names it.
async fn fresh(
    env: &Env<'_>,
    link: &Arc<Link>,
    dl: &mut Download,
    on_event: &mut impl FnMut(Event),
) -> Result<Outcome, Error> {
    let (ftp, facts) = connect_for_facts(link).await?;
    let (name, file) = transfer::reserve(dl, suggested_name(&dl.url)).await?;
    tracing::debug!(url = %redact(&dl.url), %name, "starting download");
    learn(dl, &facts);
    transfer(env, link, ftp, facts.ranges, file, dl, name, on_event).await
}

fn learn(dl: &mut Download, facts: &Facts) {
    dl.total = facts.size;
    dl.validator = facts.validator.clone();
    dl.resumable = facts.ranges;
    dl.downloaded = 0;
}

/// Fetches the whole file from the start, over several connections when
/// it's worth it.
#[allow(clippy::too_many_arguments)]
async fn transfer(
    env: &Env<'_>,
    link: &Arc<Link>,
    ftp: Ftp,
    ranges: bool,
    file: File,
    dl: &mut Download,
    name: String,
    on_event: &mut impl FnMut(Event),
) -> Result<Outcome, Error> {
    let (mut ftp, mut ranges, mut file) = (ftp, ranges, file);
    // Loops at most twice: once more if the file changes on the server in
    // the middle of a multi-connection download.
    let mut restarted_mid_run = false;
    loop {
        let first = Body::Ftp(Box::new(link.retrieve(ftp, 0).await?));
        let Some(total) = splittable(env.config, dl, ranges) else {
            started(dl, &name, on_event);
            tracing::debug!(%name, "one connection");
            return single(env, first, file, dl, &name, on_event).await;
        };
        // Full size up front, as for HTTP (see http.rs).
        file.set_len(total).await?;
        drop(file);
        dl.segments = segmented::plan(total, env.config);
        started(dl, &name, on_event);
        let part = dl.dest_dir.join(filename::part_name(&name));
        let source = Source::Ftp(Arc::clone(link));
        match segmented::run(env, source, dl, &part, Some(first), on_event).await? {
            Segmented::Finished => return complete(dl, &name).await,
            Segmented::Cancelled => return Ok(Outcome::Paused),
            Segmented::Restart(reason) if !restarted_mid_run => {
                restarted_mid_run = true;
                restarting(dl, reason, on_event).await?;
                let facts;
                (ftp, facts) = connect_for_facts(link).await?;
                learn(dl, &facts);
                ranges = facts.ranges;
                file = File::create(&part).await?;
            }
            Segmented::Restart(_) => return Err(Error::RemoteUnstable),
        }
    }
}

async fn connect_for_facts(link: &Link) -> Result<(Ftp, Facts), Error> {
    let (mut ftp, ranges) = link.connect().await?;
    let facts = link.facts(&mut ftp, ranges).await?;
    Ok((ftp, facts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(link: &str) -> Target {
        Target::of(&Url::parse(link).unwrap()).unwrap()
    }

    #[test]
    fn reads_the_link() {
        let nas = target("ftp://me:p%40ss@NAS.local/films/My%20film.mkv");
        assert_eq!(
            (nas.host.as_str(), nas.port, nas.security),
            ("nas.local", 21, Security::WhenOffered)
        );
        assert_eq!((nas.user.as_str(), nas.password.as_str()), ("me", "p@ss"));
        assert_eq!(nas.path, "films/My film.mkv");

        let implicit = target("ftps://[::1]/%2Fetc/file.bin;type=i");
        assert_eq!(implicit.port, 990);
        assert_eq!(implicit.name(), "::1");
        assert_eq!(implicit.path, "/etc/file.bin");
        assert_eq!(
            (implicit.user.as_str(), implicit.password.as_str()),
            ANONYMOUS
        );
        assert_eq!(target("ftpes://h:2121/a").security, Security::Required);
        assert_eq!(target("ftpes://h:2121/a").port, 2121);
    }

    #[test]
    fn refuses_folders_and_line_breaks() {
        for link in [
            "ftp://h/",
            "ftp://h/pub/",
            "ftp://h/a%0d%0aDELE%20b",
            "ftp://us%0aer@h/a",
        ] {
            let url = Url::parse(link).unwrap();
            assert!(matches!(Target::of(&url), Err(Error::InvalidUrl)), "{link}");
        }
    }

    #[test]
    fn names_the_file_from_the_link() {
        let url = Url::parse("ftp://h/pub/Caf%C3%A9.iso;type=i").unwrap();
        assert_eq!(suggested_name(&url), "Café.iso");
    }

    #[test]
    fn replies_lose_their_code() {
        assert_eq!(reply_text(b"550 No such file"), "No such file");
        assert_eq!(reply_text(b"213 20240102030405"), "20240102030405");
        assert_eq!(reply_text(b"530-Login incorrect."), "Login incorrect.");
    }
}

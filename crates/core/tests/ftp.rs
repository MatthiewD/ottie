#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The FTP backend against an in-process FTP server (docs/decisions/0054).

#[path = "support/ftp.rs"]
mod ftp_server;
mod support;

use std::time::Duration;

use ftp_server::{Behavior, FtpServer, Tls};
use ottie_core::{
    Backend, Download, Error, Event, HttpConfig, NativeBackend, Outcome, PART_SUFFIX, Playback,
    RestartReason, Throttle,
};
use support::test_data;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const ONE: HttpConfig = HttpConfig {
    connections: 1,
    min_split: 64 * 1024,
    stall_timeout: Duration::from_secs(10),
    piece_time: Duration::from_secs(2),
    starved_after: Duration::from_secs(20),
};
const SEVERAL: HttpConfig = HttpConfig {
    connections: 4,
    ..ONE
};

struct Setup {
    server: FtpServer,
    dir: TempDir,
    data: Vec<u8>,
}

async fn setup(size: usize, change: impl FnOnce(&mut Behavior)) -> Setup {
    let data = test_data(size, 11);
    let mut behavior = Behavior {
        body: data.clone(),
        ..Behavior::default()
    };
    change(&mut behavior);
    Setup {
        server: FtpServer::start(behavior).await,
        dir: tempfile::tempdir().unwrap(),
        data,
    }
}

impl Setup {
    fn download(&self, scheme: &str, credentials: Option<(&str, &str)>) -> Download {
        let url = self.server.url(scheme, credentials, "/pub/file.bin");
        Download::new(url, self.dir.path()).unwrap()
    }

    /// Runs until done, or paused once `pause_at` bytes arrived.
    async fn run(
        &self,
        config: HttpConfig,
        dl: &mut Download,
        pause_at: Option<u64>,
    ) -> (Result<Outcome, Error>, Vec<Event>) {
        self.run_seeing(config, dl, pause_at, |_| {}).await
    }

    /// [`Setup::run`], showing `seeing` each event as it comes.
    async fn run_seeing(
        &self,
        config: HttpConfig,
        dl: &mut Download,
        pause_at: Option<u64>,
        mut seeing: impl FnMut(&Event) + Send,
    ) -> (Result<Outcome, Error>, Vec<Event>) {
        let backend = NativeBackend::with_config(config).unwrap();
        let cancel = CancellationToken::new();
        let mut events = Vec::new();
        let mut on_event = |event: Event| {
            if let Event::Progress { downloaded, .. } = &event
                && pause_at.is_some_and(|at| *downloaded >= at)
            {
                cancel.cancel();
            }
            seeing(&event);
            events.push(event);
        };
        let result = backend
            .run(
                dl,
                &Throttle::unlimited(),
                &Playback::none(),
                &cancel,
                &mut on_event,
            )
            .await;
        (result, events)
    }

    fn assert_complete(&self, result: Result<Outcome, Error>) {
        let Ok(Outcome::Completed { path }) = result else {
            panic!("expected completion, got {result:?}");
        };
        assert_eq!(path.file_name().unwrap(), "file.bin");
        assert!(
            std::fs::read(&path).unwrap() == self.data,
            "the file differs"
        );
    }

    fn part_len(&self) -> u64 {
        let part = self.dir.path().join(format!("file.bin{PART_SUFFIX}"));
        std::fs::metadata(part).unwrap().len()
    }
}

#[tokio::test]
async fn downloads_anonymously() {
    let s = setup(300 * 1024, |_| {}).await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(ONE, &mut dl, None).await;
    s.assert_complete(result);
    let commands = s.server.commands();
    assert!(
        commands.contains(&"USER anonymous".to_owned()),
        "{commands:?}"
    );
    assert!(commands.contains(&"TYPE I".to_owned()), "{commands:?}");
    assert!(
        commands.contains(&"RETR pub/file.bin".to_owned()),
        "{commands:?}"
    );
    // The PASV answer named another address: the server's was used.
    assert_eq!(
        dl.validator.as_ref().unwrap().header_value(),
        "307200 20240102030405"
    );
}

#[tokio::test]
async fn logs_in_with_the_link_s_account() {
    let s = setup(10 * 1024, |b| {
        b.account = Some(("me".into(), "p@ss word".into()))
    })
    .await;
    let mut dl = s.download("ftp", Some(("me", "p%40ss%20word")));
    let (result, _) = s.run(ONE, &mut dl, None).await;
    s.assert_complete(result);

    let mut wrong = s.download("ftp", Some(("me", "nope")));
    let (result, _) = s.run(ONE, &mut wrong, None).await;
    let err = result.unwrap_err();
    assert!(matches!(&err, Error::FtpReply { code: 530, .. }), "{err:?}");
    assert!(!err.is_retryable());
    // Never in what's logged or shown.
    assert!(!err.to_string().contains("nope"));
}

#[tokio::test]
async fn a_missing_file_fails_for_good() {
    let s = setup(1024, |_| {}).await;
    let url = s.server.url("ftp", None, "/other.bin");
    let mut dl = Download::new(url, s.dir.path()).unwrap();
    let (result, _) = s.run(ONE, &mut dl, None).await;
    let err = result.unwrap_err();
    assert!(matches!(&err, Error::FtpReply { code: 550, .. }), "{err:?}");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn splits_across_several_logins() {
    let s = setup(1024 * 1024, |b| {
        b.chunk_delay = Some(Duration::from_millis(5))
    })
    .await;
    let mut dl = s.download("ftp", None);
    let (result, events) = s.run(SEVERAL, &mut dl, None).await;
    s.assert_complete(result);
    let started = events.iter().find_map(|e| match e {
        Event::Started { segments, .. } => Some(segments.len()),
        _ => None,
    });
    assert_eq!(started, Some(4));
    assert!(s.server.logins() >= 4, "{} logins", s.server.logins());
    let rests = s
        .server
        .commands()
        .into_iter()
        .filter(|c| c.starts_with("REST "))
        .count();
    assert!(rests >= 3, "{rests} REST");
}

#[tokio::test]
async fn a_server_allowing_fewer_logins_still_completes() {
    let s = setup(1024 * 1024, |b| {
        b.max_logins = Some(2);
        b.chunk_delay = Some(Duration::from_millis(5));
    })
    .await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(SEVERAL, &mut dl, None).await;
    s.assert_complete(result);
}

#[tokio::test]
async fn resumes_where_it_stopped() {
    let s = setup(300 * 1024, |b| b.drop_after = Some(100 * 1024)).await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(ONE, &mut dl, None).await;
    let err = result.unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
    assert_eq!(s.part_len(), dl.downloaded);
    assert!(dl.resumable);

    let (result, events) = s.run(ONE, &mut dl, None).await;
    s.assert_complete(result);
    assert!(!events.iter().any(|e| matches!(e, Event::Restarted(_))));
    let commands = s.server.commands();
    assert!(
        commands
            .iter()
            .any(|c| c.starts_with("REST ") && c != "REST 0"),
        "{commands:?}"
    );
}

#[tokio::test]
async fn pauses_and_resumes_over_several_connections() {
    let s = setup(1024 * 1024, |b| {
        b.chunk_delay = Some(Duration::from_millis(50))
    })
    .await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(SEVERAL, &mut dl, Some(200 * 1024)).await;
    assert_eq!(result.unwrap(), Outcome::Paused);
    assert!(!dl.segments.is_empty());

    s.server.configure(|b| b.chunk_delay = None);
    let (result, events) = s.run(SEVERAL, &mut dl, None).await;
    s.assert_complete(result);
    assert!(!events.iter().any(|e| matches!(e, Event::Restarted(_))));
}

#[tokio::test]
async fn a_failed_restart_never_completes_a_file_with_holes() {
    let s = setup(1024 * 1024, |b| {
        b.chunk_delay = Some(Duration::from_millis(50))
    })
    .await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(SEVERAL, &mut dl, Some(200 * 1024)).await;
    assert_eq!(result.unwrap(), Outcome::Paused);
    assert_eq!(s.part_len(), 1024 * 1024);

    // The file seems changed, so the run starts over; then no login works.
    s.server.configure(|b| {
        b.chunk_delay = None;
        b.modified = Some("20250101000000".to_owned());
    });
    let (result, events) = s
        .run_seeing(SEVERAL, &mut dl, None, |event| {
            if matches!(event, Event::Restarted(_)) {
                s.server.configure(|b| b.max_logins = Some(0));
            }
        })
        .await;
    assert!(result.is_err(), "{result:?}");
    assert!(events.contains(&Event::Restarted(RestartReason::RemoteChanged)));
    assert!(dl.segments.is_empty());
    assert_eq!(s.part_len(), 0, "the file is as empty as its map");

    // Back as it was: the next try starts from zero, where the old full
    // length with the old facts would have been taken as complete.
    s.server.configure(|b| {
        b.modified = Some("20240102030405".to_owned());
        b.max_logins = None;
    });
    let (result, _) = s.run(SEVERAL, &mut dl, None).await;
    s.assert_complete(result);
}

#[tokio::test]
async fn a_changed_file_starts_over() {
    let s = setup(300 * 1024, |b| b.drop_after = Some(100 * 1024)).await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(ONE, &mut dl, None).await;
    assert!(result.is_err());

    s.server
        .configure(|b| b.modified = Some("20250101000000".to_owned()));
    let (result, events) = s.run(ONE, &mut dl, None).await;
    s.assert_complete(result);
    assert!(events.contains(&Event::Restarted(RestartReason::RemoteChanged)));
}

#[tokio::test]
async fn without_rest_it_starts_over() {
    let s = setup(300 * 1024, |b| {
        b.rest = false;
        b.drop_after = Some(100 * 1024);
    })
    .await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(SEVERAL, &mut dl, None).await;
    assert!(result.is_err());
    assert!(!dl.resumable);
    assert!(dl.segments.is_empty(), "never split without REST");

    let (result, events) = s.run(SEVERAL, &mut dl, None).await;
    s.assert_complete(result);
    assert!(events.contains(&Event::Restarted(RestartReason::NotResumable)));
}

#[tokio::test]
async fn an_untrusted_certificate_can_be_trusted() {
    let s = setup(100 * 1024, |b| b.tls = Tls::Explicit).await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(ONE, &mut dl, None).await;
    let err = result.unwrap_err();
    let Error::UntrustedCertificate { host, fingerprint } = &err else {
        panic!("expected an untrusted certificate, got {err:?}");
    };
    assert_eq!(host, "127.0.0.1");
    assert_eq!(*fingerprint, FtpServer::fingerprint());
    assert!(!err.is_retryable());
    let commands = s.server.commands();
    assert!(
        !commands.iter().any(|c| c.starts_with("USER")),
        "no login: {commands:?}"
    );

    dl.trusted = vec![FtpServer::fingerprint()];
    let (result, _) = s.run(ONE, &mut dl, None).await;
    s.assert_complete(result);
    let commands = s.server.commands();
    assert!(commands.contains(&"AUTH TLS".to_owned()), "{commands:?}");
    assert!(commands.contains(&"PROT P".to_owned()), "{commands:?}");
}

#[tokio::test]
async fn several_connections_over_tls() {
    let s = setup(1024 * 1024, |b| {
        b.tls = Tls::Explicit;
        b.chunk_delay = Some(Duration::from_millis(5));
    })
    .await;
    let mut dl = s.download("ftpes", None);
    dl.trusted = vec![FtpServer::fingerprint()];
    let (result, _) = s.run(SEVERAL, &mut dl, None).await;
    s.assert_complete(result);
    assert!(s.server.logins() >= 4);
}

#[tokio::test]
async fn ftpes_requires_tls() {
    let s = setup(1024, |_| {}).await;
    let mut dl = s.download("ftpes", None);
    let (result, _) = s.run(ONE, &mut dl, None).await;
    assert!(matches!(result, Err(Error::NoTls)), "{result:?}");
    // The password never went in the clear.
    assert!(!s.server.commands().iter().any(|c| c == "PASS"));
}

#[tokio::test]
async fn implicit_tls() {
    let s = setup(100 * 1024, |b| b.tls = Tls::Implicit).await;
    let mut dl = s.download("ftps", None);
    dl.trusted = vec![FtpServer::fingerprint()];
    let (result, _) = s.run(ONE, &mut dl, None).await;
    s.assert_complete(result);
    let commands = s.server.commands();
    assert!(!commands.contains(&"AUTH TLS".to_owned()), "{commands:?}");
    assert!(commands.contains(&"PROT P".to_owned()), "{commands:?}");
}

#[tokio::test]
async fn another_certificate_isnt_trusted() {
    let s = setup(1024, |b| b.tls = Tls::Implicit).await;
    let mut dl = s.download("ftps", None);
    dl.trusted = vec!["ab".repeat(32)];
    let (result, _) = s.run(ONE, &mut dl, None).await;
    assert!(
        matches!(result, Err(Error::UntrustedCertificate { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn without_mdtm_it_never_joins_two_versions() {
    let s = setup(300 * 1024, |b| {
        b.modified = None;
        b.drop_after = Some(100 * 1024);
    })
    .await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(ONE, &mut dl, None).await;
    assert!(result.is_err());

    // Another version of the same size: resuming would join the two.
    let other = test_data(300 * 1024, 12);
    s.server.configure(|b| b.body = other.clone());
    let (result, events) = s.run(SEVERAL, &mut dl, None).await;
    let Ok(Outcome::Completed { path }) = result else {
        panic!("expected completion, got {result:?}");
    };
    assert!(std::fs::read(&path).unwrap() == other, "the file differs");
    assert!(events.contains(&Event::Restarted(RestartReason::NotResumable)));
    assert!(!dl.resumable, "the size alone proves nothing");
    // One login each time: never split without MDTM.
    assert_eq!(s.server.logins(), 2);
}

#[tokio::test]
async fn without_size_it_downloads_on_one_connection() {
    let s = setup(300 * 1024, |b| b.size = false).await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(SEVERAL, &mut dl, None).await;
    s.assert_complete(result);
    assert_eq!(dl.total, Some(300 * 1024));
    assert_eq!(s.server.logins(), 1);
}

#[tokio::test]
async fn without_size_a_cut_transfer_is_noticed() {
    let s = setup(300 * 1024, |b| {
        b.size = false;
        b.drop_after = Some(100 * 1024);
    })
    .await;
    let mut dl = s.download("ftp", None);
    let (result, _) = s.run(ONE, &mut dl, None).await;
    let err = result.unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
    assert!(dl.file_name.as_deref() != Some("file.bin") || s.part_len() == dl.downloaded);
}

#[tokio::test]
async fn the_manager_passes_the_certificates_trusted_for_the_server() {
    use ottie_core::{JobEvent, JobState, Manager, NewJob, Problem, Store};

    let s = setup(100 * 1024, |b| b.tls = Tls::Explicit).await;
    let manager = Manager::start(
        NativeBackend::with_config(ONE).unwrap(),
        Store::open_in_memory().await.unwrap(),
    )
    .await
    .unwrap();
    let mut events = manager.subscribe();
    let url = s.server.url("ftp", Some(("me", "secret")), "/file.bin");
    let id = manager
        .add(NewJob::new(url).in_dir(s.dir.path()))
        .await
        .unwrap();
    let mut wait_for = async |state: JobState| {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let JobEvent::Changed(job) = events.recv().await.unwrap()
                    && job.id == id
                    && job.state == state
                {
                    return job;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never {state:?}"))
    };
    let failed = wait_for(JobState::Failed).await;
    let Some(Problem::Certificate { host, fingerprint }) = failed.problem else {
        panic!("expected a certificate problem, got {:?}", failed.problem);
    };
    assert_eq!(fingerprint, FtpServer::fingerprint());

    // Written as people copy them: uppercase, with colons.
    let colons: Vec<String> = fingerprint
        .as_bytes()
        .chunks(2)
        .map(|pair| String::from_utf8_lossy(pair).to_uppercase())
        .collect();
    let trusted = manager
        .trust_certificate(&host.to_uppercase(), &colons.join(":"))
        .await
        .unwrap();
    assert_eq!(
        (trusted.host.as_str(), trusted.fingerprint.as_str()),
        ("127.0.0.1", fingerprint.as_str())
    );
    assert_eq!(
        manager.certificates().await.unwrap(),
        std::slice::from_ref(&trusted)
    );
    manager.resume(id).await.unwrap();
    wait_for(JobState::Completed).await;
    assert!(std::fs::read(s.dir.path().join("file.bin")).unwrap() == s.data);

    manager.distrust_certificate(trusted.id).await.unwrap();
    assert!(manager.certificates().await.unwrap().is_empty());
    assert!(matches!(
        manager.distrust_certificate(trusted.id).await,
        Err(Error::CertificateNotFound(_))
    ));
}

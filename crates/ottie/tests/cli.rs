#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The real `ottie` binary, started the way a user would.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::StatusCode;

/// `ottie` with its folders inside `dir`.
fn ottie(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ottie"));
    command
        .env("OTTIE_CONFIG_DIR", dir.join("config"))
        .env("OTTIE_DATA_DIR", dir.join("data"))
        .env_remove("OTTIE_ADDRESS")
        .env_remove("OTTIE_PORT")
        .env_remove("OTTIE_DOWNLOADS_DIR");
    command
}

/// Stops the daemon even if the test fails.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[test]
fn the_token_is_created_once() {
    let dir = tempfile::tempdir().unwrap();
    let print = || {
        let output = ottie(dir.path()).arg("token").output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let token = print();
    assert_eq!(token.len(), 64);
    assert_eq!(print(), token);
    assert!(dir.path().join("config").join("token").is_file());
}

#[test]
fn a_bad_config_file_stops_the_daemon_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::write(dir.path().join("config").join("config.toml"), "prot = 1").unwrap();
    let output = ottie(dir.path()).arg("serve").output().unwrap();
    assert!(!output.status.success());
    let message = String::from_utf8(output.stderr).unwrap();
    assert!(message.contains("config.toml is not valid"), "{message}");
}

#[tokio::test]
async fn serve_answers_requests_that_carry_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let child = ottie(dir.path())
        .args(["serve", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _running = Running(child);

    let url = format!("http://127.0.0.1:{port}/api/v1/jobs");
    let token_file = dir.path().join("config").join("token");
    let client = reqwest::Client::new();
    let started = Instant::now();
    let token = loop {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "never answered"
        );
        if let Ok(token) = std::fs::read_to_string(&token_file) {
            let token = token.trim().to_owned();
            let answer = client.get(&url).bearer_auth(&token).send().await;
            if answer.is_ok_and(|a| a.status() == StatusCode::OK) {
                break token;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let anonymous = client.get(&url).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let jobs = client.get(&url).bearer_auth(&token).send().await.unwrap();
    assert_eq!(jobs.text().await.unwrap(), "[]");
    assert!(dir.path().join("data").join("ottie.db").is_file());
}

#[test]
fn open_explains_when_ottie_is_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port().to_string();
    let output = ottie(dir.path())
        .args(["open", "--port", &port])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let message = String::from_utf8(output.stderr).unwrap();
    assert!(message.contains("isn't running"), "{message}");
    assert!(message.contains("ottie serve"), "{message}");
}

#[test]
fn a_taken_port_stops_serve_before_it_opens_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let output = ottie(dir.path())
        .args(["serve", "--port", &port])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let message = String::from_utf8(output.stderr).unwrap();
    assert!(message.contains("already running"), "{message}");
    // Otherwise it would have resumed the other daemon's downloads.
    assert!(!dir.path().join("data").join("ottie.db").exists());
}

#[test]
fn one_ottie_per_data_folder_whatever_the_port() {
    use std::io::{BufRead, BufReader};

    let dir = tempfile::tempdir().unwrap();
    // Started with `--managed`: it says `ready` once it holds the folder.
    let start = || {
        let port = free_port().to_string();
        let mut child = ottie(dir.path())
            .args(["serve", "--managed", "--port", &port])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut first = String::new();
        BufReader::new(stdout).read_line(&mut first).unwrap();
        assert_eq!(first.trim(), "ready");
        Running(child)
    };
    let first = start();

    let port = free_port().to_string();
    let mut second = Running(
        ottie(dir.path())
            .args(["serve", "--port", &port])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = second.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "a second Ottie runs on the same data folder"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    let message = std::io::read_to_string(second.0.stderr.take().unwrap()).unwrap();
    assert!(
        message.contains("already running with the data folder"),
        "{message}"
    );

    // Killed, not stopped: the lock goes with it, the next start works.
    drop(first);
    let _again = start();
}

#[test]
fn a_service_finds_why_it_stopped_in_its_log() {
    let dir = tempfile::tempdir().unwrap();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let log = dir.path().join("logs").join("ottie.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "an earlier start\n").unwrap();
    let output = ottie(dir.path())
        .args(["serve", "--port", &port, "--log-file"])
        .arg(&log)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let written = std::fs::read_to_string(&log).unwrap();
    assert!(
        written.starts_with("an earlier start\n"),
        "appended: {written}"
    );
    assert!(written.contains("already running"), "{written}");
    // No colours in a file.
    assert!(!written.contains('\u{1b}'), "{written}");
}

#[test]
fn link_prints_the_signed_in_address() {
    let dir = tempfile::tempdir().unwrap();
    let output = ottie(dir.path())
        .args(["link", "--port", "9123"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let token = std::fs::read_to_string(dir.path().join("config").join("token")).unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("http://127.0.0.1:9123/#token={}\n", token.trim())
    );
}

/// What the desktop app does: wait for `ready`, then close stdin to stop it.
#[test]
fn managed_serve_says_ready_and_stops_when_stdin_closes() {
    use std::io::{BufRead, BufReader};

    let dir = tempfile::tempdir().unwrap();
    let port = free_port().to_string();
    let mut child = ottie(dir.path())
        .args(["serve", "--managed", "--port", &port])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut running = Running(child);

    let (lines, ready) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let _ = lines.send(line.unwrap());
        }
    });
    let first = ready.recv_timeout(Duration::from_secs(15)).unwrap();
    assert_eq!(first, "ready");
    // It answers right away.
    let answer = std::net::TcpStream::connect(format!("127.0.0.1:{port}"));
    assert!(answer.is_ok());

    drop(stdin);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = running.0.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < Duration::from_secs(15), "still running");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success());
    let logs = std::io::read_to_string(stderr).unwrap();
    assert!(
        logs.contains("the program that started Ottie has quit"),
        "{logs}"
    );
    assert!(logs.contains("saving downloads"), "{logs}");
}

#[tokio::test]
async fn devices_and_a_new_token_from_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let port_arg = port.to_string();
    let child = ottie(dir.path())
        .args(["serve", "--port", &port_arg])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _running = Running(child);
    let run = |args: &[&str]| {
        let output = ottie(dir.path())
            .args(args)
            .args(["--port", &port_arg])
            .output()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let errors = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{args:?}: {errors}");
        text
    };
    let url = format!("http://127.0.0.1:{port}/api/v1/jobs");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let answers = |token: String| {
        let request = client.get(&url).bearer_auth(token);
        async move { request.send().await.map(|a| a.status()).ok() }
    };
    let started = Instant::now();
    let owner = loop {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "never answered"
        );
        let token = std::fs::read_to_string(dir.path().join("config").join("token"));
        if let Ok(token) = token
            && answers(token.trim().to_owned()).await == Some(StatusCode::OK)
        {
            break token.trim().to_owned();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let added = run(&["device", "add", "Phone", "--read-only"]);
    assert!(added.contains("device 1, read-only"), "{added}");
    let phone = added
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix(&format!("http://127.0.0.1:{port}/#token="))
        })
        .unwrap()
        .to_owned();
    assert_eq!(answers(phone.clone()).await, Some(StatusCode::OK));
    let listed = run(&["device", "list"]);
    assert!(
        listed.contains("Phone") && listed.contains("read-only"),
        "{listed}"
    );
    let saved = std::fs::read_to_string(dir.path().join("config").join("access.json")).unwrap();
    assert!(!saved.contains(&phone), "only a hash is kept");

    // A new token for this computer: the old one stops working at once;
    // devices keep theirs.
    let output = ottie(dir.path()).args(["token", "--new"]).output().unwrap();
    assert!(output.status.success());
    let new = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    assert_ne!(new, owner);
    assert_eq!(answers(owner).await, Some(StatusCode::UNAUTHORIZED));
    assert_eq!(answers(new).await, Some(StatusCode::OK));
    assert_eq!(answers(phone.clone()).await, Some(StatusCode::OK));

    run(&["device", "remove", "1"]);
    assert_eq!(answers(phone).await, Some(StatusCode::UNAUTHORIZED));
    assert!(run(&["device", "list"]).contains("No devices yet"));
}

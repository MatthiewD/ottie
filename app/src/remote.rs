//! Ottie on other computers (docs/decisions/0053): the window shows this
//! computer's Ottie or another one's (a NAS, a server), chosen in the page.
//! The list (names and addresses) is saved in `daemons.json`, each token in
//! the system's keychain; a page never gets a token back. The tray,
//! notifications and copied links follow the one shown; what happens when
//! everything has finished stays with this computer's (`status.rs`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, Url};
use tokio::sync::watch;

use crate::client::{self, Check, Daemon};
use crate::texts::Texts;
use crate::{clipboard, config, daemon, notify, popup, prefs, status};

const FILE: &str = "daemons.json";
/// The keychain's service for the tokens; each is `daemon-<id>` in it.
const KEYCHAIN: &str = "io.github.matfalco.ottie";
/// What the pages hear when the list or the one shown changes, with a `Daemons`.
const EVENT: &str = "daemons";
/// As long as a device's name on the daemon (`MAX_DEVICE_NAME`).
const MAX_NAME: usize = 60;
/// Tokens are 64 hex characters; this leaves room for other schemes.
const MAX_TOKEN: usize = 256;
/// How long another computer's Ottie has to answer before it counts as
/// unreachable.
const PROBE: Duration = Duration::from_secs(8);

/// Another computer's Ottie, as the list keeps it (its token is in the
/// keychain).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Other {
    /// Never given again once removed, so a keychain entry left behind
    /// never meets a new one.
    pub id: u32,
    pub name: String,
    /// Its root, `https://nas.example.ts.net/`: no path, no token.
    pub address: String,
}

/// `daemons.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct List {
    others: Vec<Other>,
    last_id: u32,
    /// The one the window showed last; `None`: this computer's.
    shown: Option<u32>,
}

impl List {
    /// Adds it with a new ID.
    fn add(&mut self, id: u32, name: String, address: &Url) {
        self.others.push(Other {
            id,
            name,
            address: address.to_string(),
        });
    }

    fn next_id(&mut self) -> u32 {
        self.last_id += 1;
        self.last_id
    }

    /// Whether it was there.
    fn remove(&mut self, id: u32) -> bool {
        let before = self.others.len();
        self.others.retain(|other| other.id != id);
        if self.shown == Some(id) {
            self.shown = None;
        }
        self.others.len() != before
    }

    fn find(&self, id: u32) -> Option<&Other> {
        self.others.iter().find(|other| other.id == id)
    }

    /// Whether a switch to `id` (`None`: this computer's), asked for as
    /// `ticket` while `latest` is the last asked for, may still happen: not
    /// if another was asked for since (`Ok(false)`), nor if `id` was taken
    /// off the list meanwhile.
    fn may_show(&self, id: Option<u32>, ticket: Option<u64>, latest: u64) -> Result<bool, Refusal> {
        if ticket.is_some_and(|ticket| ticket != latest) {
            return Ok(false);
        }
        match id {
            Some(id) if self.find(id).is_none() => Err(Refusal::NotFound),
            _ => Ok(true),
        }
    }
}

/// What the pages see: the list, and which one the window shows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Daemons {
    /// `None`: this computer's.
    shown: Option<u32>,
    others: Vec<Other>,
}

/// Why it couldn't add or show one: a code the page puts in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Refusal {
    /// Not a sign-in link (an `http(s)://` address with `#token=…`).
    BadLink,
    /// Nothing answered there.
    Unreachable,
    /// Something answered, but not Ottie.
    NotOttie,
    /// It's Ottie, but it doesn't take the token (any more).
    Refused,
    /// The system's keychain couldn't keep or give back the token.
    Keychain,
    /// The list couldn't be saved.
    NotSaved,
    /// Not on the list (any more).
    NotFound,
    /// This computer's Ottie couldn't start.
    CantStart,
}

/// The daemon the window shows.
pub struct Target {
    /// `None`: this computer's.
    pub id: Option<u32>,
    /// As the list names it; `None` for this computer's.
    pub name: Option<String>,
    /// Its page, signed in (`…/#token=…`).
    pub link: Url,
    pub daemon: Arc<Daemon>,
}

/// The daemons, as managed state.
pub struct Switch {
    /// `ottie`, to start this computer's.
    ottie: PathBuf,
    /// This computer's page, signed in (`ottie link`).
    local_link: Url,
    /// This computer's Ottie once it answers. Locked while it starts, so
    /// it starts once.
    local: tokio::sync::Mutex<Option<Arc<Daemon>>>,
    list: Mutex<List>,
    shown: watch::Sender<Option<Arc<Target>>>,
    /// Switches asked for so far: one that waited for its daemon to answer
    /// happens only if none was asked for since.
    switches: AtomicU64,
    /// Origins whose pages have their permissions already.
    granted: Mutex<HashSet<String>>,
}

impl Switch {
    pub fn new(app: &AppHandle, ottie: PathBuf, local_link: Url) -> Switch {
        Switch {
            ottie,
            local_link,
            local: tokio::sync::Mutex::new(None),
            list: Mutex::new(config::load(app, FILE).unwrap_or_default()),
            shown: watch::Sender::new(None),
            switches: AtomicU64::new(0),
            granted: Mutex::new(HashSet::new()),
        }
    }

    /// The one the window shows; `None` until it shows one.
    pub fn shown(&self) -> Option<Arc<Target>> {
        self.shown.borrow().clone()
    }

    /// Whether the window shows `id` (`None`: this computer's).
    pub fn is_shown(&self, id: Option<u32>) -> bool {
        self.shown
            .borrow()
            .as_ref()
            .is_some_and(|target| target.id == id)
    }

    /// A switch is asked for: its ticket for `show`.
    fn ask(&self) -> u64 {
        self.switches.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Hears when the window shows another one.
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<Target>>> {
        self.shown.subscribe()
    }

    /// The one shown last (`None`: this computer's), if still listed.
    pub fn last_shown(&self) -> Option<u32> {
        let list = self.lock();
        list.shown.filter(|&id| list.find(id).is_some())
    }

    /// The name it's listed under.
    pub fn name_of(&self, id: u32) -> Option<String> {
        self.lock().find(id).map(|other| other.name.clone())
    }

    /// This computer's Ottie, if it runs (started or found).
    pub async fn local(&self) -> Option<Arc<Daemon>> {
        self.local.lock().await.clone()
    }

    pub fn local_origin(&self) -> String {
        self.local_link.origin().ascii_serialization()
    }

    fn daemons(&self) -> Daemons {
        let list = self.lock();
        Daemons {
            shown: self.shown.borrow().as_ref().and_then(|target| target.id),
            others: list.others.clone(),
        }
    }

    /// Applies `change` to the list and saves it.
    fn change<T>(
        &self,
        app: &AppHandle,
        change: impl FnOnce(&mut List) -> T,
    ) -> Result<T, Refusal> {
        let mut list = self.lock();
        let mut next = list.clone();
        let out = change(&mut next);
        if next != *list {
            config::save(app, FILE, &next).map_err(|_| Refusal::NotSaved)?;
            *list = next;
        }
        Ok(out)
    }

    fn lock(&self) -> MutexGuard<'_, List> {
        // Nothing can panic while it's held, so a poisoned lock is still consistent.
        self.list.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// This computer's Ottie, found or started (once), and followed from then
/// on: the tray while it's shown, and when everything has finished.
pub async fn start_local(app: &AppHandle) -> anyhow::Result<Arc<Daemon>> {
    let switch = app.state::<Switch>();
    let mut local = switch.local.lock().await;
    if let Some(daemon) = &*local {
        return Ok(daemon.clone());
    }
    let connection = daemon::connect(&switch.ottie, switch.local_link.clone()).await?;
    let exit = connection.started.as_ref().map(daemon::Started::exit);
    if let Err(started) = crate::keep(app, connection.started) {
        started.stop().await;
        anyhow::bail!("the app quits");
    }
    if let Some(exit) = exit {
        tauri::async_runtime::spawn(watch_over(app.clone(), exit));
    }
    // Started by us, or it proved it's this computer's Ottie: its pages may
    // have the app's commands.
    crate::grant_local(app, &connection.link)?;
    let daemon = Arc::new(Daemon::new(&connection.link)?);
    *local = Some(daemon.clone());
    let followed = status::Followed::Local;
    tauri::async_runtime::spawn(status::follow(app.clone(), daemon.clone(), followed));
    Ok(daemon)
}

/// The waits before each new start of this computer's Ottie, after it
/// stopped unasked; then the app gives up on it.
const RESTARTS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];
/// A daemon that ran this long stopped by accident, not as it starts: the
/// waits start over.
const STEADY: Duration = Duration::from_secs(60);

/// When to start this computer's Ottie again.
#[derive(Debug, Default)]
struct Restarts {
    tries: usize,
}

impl Restarts {
    /// It stopped after running for `ran` (a start that failed: zero): the
    /// wait before starting it again, `None` to give up.
    fn after(&mut self, ran: Duration) -> Option<Duration> {
        if ran >= STEADY {
            self.tries = 0;
        }
        let wait = RESTARTS.get(self.tries).copied();
        self.tries += 1;
        wait
    }
}

/// The daemon the app started stopped without being asked to (a crash,
/// ended from the task manager): it's started again, at the same address
/// with the same token, so the window, the tray and the page carry on with
/// it. Not as the app quits, and not once another Ottie answers there (a
/// terminal, a service): that one isn't the app's to watch.
async fn watch_over(app: AppHandle, mut exit: daemon::Exit) {
    let mut restarts = Restarts::default();
    let mut since = Instant::now();
    loop {
        let mut why = exit.wait().await;
        if crate::quitting(&app) {
            return;
        }
        eprintln!("ottie-app: Ottie stopped unexpectedly: {why}");
        let mut ran = since.elapsed();
        exit = loop {
            let Some(wait) = restarts.after(ran) else {
                gave_up(&app, &why);
                return;
            };
            ran = Duration::ZERO;
            tokio::time::sleep(wait).await;
            if crate::quitting(&app) {
                return;
            }
            let switch = app.state::<Switch>();
            match daemon::connect(&switch.ottie, switch.local_link.clone()).await {
                Ok(connection) => {
                    let exit = connection.started.as_ref().map(daemon::Started::exit);
                    if let Err(started) = crate::keep(&app, connection.started) {
                        started.stop().await;
                        return;
                    }
                    match exit {
                        Some(exit) => break exit,
                        None => return,
                    }
                }
                Err(err) => why = format!("{err:#}"),
            }
        };
        eprintln!("ottie-app: Ottie started again");
        since = Instant::now();
    }
}

/// This computer's Ottie stopped and won't start: the app tells, once.
fn gave_up(app: &AppHandle, why: &str) {
    eprintln!("ottie-app: Ottie won't start again: {why}");
    let language = app.state::<prefs::Saved>().get().language;
    let texts = Texts::for_language(language.as_deref());
    notify::warn(app, texts.local_failed.clone(), Some(why.to_owned()));
}

/// The daemon `id` names (`None`: this computer's, started if it isn't),
/// once it answers and takes its token.
pub async fn target(app: &AppHandle, id: Option<u32>) -> Result<Arc<Target>, Refusal> {
    let switch = app.state::<Switch>();
    let Some(id) = id else {
        let daemon = start_local(app).await.map_err(|err| {
            eprintln!("ottie-app: {err:#}");
            Refusal::CantStart
        })?;
        return Ok(Arc::new(Target {
            id: None,
            name: None,
            link: switch.local_link.clone(),
            daemon,
        }));
    };
    let other = switch.lock().find(id).cloned().ok_or(Refusal::NotFound)?;
    let address = Url::parse(&other.address).map_err(|_| Refusal::BadLink)?;
    let token = read_token(id).await?;
    let link = signed_in(&address, &token);
    let daemon = Daemon::new(&link).map_err(|_| Refusal::Unreachable)?;
    checked(daemon.check(PROBE).await)?;
    Ok(Arc::new(Target {
        id: Some(id),
        name: Some(other.name),
        link,
        daemon: Arc::new(daemon),
    }))
}

/// The window shows `target` from now on (the caller moves the window):
/// its page gets its permissions, the tray and copied links follow it.
/// With a `ticket` (from `Switch::ask`), only if no other switch was asked
/// for since (else `Ok(false)`) and it's still listed: it may have been
/// removed while it was asked whether it answers.
pub fn show(app: &AppHandle, target: Arc<Target>, ticket: Option<u64>) -> Result<bool, Refusal> {
    let switch = app.state::<Switch>();
    let origin = target.link.origin().ascii_serialization();
    let remote = target.id.is_some() && origin != switch.local_origin();
    let fresh = switch
        .granted
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(origin.clone());
    if remote && fresh && crate::grant_remote(app, &origin).is_err() {
        // Its page then works without the app's commands (no switcher).
        eprintln!("ottie-app: no permissions for {origin}");
    }
    let (id, daemon) = (target.id, target.daemon.clone());
    {
        // Checked and shown at once: a removal waits, then sees it shown.
        let mut list = switch.lock();
        if !list.may_show(id, ticket, switch.switches.load(Ordering::SeqCst))? {
            return Ok(false);
        }
        status::moved(app);
        switch.shown.send_replace(Some(target.clone()));
        if list.shown != id {
            let mut next = list.clone();
            next.shown = id;
            // Best effort: the next start shows this computer's instead.
            if config::save(app, FILE, &next).is_ok() {
                *list = next;
            }
        }
    }
    if id.is_some() {
        let followed = status::Followed::Remote(target);
        tauri::async_runtime::spawn(status::follow(app.clone(), daemon, followed));
    }
    // The tray menu and the window for copied links are the shown one's pages.
    popup::forget(app);
    clipboard::moved(app);
    changed(app);
    Ok(true)
}

/// The window's page goes to the one shown.
fn go(app: &AppHandle, link: Url) {
    if let Some(window) = app.get_webview_window(crate::WINDOW) {
        // Best effort: it stays where it is, and the tray shows the new one.
        let _ = window.navigate(link);
    }
}

fn changed(app: &AppHandle) {
    // Best effort: a page also asks when it loads.
    let _ = app.emit(EVENT, app.state::<Switch>().daemons());
}

fn checked(check: Check) -> Result<(), Refusal> {
    match check {
        Check::Accepted => Ok(()),
        Check::Refused => Err(Refusal::Refused),
        Check::NotOttie => Err(Refusal::NotOttie),
        Check::Unreachable => Err(Refusal::Unreachable),
    }
}

/// A sign-in link (`https://nas.example.ts.net/#token=…`, as Settings →
/// Connection or `ottie device add` give one): the daemon's root and the
/// token.
fn parse_link(text: &str) -> Option<(Url, String)> {
    let link = Url::parse(text.trim()).ok()?;
    if !matches!(link.scheme(), "http" | "https") || link.host_str().is_none() {
        return None;
    }
    let token = client::token(&link);
    let fits = (1..=MAX_TOKEN).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b));
    if !fits {
        return None;
    }
    let root = link.join("/").ok()?;
    Some((root, token.to_owned()))
}

/// Its page, signed in.
fn signed_in(address: &Url, token: &str) -> Url {
    let mut link = address.clone();
    link.set_fragment(Some(&format!("token={token}")));
    link
}

/// The name given, tidied, else the address's host.
fn name_for(name: &str, address: &Url) -> String {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_NAME)
        .collect();
    if name.is_empty() {
        address.host_str().unwrap_or_default().to_owned()
    } else {
        name
    }
}

fn entry(id: u32) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN, &format!("daemon-{id}"))
}

/// Runs a keychain call on a thread of its own: some block (D-Bus).
async fn keychain<T: Send + 'static>(
    call: impl FnOnce() -> keyring::Result<T> + Send + 'static,
) -> Result<T, Refusal> {
    let result = tauri::async_runtime::spawn_blocking(call)
        .await
        .map_err(|_| Refusal::Keychain)?;
    result.map_err(|err| {
        eprintln!("ottie-app: keychain: {err}");
        Refusal::Keychain
    })
}

async fn save_token(id: u32, token: String) -> Result<(), Refusal> {
    keychain(move || entry(id)?.set_password(&token)).await
}

async fn read_token(id: u32) -> Result<String, Refusal> {
    keychain(move || entry(id)?.get_password()).await
}

async fn forget_token(id: u32) {
    // Best effort: a token left behind belongs to an ID never given again.
    let _ = keychain(move || entry(id)?.delete_credential()).await;
}

/// The list, and which one the window shows.
#[tauri::command]
pub fn daemons(switch: State<'_, Switch>) -> Daemons {
    switch.daemons()
}

/// Shows another computer's Ottie in the window, or this computer's (`None`).
#[tauri::command]
pub async fn switch_daemon(app: AppHandle, id: Option<u32>) -> Result<(), Refusal> {
    let switch = app.state::<Switch>();
    // Asked for first: a switch still waiting for its daemon is dropped.
    let ticket = switch.ask();
    if switch.is_shown(id) {
        return Ok(());
    }
    let target = target(&app, id).await?;
    let link = target.link.clone();
    if show(&app, target, Some(ticket))? {
        go(&app, link);
    }
    Ok(())
}

/// The tray's way back while another computer's Ottie is shown, even when
/// its page can't load (and with it the switcher).
pub async fn show_local(app: &AppHandle) -> Result<(), Refusal> {
    switch_daemon(app.clone(), None).await
}

/// Adds another computer's Ottie from a sign-in link, once it answers and
/// takes the token.
#[tauri::command]
pub async fn add_daemon(app: AppHandle, name: String, link: String) -> Result<Daemons, Refusal> {
    let (address, token) = parse_link(&link).ok_or(Refusal::BadLink)?;
    let daemon = Daemon::new(&signed_in(&address, &token)).map_err(|_| Refusal::BadLink)?;
    checked(daemon.check(PROBE).await)?;
    let switch = app.state::<Switch>();
    let id = switch.change(&app, List::next_id)?;
    save_token(id, token).await?;
    let name = name_for(&name, &address);
    if let Err(refusal) = switch.change(&app, |list| list.add(id, name, &address)) {
        forget_token(id).await;
        return Err(refusal);
    }
    changed(&app);
    Ok(switch.daemons())
}

/// Takes one off the list and forgets its token. The window showing it
/// goes back to this computer's first.
#[tauri::command]
pub async fn remove_daemon(app: AppHandle, id: u32) -> Result<Daemons, Refusal> {
    let switch = app.state::<Switch>();
    if switch.is_shown(Some(id)) {
        switch_daemon(app.clone(), None).await?;
    }
    if !switch.change(&app, |list| list.remove(id))? {
        return Err(Refusal::NotFound);
    }
    // A switch to it that was already checked showed it meanwhile; none
    // can from now on.
    if switch.is_shown(Some(id)) {
        switch_daemon(app.clone(), None).await?;
    }
    forget_token(id).await;
    changed(&app);
    Ok(switch.daemons())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_last_switch_asked_for_shows_a_listed_daemon() {
        let mut list = List::default();
        list.add(
            1,
            "NAS".into(),
            &Url::parse("https://nas.example/").unwrap(),
        );
        assert_eq!(list.may_show(Some(1), Some(3), 3), Ok(true));
        assert_eq!(list.may_show(None, Some(3), 3), Ok(true));
        // Another was asked for while this one waited.
        assert_eq!(list.may_show(Some(1), Some(2), 3), Ok(false));
        // Removed while it waited.
        assert!(list.remove(1));
        assert_eq!(list.may_show(Some(1), Some(3), 3), Err(Refusal::NotFound));
        // Shown at start, as asked for by nobody.
        assert_eq!(list.may_show(None, None, 3), Ok(true));
    }

    #[test]
    fn this_computers_ottie_starts_again_three_times_then_gives_up() {
        let mut restarts = Restarts::default();
        // It ran a moment, then each start fails or stops at once.
        assert_eq!(restarts.after(Duration::from_secs(5)), Some(RESTARTS[0]));
        assert_eq!(restarts.after(Duration::ZERO), Some(RESTARTS[1]));
        assert_eq!(restarts.after(Duration::ZERO), Some(RESTARTS[2]));
        assert_eq!(restarts.after(Duration::from_secs(1)), None);
        // One that ran a good while stopped by accident: the waits start over.
        let mut restarts = Restarts::default();
        assert_eq!(restarts.after(Duration::ZERO), Some(RESTARTS[0]));
        assert_eq!(restarts.after(Duration::ZERO), Some(RESTARTS[1]));
        assert_eq!(restarts.after(STEADY), Some(RESTARTS[0]));
    }

    #[test]
    fn a_sign_in_link_gives_the_root_and_the_token() {
        let (address, token) =
            parse_link(" https://nas.tail1234.ts.net/downloads?status=x#token=ab12cd ").unwrap();
        assert_eq!(address.as_str(), "https://nas.tail1234.ts.net/");
        assert_eq!(token, "ab12cd");
        let (address, _) = parse_link("http://10.8.0.1:24068/#token=ab").unwrap();
        assert_eq!(address.as_str(), "http://10.8.0.1:24068/");
        assert_eq!(
            signed_in(&address, "ab").as_str(),
            "http://10.8.0.1:24068/#token=ab"
        );
    }

    #[test]
    fn only_web_addresses_with_a_token_are_sign_in_links() {
        assert!(parse_link("https://nas.example/").is_none());
        assert!(parse_link("https://nas.example/#token=").is_none());
        assert!(parse_link("https://nas.example/#token=a b").is_none());
        assert!(parse_link("https://nas.example/#token=a\"b").is_none());
        assert!(parse_link("file:///etc/passwd#token=ab").is_none());
        assert!(parse_link("javascript:alert(1)#token=ab").is_none());
        assert!(parse_link("nas.example#token=ab").is_none());
        let long = format!("https://nas.example/#token={}", "a".repeat(MAX_TOKEN + 1));
        assert!(parse_link(&long).is_none());
    }

    #[test]
    fn a_name_is_tidied_or_the_host() {
        let address = Url::parse("https://nas.example/").unwrap();
        assert_eq!(name_for("  NAS\n ", &address), "NAS");
        assert_eq!(name_for("", &address), "nas.example");
        assert_eq!(
            name_for(&"é".repeat(100), &address).chars().count(),
            MAX_NAME
        );
    }

    #[test]
    fn ids_never_come_back_and_the_shown_one_goes_with_it() {
        let address = Url::parse("https://nas.example/").unwrap();
        let mut list = List::default();
        let first = list.next_id();
        list.add(first, "NAS".to_owned(), &address);
        list.shown = Some(first);
        assert!(list.remove(first));
        assert_eq!(list.shown, None);
        assert!(!list.remove(first));
        assert_eq!(list.next_id(), first + 1);
    }

    #[test]
    fn the_list_reads_what_it_saved() {
        let mut list = List::default();
        let id = list.next_id();
        list.add(
            id,
            "NAS".to_owned(),
            &Url::parse("https://nas.example/").unwrap(),
        );
        list.shown = Some(id);
        let text = serde_json::to_string(&list).unwrap();
        assert!(text.contains(r#""lastId":1"#));
        assert_eq!(serde_json::from_str::<List>(&text).unwrap(), list);
        assert_eq!(serde_json::from_str::<List>("{}").unwrap(), List::default());
    }
}

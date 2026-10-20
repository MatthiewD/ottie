//! Who is asking (docs/decisions/0052). The token file's token is this
//! computer's: it can do everything, including managing devices and the
//! folders they may use. Every other device gets a token of its own, full or
//! read-only, that can be removed without signing out the others. Only a
//! hash of a device's token is kept, in `access.json` beside the token file.
//!
//! A device may only have downloads written into the folders categories use
//! and the ones listed (in Settings, or `folders` in config.toml): whoever
//! holds its token can't write anywhere its account can (a Startup folder…).

use std::collections::HashMap;
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequestParts, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use ottie_core::Manager;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::AppState;
use crate::auth::Token;
use crate::error::Failure;
use crate::types::{
    AccessInfo, AddDevice, AddedDevice, AllowedFolders, ApiError, Device, FolderList, Reason,
    millis,
};

/// How `access.json` is named in the config folder.
pub const ACCESS_FILE: &str = "access.json";

/// "Last used" is kept to the minute, but written to the file at most hourly:
/// every request would otherwise rewrite it.
const SAVE_LAST_USED_EVERY: u64 = 60 * 60 * 1000;

/// The longest device name, in characters.
const NAME_LIMIT: usize = 60;

/// How often the token file is looked at besides each request.
const TOKEN_CHECK: std::time::Duration = std::time::Duration::from_secs(2);

/// What the daemon starts with.
pub struct AccessConfig {
    /// This computer's token (the token file's).
    pub token: Token,
    /// Where that token is: read again when it changes (`ottie token --new`).
    pub token_file: Option<PathBuf>,
    /// `access.json`. `None`: devices live in memory only (tests).
    pub file: Option<PathBuf>,
    /// `folders` in config.toml: devices may always use these.
    pub folders: Vec<PathBuf>,
}

impl AccessConfig {
    /// Only this computer's token; devices kept in memory.
    pub fn new(token: Token) -> Self {
        AccessConfig {
            token,
            token_file: None,
            file: None,
            folders: Vec::new(),
        }
    }
}

/// Tokens and folders, shared by every request.
#[derive(Clone)]
pub struct Access(Arc<Inner>);

struct Inner {
    owner: Mutex<Owner>,
    token_file: Option<PathBuf>,
    file: Option<PathBuf>,
    config_folders: Vec<PathBuf>,
    saved: Mutex<Saved>,
}

struct Owner {
    token: Token,
    /// When the token file was last read.
    modified: Option<SystemTime>,
    /// Cancelled when the token changes: its open event streams close.
    revoked: CancellationToken,
}

struct Saved {
    file: AccessFile,
    /// Cancelled when a device is removed.
    revoked: HashMap<u32, CancellationToken>,
}

/// `access.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessFile {
    /// Folders chosen in Settings.
    #[serde(default)]
    folders: Vec<PathBuf>,
    #[serde(default)]
    devices: Vec<SavedDevice>,
    /// The highest device ID handed out, so a removed one's never comes back.
    #[serde(default)]
    last_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedDevice {
    id: u32,
    name: String,
    read_only: bool,
    /// SHA-256 of its token, as hex. The token itself is shown once.
    hash: String,
    /// Milliseconds since 1970.
    created: u64,
    last_used: Option<u64>,
}

/// Who sent a request, from its token. Handlers take it as an extractor.
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    /// `None`: this computer.
    device: Option<u32>,
    read_only: bool,
    /// Cancelled once the token no longer works.
    pub(crate) revoked: CancellationToken,
}

impl Caller {
    pub(crate) fn read_only(&self) -> bool {
        self.read_only
    }

    fn is_owner(&self) -> bool {
        self.device.is_none()
    }

    /// Devices and folders are this computer's to manage.
    fn must_be_owner(&self) -> Result<(), Failure> {
        if self.is_owner() {
            Ok(())
        } else {
            Err(Failure::new(
                StatusCode::FORBIDDEN,
                Reason::OwnerOnly,
                "only the computer running Ottie can do this (its token file's token)",
            ))
        }
    }
}

impl<S: Sync> FromRequestParts<S> for Caller {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        // Put there by `auth::require_token`, which every private route has.
        parts
            .extensions
            .get::<Caller>()
            .cloned()
            .ok_or_else(Failure::internal)
    }
}

impl Access {
    pub fn open(config: AccessConfig) -> io::Result<Access> {
        let file = match &config.file {
            Some(path) => load(path)?,
            None => AccessFile::default(),
        };
        let revoked = file
            .devices
            .iter()
            .map(|device| (device.id, CancellationToken::new()))
            .collect();
        let modified = config.token_file.as_deref().and_then(modified);
        Ok(Access(Arc::new(Inner {
            owner: Mutex::new(Owner {
                token: config.token,
                modified,
                revoked: CancellationToken::new(),
            }),
            token_file: config.token_file,
            file: config.file,
            config_folders: config.folders,
            saved: Mutex::new(Saved { file, revoked }),
        })))
    }

    /// Who one of these tokens belongs to, if any.
    pub(crate) fn identify(&self, candidates: &[&str]) -> Option<Caller> {
        // `ottie token --new` may have replaced the token file: the old token
        // stops working at once. (A look at its date, each request.)
        self.reload_owner();
        self.owner_among(candidates)
            .or_else(|| self.device_among(candidates))
    }

    fn owner_among(&self, candidates: &[&str]) -> Option<Caller> {
        let owner = lock(&self.0.owner);
        candidates
            .iter()
            .any(|candidate| owner.token.matches(candidate))
            .then(|| Caller {
                device: None,
                read_only: false,
                revoked: owner.revoked.clone(),
            })
    }

    fn device_among(&self, candidates: &[&str]) -> Option<Caller> {
        let hashes: Vec<String> = candidates.iter().map(|token| hash(token)).collect();
        let mut saved = lock(&self.0.saved);
        let now = millis(SystemTime::now());
        let device = saved
            .file
            .devices
            .iter_mut()
            .find(|device| hashes.contains(&device.hash))?;
        let save = device
            .last_used
            .is_none_or(|last| now.saturating_sub(last) >= SAVE_LAST_USED_EVERY);
        device.last_used = Some(now);
        let (id, read_only) = (device.id, device.read_only);
        if save && let Err(err) = self.save(&saved.file) {
            tracing::warn!("can't save when a device was last used: {err}");
        }
        Some(Caller {
            device: Some(id),
            read_only,
            revoked: saved.revoked.entry(id).or_default().clone(),
        })
    }

    /// `f` with this computer's token as it is now (the token file's).
    pub(crate) fn with_owner_token<T>(&self, f: impl FnOnce(&str) -> T) -> T {
        self.reload_owner();
        f(lock(&self.0.owner).token.as_str())
    }

    /// Looks at the token file every [`TOKEN_CHECK`] until `stop`, so a new
    /// token closes the old one's event streams even when no request comes
    /// to notice it (docs/decisions/0052).
    pub(crate) fn watch_token_file(&self, stop: CancellationToken) {
        if self.0.token_file.is_none() {
            return;
        }
        let access = self.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(TOKEN_CHECK);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = every.tick() => access.reload_owner(),
                }
            }
        });
    }

    /// Reads the token file again if it changed. A read that fails or finds it
    /// empty (being written) is tried again on the next request: only a token
    /// read marks this version of the file as seen.
    fn reload_owner(&self) {
        let Some(path) = &self.0.token_file else {
            return;
        };
        let now = modified(path);
        let mut owner = lock(&self.0.owner);
        if now.is_none() || now == owner.modified {
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let token = text.trim();
        if token.is_empty() {
            return;
        }
        owner.modified = now;
        if owner.token.matches(token) {
            return;
        }
        tracing::info!("the token file changed: this computer's old token no longer works");
        owner.token = Token::new(token);
        owner.revoked.cancel();
        owner.revoked = CancellationToken::new();
    }

    /// The folders a caller may have downloads written to: `None` for this
    /// computer (anywhere its account can), else those listed and every
    /// category's.
    pub(crate) async fn folders_for(
        &self,
        caller: &Caller,
        manager: &Manager,
    ) -> Result<Folders, Failure> {
        if caller.is_owner() {
            return Ok(Folders(None));
        }
        let mut roots = self.listed();
        let categories = manager.categories().await?;
        roots.extend(categories.into_iter().filter_map(|c| c.settings.folder));
        Ok(Folders(Some(
            roots.iter().map(|root| resolve(root)).collect(),
        )))
    }

    /// config.toml's folders, then those chosen in Settings.
    fn listed(&self) -> Vec<PathBuf> {
        let saved = lock(&self.0.saved);
        self.0
            .config_folders
            .iter()
            .chain(&saved.file.folders)
            .cloned()
            .collect()
    }

    fn allowed_folders(&self) -> AllowedFolders {
        let saved = lock(&self.0.saved);
        AllowedFolders {
            config: self.0.config_folders.iter().map(|f| text(f)).collect(),
            chosen: saved.file.folders.iter().map(|f| text(f)).collect(),
        }
    }

    fn device(&self, id: u32) -> Option<Device> {
        let saved = lock(&self.0.saved);
        saved
            .file
            .devices
            .iter()
            .find(|device| device.id == id)
            .map(Device::from)
    }

    fn save(&self, file: &AccessFile) -> io::Result<()> {
        let Some(path) = &self.0.file else {
            return Ok(());
        };
        let mut text = serde_json::to_string_pretty(file).map_err(io::Error::other)?;
        text.push('\n');
        // Written beside it, then renamed over it: never half written.
        let mut next = path.as_os_str().to_owned();
        next.push(".new");
        let next = PathBuf::from(next);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Like the token file: only its owner reads it.
            options.mode(0o600);
        }
        options.open(&next)?.write_all(text.as_bytes())?;
        std::fs::rename(&next, path)
    }
}

/// Where a device may have downloads written: `None` means anywhere.
pub(crate) struct Folders(Option<Vec<PathBuf>>);

impl Folders {
    /// The folder, if it's inside an allowed one. A `..` is refused: after
    /// a link it means the link's target's parent, which the check can't
    /// follow the way the system does (`link/../x` could leave the folder).
    /// Without one, the system follows the same links at the check and at
    /// the write.
    pub(crate) fn check(&self, folder: PathBuf) -> Result<PathBuf, Failure> {
        let Some(roots) = &self.0 else {
            return Ok(folder);
        };
        let climbs = folder
            .components()
            .any(|component| component == Component::ParentDir);
        let resolved = resolve(&folder);
        if !climbs && roots.iter().any(|root| within(&resolved, root)) {
            Ok(folder)
        } else {
            Err(Failure::new(
                StatusCode::FORBIDDEN,
                Reason::FolderNotAllowed {
                    folder: text(&folder),
                },
                "this device may only use the folders of categories and those listed in Settings",
            ))
        }
    }
}

/// The folder as the system sees it: `..` and `.` gone, links followed as far
/// as it exists (a link inside an allowed folder may point out of it).
fn resolve(path: &Path) -> PathBuf {
    let mut plain = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                plain.pop();
            }
            Component::CurDir => {}
            other => plain.push(other),
        }
    }
    let mut rest = Vec::new();
    let mut existing = plain.as_path();
    loop {
        if let Ok(real) = existing.canonicalize() {
            return rest.iter().rev().fold(real, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return plain,
        }
    }
}

/// Whether `path` is `root` or inside it. Windows ignores case in names.
fn within(path: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let lower = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
        lower(path).starts_with(lower(root))
    } else {
        path.starts_with(root)
    }
}

fn load(path: &Path) -> io::Result<AccessFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not valid: {err}", path.display()),
            )
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(AccessFile::default()),
        Err(err) => Err(err),
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A new token: 32 random bytes as hex, like the token file's.
fn generate() -> Result<String, Failure> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| {
        tracing::error!("no random numbers available: {err}");
        Failure::internal()
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Folders were checked to be Unicode when they came in.
fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A poisoned lock still holds consistent data here: every change is made
/// in one step.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl From<&SavedDevice> for Device {
    fn from(device: &SavedDevice) -> Self {
        Device {
            id: device.id,
            name: device.name.clone(),
            read_only: device.read_only,
            created: device.created,
            last_used: device.last_used,
        }
    }
}

fn saving_failed(err: io::Error) -> Failure {
    tracing::error!("can't save {ACCESS_FILE}: {err}");
    Failure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        Reason::Internal,
        format!("can't save {ACCESS_FILE}"),
    )
}

/// Who this token is, and which folders devices may use.
#[utoipa::path(
    get,
    path = "/access",
    operation_id = "getAccess",
    tag = "access",
    responses((status = OK, description = "This token's device (null: the computer running Ottie) and the folders devices may use", body = AccessInfo))
)]
pub(crate) async fn get(State(state): State<AppState>, caller: Caller) -> Json<AccessInfo> {
    let access = &state.access;
    Json(AccessInfo {
        device: caller.device.and_then(|id| access.device(id)),
        read_only: caller.read_only,
        folders: access.allowed_folders(),
    })
}

/// Replaces the folders chosen in Settings, besides those of config.toml.
/// Categories' folders are always allowed. Only for the computer running
/// Ottie.
#[utoipa::path(
    put,
    path = "/access/folders",
    operation_id = "setFolders",
    tag = "access",
    request_body = FolderList,
    responses(
        (status = OK, description = "The folders devices may use now", body = AllowedFolders),
        (status = BAD_REQUEST, description = "`relativeFolder`", body = ApiError),
        (status = FORBIDDEN, description = "`ownerOnly`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn set_folders(
    State(state): State<AppState>,
    caller: Caller,
    body: Result<Json<FolderList>, JsonRejection>,
) -> Result<Json<AllowedFolders>, Failure> {
    caller.must_be_owner()?;
    let Json(body) = body?;
    let mut folders = Vec::new();
    for folder in body.folders {
        let folder = crate::categories::absolute_folder(folder)?;
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    let access = &state.access;
    {
        let mut saved = lock(&access.0.saved);
        let before = std::mem::replace(&mut saved.file.folders, folders);
        if let Err(err) = access.save(&saved.file) {
            saved.file.folders = before;
            return Err(saving_failed(err));
        }
    }
    Ok(Json(access.allowed_folders()))
}

/// Every device with a token of its own. Only for the computer running Ottie.
#[utoipa::path(
    get,
    path = "/access/devices",
    operation_id = "listDevices",
    tag = "access",
    responses(
        (status = OK, description = "Every device, oldest first", body = [Device]),
        (status = FORBIDDEN, description = "`ownerOnly`", body = ApiError),
    )
)]
pub(crate) async fn list_devices(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<Device>>, Failure> {
    caller.must_be_owner()?;
    let saved = lock(&state.access.0.saved);
    Ok(Json(saved.file.devices.iter().map(Device::from).collect()))
}

/// Gives a device a token of its own, full or read-only. The token is in
/// this answer only: Ottie keeps a hash of it. Only for the computer running
/// Ottie.
#[utoipa::path(
    post,
    path = "/access/devices",
    operation_id = "addDevice",
    tag = "access",
    request_body = AddDevice,
    responses(
        (status = CREATED, description = "The device and its token", body = AddedDevice),
        (status = BAD_REQUEST, description = "`invalidDeviceName`", body = ApiError),
        (status = FORBIDDEN, description = "`ownerOnly`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn add_device(
    State(state): State<AppState>,
    caller: Caller,
    body: Result<Json<AddDevice>, JsonRejection>,
) -> Result<(StatusCode, Json<AddedDevice>), Failure> {
    caller.must_be_owner()?;
    let Json(body) = body?;
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > NAME_LIMIT || name.chars().any(char::is_control) {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            Reason::InvalidDeviceName,
            format!("a device's name has 1 to {NAME_LIMIT} characters"),
        ));
    }
    let read_only = body.read_only.unwrap_or(false);
    let token = generate()?;
    let access = &state.access;
    let mut saved = lock(&access.0.saved);
    let id = saved.file.last_id.saturating_add(1);
    let device = SavedDevice {
        id,
        name: name.to_owned(),
        read_only,
        hash: hash(&token),
        created: millis(SystemTime::now()),
        last_used: None,
    };
    saved.file.devices.push(device.clone());
    let last_id = std::mem::replace(&mut saved.file.last_id, id);
    if let Err(err) = access.save(&saved.file) {
        saved.file.devices.pop();
        saved.file.last_id = last_id;
        return Err(saving_failed(err));
    }
    saved.revoked.insert(id, CancellationToken::new());
    tracing::info!(id, read_only, "a device was added");
    Ok((
        StatusCode::CREATED,
        Json(AddedDevice {
            device: Device::from(&device),
            token,
        }),
    ))
}

/// Takes a device's token away: its requests are refused from now on, and
/// its open event streams close. Only for the computer running Ottie.
#[utoipa::path(
    delete,
    path = "/access/devices/{id}",
    operation_id = "removeDevice",
    tag = "access",
    params(("id" = u32, Path, description = "Device ID")),
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = FORBIDDEN, description = "`ownerOnly`", body = ApiError),
        (status = NOT_FOUND, description = "`deviceNotFound`", body = ApiError),
    )
)]
pub(crate) async fn remove_device(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<u32>,
) -> Result<StatusCode, Failure> {
    caller.must_be_owner()?;
    let access = &state.access;
    let mut saved = lock(&access.0.saved);
    let Some(place) = saved.file.devices.iter().position(|device| device.id == id) else {
        return Err(Failure::new(
            StatusCode::NOT_FOUND,
            Reason::DeviceNotFound { id },
            "no such device",
        ));
    };
    let removed = saved.file.devices.remove(place);
    if let Err(err) = access.save(&saved.file) {
        saved.file.devices.insert(place, removed);
        return Err(saving_failed(err));
    }
    if let Some(revoked) = saved.revoked.remove(&id) {
        revoked.cancel();
    }
    tracing::info!(id, "a device was removed");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_are_resolved_before_comparing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Downloads");
        std::fs::create_dir(&root).unwrap();
        let folders = Folders(Some(vec![resolve(&root)]));
        assert!(folders.check(root.clone()).is_ok());
        assert!(folders.check(root.join("new").join("deeper")).is_ok());
        assert!(folders.check(root.join("..").join("Startup")).is_err());
        // Even one that stays inside.
        assert!(
            folders
                .check(root.join("new").join("..").join("x"))
                .is_err()
        );
        // Only a device's folders are checked.
        assert!(Folders(None).check(root.join("..")).is_ok());
        assert!(folders.check(dir.path().join("Downloads2")).is_err());
        assert!(Folders(None).check(dir.path().into()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_an_allowed_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("in"), dir.path().join("out"));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let folders = Folders(Some(vec![resolve(&root)]));
        assert!(folders.check(root.join("link").join("x")).is_err());
        // A link to a folder beside the allowed one, then `..`: back inside
        // on paper, outside for the system (`out/../x`).
        let beside = root.join("deep");
        std::fs::create_dir(&beside).unwrap();
        std::os::unix::fs::symlink(outside.join("."), beside.join("away")).unwrap();
        assert!(
            folders
                .check(beside.join("away").join("..").join("x"))
                .is_err()
        );
    }

    #[test]
    fn devices_are_found_by_hash_and_saved_without_their_token() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(ACCESS_FILE);
        let mut config = AccessConfig::new(Token::new("owner"));
        config.file = Some(file.clone());
        let access = Access::open(config).unwrap();
        {
            let mut saved = lock(&access.0.saved);
            saved.file.devices.push(SavedDevice {
                id: 1,
                name: "Phone".into(),
                read_only: true,
                hash: hash("phone-token"),
                created: 0,
                last_used: None,
            });
        }
        let caller = access.identify(&["nope", "phone-token"]).unwrap();
        assert_eq!((caller.device, caller.read_only), (Some(1), true));
        assert!(access.identify(&["owner"]).unwrap().is_owner());
        assert!(access.identify(&["nope"]).is_none());
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(!written.contains("phone-token"), "{written}");
        assert!(written.contains(&hash("phone-token")));
        // Read back as it was.
        let mut config = AccessConfig::new(Token::new("owner"));
        config.file = Some(file);
        let again = Access::open(config).unwrap();
        assert!(again.identify(&["phone-token"]).is_some());
    }

    #[test]
    fn a_new_token_file_replaces_the_old_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "first\n").unwrap();
        let mut config = AccessConfig::new(Token::new("first"));
        config.token_file = Some(path.clone());
        let access = Access::open(config).unwrap();
        let old = access.identify(&["first"]).unwrap();
        // A different modification time, whatever the file system's precision.
        std::fs::write(&path, "second\n").unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(access.identify(&["second"]).is_some());
        assert!(access.identify(&["first"]).is_none());
        assert!(old.revoked.is_cancelled());
    }

    #[test]
    fn a_token_file_read_half_written_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(
            &path, "first
",
        )
        .unwrap();
        let mut config = AccessConfig::new(Token::new("first"));
        config.token_file = Some(path.clone());
        let access = Access::open(config).unwrap();
        let set_time = |at| {
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(at).unwrap();
        };
        // Caught empty, as while it's being written: the old token stays.
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::write(&path, "").unwrap();
        set_time(later);
        assert!(access.identify(&["first"]).is_some());
        // Then written, within the same modification time.
        std::fs::write(
            &path, "second
",
        )
        .unwrap();
        set_time(later);
        assert!(access.identify(&["second"]).is_some());
        assert!(access.identify(&["first"]).is_none());
    }
}

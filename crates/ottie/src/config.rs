//! Where Ottie keeps its files, and the daemon's options. See
//! docs/decisions/0014.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use directories::{BaseDirs, UserDirs};
use serde::Deserialize;

pub const DEFAULT_PORT: u16 = 24068;
pub const CONFIG_FILE: &str = "config.toml";
pub const DATABASE_FILE: &str = "ottie.db";
/// Locked by the daemon using the data folder (`serve.rs`).
pub const LOCK_FILE: &str = "ottie.lock";

/// Folders, from flags or environment variables; otherwise the OS's usual places.
#[derive(Args, Debug, Clone, Default)]
pub struct DirArgs {
    /// Folder for config.toml and the token [default: %APPDATA%\Ottie, ~/.config/ottie]
    #[arg(long, env = "OTTIE_CONFIG_DIR", value_name = "FOLDER")]
    pub config_dir: Option<PathBuf>,
    /// Folder for the database [default: %LOCALAPPDATA%\Ottie, ~/.local/share/ottie]
    #[arg(long, env = "OTTIE_DATA_DIR", value_name = "FOLDER")]
    pub data_dir: Option<PathBuf>,
    /// The system-wide service: for the whole computer, at boot, under an
    /// account of its own (administrators only). Its folder: %ProgramData%\Ottie,
    /// /var/lib/ottie
    #[arg(long)]
    pub system: bool,
}

/// Where the daemon is (or will be) listening: for `serve` and `open`.
#[derive(Args, Debug, Clone, Default)]
pub struct DaemonArgs {
    #[command(flatten)]
    pub dirs: DirArgs,
    /// Address to listen on; 0.0.0.0 opens the API to other computers [default: 127.0.0.1]
    #[arg(long, env = "OTTIE_ADDRESS")]
    pub address: Option<IpAddr>,
    /// Port to listen on [default: 24068]
    #[arg(long, env = "OTTIE_PORT")]
    pub port: Option<u16>,
}

pub struct Dirs {
    pub config: PathBuf,
    pub data: PathBuf,
    /// The system-wide service's: its own account owns them (0051).
    pub system: bool,
}

impl Dirs {
    pub fn resolve(args: &DirArgs) -> Result<Dirs> {
        // Wins over the folders: those may come from `OTTIE_*` variables set
        // for your own Ottie.
        if args.system {
            let folder = system_folder()?;
            return Ok(Dirs {
                config: folder.clone(),
                data: folder,
                system: true,
            });
        }
        // "Ottie" where folder names are usually capitalized, "ottie" on Linux.
        let name = if cfg!(any(windows, target_os = "macos")) {
            "Ottie"
        } else {
            "ottie"
        };
        let base = || {
            BaseDirs::new().context("can't find the home folder; use --config-dir and --data-dir")
        };
        let config = match &args.config_dir {
            Some(dir) => dir.clone(),
            None => base()?.config_dir().join(name),
        };
        let data = match &args.data_dir {
            Some(dir) => dir.clone(),
            None => base()?.data_local_dir().join(name),
        };
        Ok(Dirs {
            config,
            data,
            system: false,
        })
    }

    /// Where downloads go until someone chooses a folder for General: the
    /// OS's Downloads folder, or for the system-wide service one in its own.
    pub fn default_downloads(&self) -> Option<PathBuf> {
        if self.system {
            return Some(self.data.join(if cfg!(windows) {
                "Downloads"
            } else {
                "downloads"
            }));
        }
        let user = UserDirs::new()?;
        let named = user.download_dir().map(Path::to_owned);
        // `~/Downloads` where the OS names none (a Linux server without
        // desktop folders).
        Some(named.unwrap_or_else(|| user.home_dir().join("Downloads")))
    }
}

/// The system-wide service's one folder, for its config, token, database and
/// log: only its account and administrators can read it.
pub fn system_folder() -> Result<PathBuf> {
    if cfg!(windows) {
        let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        Ok(PathBuf::from(base).join("Ottie"))
    } else if cfg!(target_os = "macos") {
        anyhow::bail!("Ottie has no system-wide service on macOS yet")
    } else {
        Ok(PathBuf::from("/var/lib/ottie"))
    }
}

/// `config.toml`. Every key is optional; flags and `OTTIE_*` variables win.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// `"0.0.0.0"` opens the API to other computers (use a VPN or reverse proxy).
    pub address: Option<IpAddr>,
    pub port: Option<u16>,
    /// Folders other devices may have downloads written to, besides every
    /// category's (docs/decisions/0052). Settings can add more, not remove
    /// these. The computer running Ottie isn't limited.
    #[serde(default)]
    pub folders: Vec<PathBuf>,
}

impl FileConfig {
    /// A missing file is fine: every option has a default.
    pub fn load(path: &Path) -> Result<FileConfig> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(FileConfig::default()),
            Err(err) => return Err(err).with_context(|| format!("can't read {}", path.display())),
        };
        let file: FileConfig =
            toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
        if let Some(relative) = file.folders.iter().find(|folder| !folder.is_absolute()) {
            anyhow::bail!(
                "{}: `folders` takes absolute folders, not {}",
                path.display(),
                relative.display()
            );
        }
        Ok(file)
    }
}

/// Flag or variable, else config file, else 127.0.0.1:24068.
pub fn listen_addr(address: Option<IpAddr>, port: Option<u16>, file: &FileConfig) -> SocketAddr {
    let ip = address
        .or(file.address)
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    SocketAddr::new(ip, port.or(file.port).unwrap_or(DEFAULT_PORT))
}

/// How to reach a daemon listening on `addr` from this computer: "all
/// addresses" (0.0.0.0, ::) includes the loopback one.
pub fn local(addr: SocketAddr) -> SocketAddr {
    let ip = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, addr.port())
}

/// The web UI, signed in. The token is after `#`, which browsers never send
/// to the server, and the UI removes it from the address bar.
pub fn ui_url(addr: SocketAddr, token: &str) -> String {
    format!("http://{}/#token={token}", local(addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_this_computer_only() {
        let addr = listen_addr(None, None, &FileConfig::default());
        assert_eq!(addr.to_string(), "127.0.0.1:24068");
    }

    #[test]
    fn flags_win_over_the_file() {
        let file: FileConfig = toml::from_str("address = \"0.0.0.0\"\nport = 9000").unwrap();
        assert_eq!(listen_addr(None, None, &file).to_string(), "0.0.0.0:9000");
        let flags = listen_addr(Some(Ipv4Addr::LOCALHOST.into()), Some(7000), &file);
        assert_eq!(flags.to_string(), "127.0.0.1:7000");
    }

    #[test]
    fn the_ui_link_points_at_this_computer() {
        let any = "0.0.0.0:24068".parse().unwrap();
        assert_eq!(ui_url(any, "abc"), "http://127.0.0.1:24068/#token=abc");
        let v6 = "[::]:9".parse().unwrap();
        assert_eq!(ui_url(v6, "abc"), "http://[::1]:9/#token=abc");
        let lan = "192.168.1.5:9".parse().unwrap();
        assert_eq!(local(lan), lan);
    }

    #[test]
    fn config_files_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        assert_eq!(FileConfig::load(&path).unwrap(), FileConfig::default());
        std::fs::write(&path, "prot = 80").unwrap();
        let typo = FileConfig::load(&path).unwrap_err();
        assert!(format!("{typo:#}").contains("prot"), "{typo:#}");
        std::fs::write(&path, "folders = [\"Downloads\"]").unwrap();
        let relative = FileConfig::load(&path).unwrap_err();
        assert!(format!("{relative:#}").contains("absolute"), "{relative:#}");
    }

    #[test]
    fn flags_choose_the_folders() {
        let args = DirArgs {
            config_dir: Some("/c".into()),
            data_dir: Some("/d".into()),
            ..DirArgs::default()
        };
        let dirs = Dirs::resolve(&args).unwrap();
        assert_eq!((dirs.config, dirs.data), ("/c".into(), "/d".into()));
        assert!(!dirs.system);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_system_service_has_one_folder() {
        let args = DirArgs {
            system: true,
            ..DirArgs::default()
        };
        let dirs = Dirs::resolve(&args).unwrap();
        assert_eq!(dirs.config, PathBuf::from("/var/lib/ottie"));
        assert_eq!(dirs.data, dirs.config);
        assert_eq!(
            dirs.default_downloads(),
            Some("/var/lib/ottie/downloads".into())
        );
    }
}

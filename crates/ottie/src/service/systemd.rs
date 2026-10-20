//! Linux: a systemd unit. For you in `~/.config/systemd/user` (from your
//! login on, or from boot with lingering), or for the system in
//! `/etc/systemd/system`, run by an `ottie` account.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

pub const UNIT: &str = "ottie.service";
const SYSTEM_UNITS: &str = "/etc/systemd/system";
/// The system-wide service's copy of `ottie`.
const SYSTEM_EXE: &str = "/usr/local/bin/ottie";
/// Its account, and the folder systemd makes for it (`StateDirectory`).
const ACCOUNT: &str = "ottie";
const HOME: &str = "/var/lib/ottie";

/// The unit file. `exe` and `args` are quoted: a path may hold spaces, `%`
/// (systemd's specifiers) or `$`.
pub fn unit(exe: &str, args: &[String], system: bool) -> String {
    let command = std::iter::once(exe)
        .chain(args.iter().map(String::as_str))
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ");
    let mut text = String::from(
        "# Written by `ottie service install`; `ottie service uninstall` removes it.\n\
         [Unit]\n\
         Description=Ottie download manager\n\
         Documentation=https://github.com/MatthiewD/ottie\n",
    );
    if system {
        text.push_str("Wants=network-online.target\nAfter=network-online.target\n");
    }
    text.push_str("\n[Service]\n");
    if system {
        text.push_str(&format!("User={ACCOUNT}\nGroup={ACCOUNT}\n"));
    }
    text.push_str(&format!(
        "ExecStart={command}\n\
         # A crash restarts it; a stop by hand doesn't.\n\
         Restart=on-failure\n\
         RestartSec=5\n"
    ));
    if system {
        text.push_str(
            "# /var/lib/ottie: config, token and database, for this account and its group.\n\
             StateDirectory=ottie\n\
             StateDirectoryMode=0750\n\
             UMask=0027\n\
             NoNewPrivileges=yes\n\
             PrivateTmp=yes\n\
             ProtectSystem=full\n\
             ProtectControlGroups=yes\n\
             ProtectKernelModules=yes\n\
             ProtectKernelTunables=yes\n\
             RestrictSUIDSGID=yes\n\
             LockPersonality=yes\n",
        );
    }
    let target = if system {
        "multi-user.target"
    } else {
        "default.target"
    };
    text.push_str(&format!("\n[Install]\nWantedBy={target}\n"));
    text
}

/// One word of `ExecStart`, in double quotes: `\` and `"` escaped, `%` and `$`
/// doubled so systemd takes them as they are.
fn quote(word: &str) -> String {
    let mut quoted = String::from("\"");
    for c in word.chars() {
        match c {
            '\\' | '"' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' | '$' => {
                quoted.push(c);
                quoted.push(c);
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(target_os = "linux")]
pub use here::{install, status, uninstall};

#[cfg(target_os = "linux")]
mod here {
    use std::path::PathBuf;
    use std::process::Command;

    use anyhow::{Context, Result, bail};
    use directories::BaseDirs;

    use super::{ACCOUNT, HOME, SYSTEM_EXE, SYSTEM_UNITS, UNIT, unit};
    use crate::service::{self, Install, answers, describe, text, tool};

    pub fn install(install: &Install) -> Result<()> {
        let system = install.system;
        if system {
            must_be_root()?;
            add_account()?;
        }
        let scope = Scope(system);
        // Ours stops first (a new version, or new options), so a daemon that
        // still answers afterwards is another one.
        if scope.ctl_says(&["is-active", UNIT]) == "active" {
            scope.ctl(&["stop", UNIT])?;
        }
        let exe = if system {
            service::copy_exe(&install.exe, SYSTEM_EXE.as_ref())?;
            PathBuf::from(SYSTEM_EXE)
        } else {
            install.exe.clone()
        };
        let path = scope.unit_path()?;
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)
                .with_context(|| format!("can't create {}", folder.display()))?;
        }
        std::fs::write(&path, unit(&text(&exe)?, &install.args, system))
            .with_context(|| format!("can't write {}", path.display()))?;
        scope.ctl(&["daemon-reload"])?;
        scope.ctl(&["enable", UNIT])?;
        let busy = answers(install.addr);
        if !busy {
            scope.ctl(&["start", UNIT])?;
        }

        let user = if system { "" } else { "--user " };
        if system {
            println!("Ottie runs as a service for this computer, as the `{ACCOUNT}` account.");
            println!("  Unit: {}", path.display());
            println!("  It starts at boot. Logs: journalctl -u ottie");
            println!("  Its folder: {HOME} (config.toml, token, database).");
            println!(
                "  Downloads go to {HOME}/downloads until you choose a folder, which the `{ACCOUNT}` \
                 account must be able to write to. To read them yourself: sudo usermod -aG {ACCOUNT} $USER"
            );
            println!("  Its address, signed in: sudo ottie link --system");
        } else {
            println!("Ottie runs as a service for you.");
            println!("  Unit: {}", path.display());
            if install.at_boot {
                lingering_on()?;
                println!("  It starts at boot and keeps running after you log out.");
            } else {
                println!("  It starts when you log in. Logs: journalctl --user -u ottie");
            }
            println!(
                "  It runs {}: after updating it, install again.",
                exe.display()
            );
        }
        if busy {
            println!(
                "Another Ottie already answers at {}, so this one starts {} (or once that one stops: \
                 {}systemctl {user}start ottie).",
                install.addr,
                if system || install.at_boot {
                    "at the next boot"
                } else {
                    "at your next login"
                },
                if system { "sudo " } else { "" }
            );
        }
        Ok(())
    }

    pub fn uninstall(system: bool) -> Result<()> {
        if system {
            must_be_root()?;
        }
        let scope = Scope(system);
        let path = scope.unit_path()?;
        if !path.exists() {
            println!("Ottie isn't installed as a service {}.", scope.name());
            return Ok(());
        }
        // Stopping saves the downloads: they carry on at Ottie's next start.
        scope.ctl(&["disable", "--now", UNIT])?;
        std::fs::remove_file(&path).with_context(|| format!("can't remove {}", path.display()))?;
        scope.ctl(&["daemon-reload"])?;
        println!("Ottie is no longer a service {}.", scope.name());
        if system {
            println!(
                "  Kept: {HOME} (downloads, database, token), the `{ACCOUNT}` account and {SYSTEM_EXE}.\n  \
                 To remove them: sudo userdel {ACCOUNT}; sudo rm -r {HOME} {SYSTEM_EXE}"
            );
        } else if lingering() {
            println!(
                "  Lingering stays on (other services may need it): `loginctl disable-linger` turns it off."
            );
        }
        Ok(())
    }

    pub fn status() -> Result<Vec<String>> {
        let mut lines = Vec::new();
        for system in [false, true] {
            let scope = Scope(system);
            let installed = scope.unit_path()?.exists();
            let what = match (system, installed && lingering()) {
                (true, _) => "systemd, at boot",
                (false, true) => "systemd user unit, from boot with lingering",
                (false, false) => "systemd user unit, at login",
            };
            let running = installed.then(|| scope.ctl_says(&["is-active", UNIT]) == "active");
            lines.push(describe(scope.title(), installed.then_some(what), running));
        }
        Ok(lines)
    }

    /// `systemctl` for you (`--user`) or for the system.
    struct Scope(bool);

    impl Scope {
        fn name(&self) -> &'static str {
            if self.0 {
                "for this computer"
            } else {
                "for you"
            }
        }

        fn title(&self) -> &'static str {
            if self.0 { "System-wide" } else { "For you" }
        }

        fn unit_path(&self) -> Result<PathBuf> {
            if self.0 {
                return Ok(PathBuf::from(SYSTEM_UNITS).join(UNIT));
            }
            let base = BaseDirs::new().context("can't find your home folder")?;
            Ok(base.config_dir().join("systemd").join("user").join(UNIT))
        }

        fn command(&self, args: &[&str]) -> Vec<String> {
            let mut all = Vec::new();
            if !self.0 {
                all.push("--user".to_owned());
            }
            all.extend(args.iter().map(|&arg| arg.to_owned()));
            all
        }

        fn ctl(&self, args: &[&str]) -> Result<()> {
            tool("systemctl", &self.command(args)).map(drop).with_context(|| {
                if self.0 {
                    "is systemd running?".to_owned()
                } else {
                    "is systemd running for you? (over ssh, log in with a session, or use --system)"
                        .to_owned()
                }
            })
        }

        /// What systemctl answers (`active`, `inactive`, `enabled`…),
        /// whatever its exit code.
        fn ctl_says(&self, args: &[&str]) -> String {
            Command::new("systemctl")
                .args(self.command(args))
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                .unwrap_or_default()
        }
    }

    fn must_be_root() -> Result<()> {
        if tool("id", &["-u"])? != "0" {
            bail!("a system-wide service needs root: run it with sudo");
        }
        Ok(())
    }

    /// A system account of its own, without a login, its home the folder
    /// systemd makes for the service.
    fn add_account() -> Result<()> {
        if tool("id", &["-u", ACCOUNT]).is_ok() {
            return Ok(());
        }
        let shell = ["/usr/sbin/nologin", "/sbin/nologin"]
            .into_iter()
            .find(|shell| std::path::Path::new(shell).exists())
            .unwrap_or("/bin/false");
        tool(
            "useradd",
            &[
                "--system",
                "--user-group",
                "--home-dir",
                HOME,
                "--no-create-home",
                "--shell",
                shell,
                ACCOUNT,
            ],
        )
        .map(drop)
        .context("can't create the `ottie` account")
    }

    fn me() -> Result<String> {
        tool("id", &["-un"])
    }

    fn lingering() -> bool {
        me().is_ok_and(|me| {
            Command::new("loginctl")
                .args(["show-user", &me, "--property=Linger", "--value"])
                .output()
                .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "yes")
        })
    }

    /// Your services start at boot and outlive your sessions. Asking for it
    /// for yourself is usually allowed; if not, say how.
    fn lingering_on() -> Result<()> {
        let me = me()?;
        if let Err(err) = tool("loginctl", &["enable-linger", &me]) {
            println!(
                "  Couldn't turn lingering on ({err:#}), so it starts at login for now. \
                 This turns it on: sudo loginctl enable-linger {me}"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_quoted_for_systemd() {
        assert_eq!(quote("serve"), r#""serve""#);
        assert_eq!(quote("/home/ann lee/ottie"), r#""/home/ann lee/ottie""#);
        assert_eq!(quote(r#"a\b"c"#), r#""a\\b\"c""#);
        assert_eq!(quote("100%$HOME"), r#""100%%$$HOME""#);
    }

    #[test]
    fn a_unit_for_you() {
        let unit = unit(
            "/home/ann/.cargo/bin/ottie",
            &["serve".into(), "--data-dir".into(), "/srv/o".into()],
            false,
        );
        assert!(
            unit.contains(
                r#"ExecStart="/home/ann/.cargo/bin/ottie" "serve" "--data-dir" "/srv/o""#
            )
        );
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("User="));
    }

    #[test]
    fn a_unit_for_the_system_has_its_account_and_folder() {
        let unit = unit(SYSTEM_EXE, &["serve".into(), "--system".into()], true);
        assert!(unit.contains("User=ottie\nGroup=ottie\n"));
        assert!(unit.contains(r#"ExecStart="/usr/local/bin/ottie" "serve" "--system""#));
        assert!(unit.contains("StateDirectory=ottie\n"));
        assert!(unit.contains("WantedBy=multi-user.target"));
    }
}

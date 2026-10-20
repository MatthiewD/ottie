# Running Ottie as a service

Ottie's downloads run in `ottie serve`, the daemon. The desktop app starts it for you, but it can also run on its own, without the app: started when you sign in, or at boot on a machine nobody sits at. You reach it as usual: `ottie open` (the web UI in your browser, signed in), `ottie link` (that address, for another browser or device), the desktop app (it uses a daemon that already answers) or the browser extension.

Only one Ottie can listen on a port. A service that finds another Ottie already answering is set up but not started; it says when it will start.

## For you

```bash
ottie service install      # starts now, and whenever you sign in
ottie service status       # installed? running? does Ottie answer?
ottie service uninstall    # stops it; downloads, settings and the database stay
```

| System | What install sets up | Log |
|---|---|---|
| Linux | a systemd user unit, `~/.config/systemd/user/ottie.service` | `journalctl --user -u ottie` |
| Windows | the task "Ottie" in Task Scheduler, at your sign-in | `%LOCALAPPDATA%\Ottie\ottie.log` |
| macOS | a LaunchAgent, `~/Library/LaunchAgents/io.github.matfalco.ottie.service.plist` | `~/Library/Application Support/Ottie/ottie.log` |

It runs the `ottie` you installed it with: after updating Ottie, run `ottie service install` again to restart it on the new version. To use other folders, pass them to install (`--config-dir`, `--data-dir`, `--downloads-dir`); the address and port come from `config.toml` in the config folder (`address = "…"`, `port = …`), since a service doesn't see your terminal's `OTTIE_*` variables.

On Linux, a user service stops when you log out. `ottie service install --at-boot` also turns on systemd's *lingering*, so it starts at boot and keeps running after you log out (for a machine you reach over SSH).

On Windows, a console window may flash at sign-in while Ottie lets go of it. A crash isn't restarted before your next sign-in (systemd and launchd restart it).

## For the whole computer

For a headless machine: Ottie starts at boot under an account of its own, which can't read people's files, and whose token people can't read.

```bash
sudo ottie service install --system      # Linux (Windows: from a terminal opened as administrator)
sudo ottie link --system                 # the web UI's address, signed in
sudo ottie token --system                # the token alone
sudo ottie service uninstall --system
```

| | Linux | Windows |
|---|---|---|
| Runs as | the system account `ottie` | `NT SERVICE\Ottie` |
| Its folder (config.toml, token, database) | `/var/lib/ottie` | `%ProgramData%\Ottie` |
| Downloads, until you choose a folder | `/var/lib/ottie/downloads` | `%ProgramData%\Ottie\Downloads` |
| Its copy of `ottie` | `/usr/local/bin/ottie` | `%ProgramFiles%\Ottie Service\ottie.exe` |
| Log | `journalctl -u ottie` | `%ProgramData%\Ottie\ottie.log` |

A folder you choose for downloads must be writable by that account. On Linux, to read what it downloads into `/var/lib/ottie`, join its group: `sudo usermod -aG ottie $USER`. The group reads downloads only: the database (which keeps the cookies and FTP passwords downloads were given), the token and `access.json` are the `ottie` account's alone. Uninstalling keeps the folder, the account and the copy, and says how to remove them.

Not on macOS yet: install it for yourself, or use Docker.

## Docker

```bash
docker build -t ottie .
docker run -d --name ottie --restart unless-stopped \
  -p 127.0.0.1:24068:24068 \
  -v ottie-config:/config -v ~/Downloads:/downloads ottie
docker exec ottie ottie link     # the web UI's address, signed in
```

Or with Compose:

```yaml
services:
  ottie:
    build: .
    restart: unless-stopped
    ports:
      - "127.0.0.1:24068:24068"
    volumes:
      - ottie-config:/config
      - ~/Downloads:/downloads
volumes:
  ottie-config:
```

`/config` holds config.toml, the token and the database; `/downloads` is where downloads go until you choose another folder, which must be a volume too. Inside the container Ottie listens on every address, so publish its port only to `127.0.0.1` (as above), a VPN or a reverse proxy with HTTPS, never straight to the internet. It runs as user 1000: give the volumes to that user, or run it as yours with `--user "$(id -u):$(id -g)"`.

## Reaching it from elsewhere

Ottie listens on 127.0.0.1 only, unless `address` in `config.toml` says otherwise. Every request needs a token, but the API isn't meant to face the internet: reach it through Tailscale, a VPN or a reverse proxy with HTTPS, and give each device a token of its own (`ottie device add`). How: [remote.md](remote.md).

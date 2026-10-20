//! `ottie open`: the web UI in your browser, already signed in. `ottie link`
//! prints that address instead.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{self, CONFIG_FILE, DaemonArgs, Dirs, FileConfig};
use crate::{device, token};

/// Rewritten on every `ottie open`; readable only by you, like the token.
const REDIRECT_PAGE: &str = "open.html";

pub async fn run(args: &DaemonArgs) -> Result<()> {
    let dirs = Dirs::resolve(&args.dirs)?;
    if dirs.system {
        // Its token is for administrators, and a browser shouldn't run as one.
        bail!(
            "for the system-wide service, print the address with `ottie link --system` (as an administrator) and open it"
        );
    }
    let file = FileConfig::load(&dirs.config.join(CONFIG_FILE))?;
    let addr = config::listen_addr(args.address, args.port, &file);
    let token = token::for_client(&dirs)?;
    // The page gets the token: only if it's this computer's Ottie.
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .context("can't make an HTTP client")?;
    device::check_identity(&http, addr, &token).await?;

    // The link holds the token. Handing it to the browser on its command
    // line would show it to other users of this computer (in the process
    // list), so the browser opens a private page that redirects instead.
    let page = dirs.config.join(REDIRECT_PAGE);
    token::write_private(&page, &redirect(&config::ui_url(addr, &token)), true)
        .with_context(|| format!("can't write {}", page.display()))?;
    open::that_detached(&page).context("can't start a browser")?;
    println!("Opening Ottie in your browser…");
    Ok(())
}

/// `ottie link`. Like `ottie token`, it creates the token on first use, so it
/// works before the daemon's first start (the desktop app runs it first).
pub fn print_link(args: &DaemonArgs) -> Result<()> {
    let dirs = Dirs::resolve(&args.dirs)?;
    let file = FileConfig::load(&dirs.config.join(CONFIG_FILE))?;
    let addr = config::listen_addr(args.address, args.port, &file);
    println!("{}", config::ui_url(addr, &token::for_client(&dirs)?));
    Ok(())
}

fn redirect(url: &str) -> String {
    format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n\
         <meta http-equiv=\"refresh\" content=\"0;url={url}\">\n\
         <title>Ottie</title>\n<a href=\"{url}\">Open Ottie</a>\n"
    )
}

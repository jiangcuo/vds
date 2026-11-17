//! vds-connect: opens a `.vds` connection file and runs FreeRDP on it.
//!
//! Usage: `vds-connect <file.vds>` (registered as the handler of `.vds` files).
//!
//! The file holds `key=value` lines:
//!
//! ```text
//! transport=websocket        ; or tcp
//! url=wss://pve:8006/...     ; websocket only
//! address=10.0.0.5           ; tcp only
//! port=3389                  ; tcp only
//! username=...
//! password=...
//! ws_fingerprint=...         ; websocket only: SHA-256 of the WebSocket server certificate
//! rdp_fingerprint=...        ; SHA-256 of the RDP server certificate
//! ```
//!
//! Environment: `FREERDP_PATH` picks the FreeRDP client, `FREERDP_ARGS` adds arguments that
//! override the defaults.
//!
//! The log goes to a file (see `init_logging`); run from a terminal, it also goes there, together
//! with FreeRDP's own output.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod finder;
mod launcher;
mod tunnel;

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{bail, Context as _, Result};
use tracing_subscriber::fmt::writer::MakeWriterExt as _;

/// A parsed `.vds` file.
pub struct Connection {
    pub transport: Transport,
    pub username: String,
    pub password: String,
    /// SHA-256 of the RDP server certificate, as hex; checked by FreeRDP.
    pub rdp_fingerprint: Option<String>,
}

pub enum Transport {
    /// FreeRDP connects to the RDP server directly.
    Tcp { address: String, port: u16 },
    /// FreeRDP connects to a local tunnel relaying raw RDP over this WebSocket.
    WebSocket {
        url: String,
        /// SHA-256 of the WebSocket server certificate, as hex (required for wss://).
        ws_fingerprint: Option<String>,
    },
}

fn main() {
    let console = std::io::stderr().is_terminal();
    let log = init_logging(console);

    if let Err(error) = run(log, console) {
        tracing::error!("{error:#}");
        std::process::exit(1);
    }
}

fn run(log: Option<File>, console: bool) -> Result<()> {
    let path = std::env::args_os().nth(1).context("usage: vds-connect <file.vds>")?;
    let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", PathBuf::from(&path).display()))?;
    let connection = parse(&text)?;

    let freerdp = finder::find()?;
    tracing::info!(freerdp = %freerdp.display(), "found FreeRDP");

    let (host, port) = match &connection.transport {
        Transport::Tcp { address, port } => (address.clone(), *port),
        Transport::WebSocket { url, ws_fingerprint } => {
            let port = tunnel::start(url, ws_fingerprint.as_deref())?;
            ("127.0.0.1".to_owned(), port)
        }
    };

    let code = launcher::run(&freerdp, &host, port, &connection, log, console)?;

    // The tunnel threads end with the process.
    std::process::exit(code);
}

fn parse(text: &str) -> Result<Connection> {
    let mut fields = HashMap::new();

    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';', '[']) {
            continue;
        }

        // The line may hold a password: report its number only.
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("line {} is not `key=value`", index + 1))?;
        fields.insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
    }

    let mut take = |key: &str| fields.remove(key).filter(|value| !value.is_empty());

    let transport = match take("transport").as_deref() {
        Some("tcp") => Transport::Tcp {
            address: take("address").context("missing `address`")?,
            port: take("port")
                .context("missing `port`")?
                .parse()
                .context("invalid `port`")?,
        },
        Some("websocket") => Transport::WebSocket {
            url: take("url").context("missing `url`")?,
            ws_fingerprint: take("ws_fingerprint"),
        },
        Some(other) => bail!("unknown transport `{other}` (expected `tcp` or `websocket`)"),
        None => bail!("missing `transport`"),
    };

    Ok(Connection {
        transport,
        username: take("username").context("missing `username`")?,
        password: take("password").context("missing `password`")?,
        rdp_fingerprint: take("rdp_fingerprint"),
    })
}

/// Hex fingerprint with any `:` separators removed, lowercase.
pub fn normalize_fingerprint(fingerprint: &str) -> String {
    fingerprint.chars().filter(|c| *c != ':').collect::<String>().to_ascii_lowercase()
}

/// Logs to `%LOCALAPPDATA%\vds-connect\vds-connect.log` on Windows and the temporary
/// directory elsewhere, and to stderr as well when `console`. Returns another handle to the log
/// file for FreeRDP's output.
fn init_logging(console: bool) -> Option<File> {
    #[cfg(windows)]
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("vds-connect");
    #[cfg(not(windows))]
    let dir = std::env::temp_dir();

    let file = fs::create_dir_all(&dir).ok().and_then(|()| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("vds-connect.log"))
            .ok()
    });
    let child_output = file.as_ref().and_then(|file| file.try_clone().ok());

    let subscriber = tracing_subscriber::fmt().with_ansi(false);
    match (file, console) {
        (Some(file), true) => subscriber.with_writer(Mutex::new(file).and(std::io::stderr)).init(),
        (Some(file), false) => subscriber.with_writer(Mutex::new(file)).init(),
        (None, true) => subscriber.with_writer(std::io::stderr).init(),
        (None, false) => {}
    }

    child_output
}

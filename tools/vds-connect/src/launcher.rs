//! Runs FreeRDP and waits for it to exit.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

use crate::{normalize_fingerprint, Connection};

/// Returns FreeRDP's exit code.
pub fn run(freerdp: &Path, host: &str, port: u16, connection: &Connection, log: Option<File>) -> Result<i32> {
    let host = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };

    let mut command = Command::new(freerdp);
    command
        .arg(format!("/v:{host}:{port}"))
        .arg(format!("/u:{}", connection.username))
        .arg(format!("/p:{}", connection.password))
        .arg(match &connection.rdp_fingerprint {
            Some(fingerprint) => format!("/cert:fingerprint:sha256:{}", normalize_fingerprint(fingerprint)),
            None => "/cert:ignore".to_owned(),
        })
        .args(["/dynamic-resolution", "/sound", "/gfx"]);

    // Extra FreeRDP arguments, separated by spaces; they come last, so they override the ones above.
    if let Some(extra) = std::env::var_os("FREERDP_ARGS") {
        let extra = extra.to_string_lossy().into_owned();
        let extra: Vec<&str> = extra.split_whitespace().collect();
        tracing::info!(count = extra.len(), "adding FREERDP_ARGS");
        command.args(extra);
    }

    // FreeRDP's own output goes to our log.
    if let Some(log) = log {
        if let Ok(stderr) = log.try_clone() {
            command.stdout(log).stderr(stderr);
        }
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        // No console window for console-subsystem clients such as sdl-freerdp.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    tracing::info!(%host, port, "starting FreeRDP");
    let status = command
        .status()
        .with_context(|| format!("cannot start {}", freerdp.display()))?;

    let code = status.code().unwrap_or(1);
    tracing::info!(code, "FreeRDP exited");

    Ok(code)
}

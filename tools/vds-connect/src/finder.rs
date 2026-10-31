//! Locates the FreeRDP client.
//!
//! `FREERDP_PATH` (the client itself, or a directory holding it) comes first. Otherwise the
//! directory of this executable on Windows, `/usr/local/bin` and `/usr/bin` elsewhere, then `PATH`.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// Client names, preferred first.
const NAMES: &[&str] = &[
    "sdl-freerdp3",
    "sdl3-freerdp",
    "sdl-freerdp",
    "sdl2-freerdp",
    "xfreerdp3",
    "xfreerdp",
    "freerdp3-x11",
    "freerdp-x11",
    "wlfreerdp3",
    "wlfreerdp",
    "wfreerdp",
];

pub fn find() -> Result<PathBuf> {
    if let Some(path) = env::var_os("FREERDP_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        if let Some(client) = search(&path) {
            return Ok(client);
        }
        bail!("no FreeRDP client at FREERDP_PATH ({})", path.display());
    }

    default_dirs()
        .iter()
        .find_map(|dir| search(dir))
        .ok_or_else(|| anyhow::anyhow!("no FreeRDP client found; set FREERDP_PATH"))
}

fn search(dir: &Path) -> Option<PathBuf> {
    NAMES
        .iter()
        .map(|name| dir.join(format!("{name}{}", env::consts::EXE_SUFFIX)))
        .find(|path| path.is_file())
}

fn default_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if cfg!(windows) {
        if let Some(dir) = env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) {
            dirs.push(dir);
        }
    } else {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/bin"));
    }

    if let Some(path) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&path));
    }

    dirs
}

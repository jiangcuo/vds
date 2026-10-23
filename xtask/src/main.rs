//! Build helpers for libvds.
//!
//! ```text
//! cargo run -p xtask -- gen-abi              # regenerate include/vds.h with cbindgen
//! cargo run -p xtask -- check-abi            # fail if include/vds.h is out of date
//! cargo run -p xtask -- install --prefix DIR # release build + header + vds-1.pc
//! cargo run -p xtask -- c-smoke              # compile and run the C ABI smoke test
//! ```
//! On Windows run these from an MSYS2 UCRT64 shell.

#![expect(clippy::print_stdout, clippy::print_stderr, reason = "command-line tool output")]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root().join("target"), PathBuf::from)
}

fn run(cmd: &mut Command) -> Result<()> {
    eprintln!("+ {cmd:?}");
    let status = cmd.status().with_context(|| format!("failed to run {cmd:?}"))?;
    if !status.success() {
        bail!("command failed: {cmd:?} ({status})");
    }
    Ok(())
}

fn generate_header(out: &Path) -> Result<()> {
    run(Command::new("cbindgen")
        .current_dir(root())
        .arg("--config")
        .arg("crates/vds-capi/cbindgen.toml")
        .arg("--crate")
        .arg("vds-capi")
        .arg("--output")
        .arg(out)
        .arg("crates/vds-capi"))
}

fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn check_abi() -> Result<()> {
    let tmp = target_dir().join("vds.h.check");
    generate_header(&tmp)?;
    let fresh = normalize(&std::fs::read_to_string(&tmp)?);
    let committed =
        normalize(&std::fs::read_to_string(root().join("include/vds.h")).context("include/vds.h is missing")?);
    if fresh != committed {
        bail!("include/vds.h is out of date: run `cargo run -p xtask -- gen-abi` and commit the result");
    }
    println!("include/vds.h is up to date");
    Ok(())
}

fn version() -> String {
    String::from(env!("CARGO_PKG_VERSION"))
}

fn build_release() -> Result<PathBuf> {
    run(
        Command::new(std::env::var("CARGO").unwrap_or_else(|_| String::from("cargo")))
            .current_dir(root())
            .args(["build", "--release", "-p", "vds-capi"]),
    )?;
    Ok(target_dir().join("release"))
}

/// Copies one file, collecting the error instead of stopping at it.
///
/// A running QEMU keeps the installed `vds.dll` open, and that must not cost us the header
/// and the import library: they are what a build needs. The failures are reported at the end.
fn install_file(from: &Path, to: &Path, failures: &mut Vec<String>) {
    if let Err(e) = std::fs::copy(from, to) {
        failures.push(format!("{}: {e}", to.display()));
    }
}

fn install(prefix: &Path) -> Result<()> {
    let out = build_release()?;
    let lib = prefix.join("lib");
    let include = prefix.join("include");
    let pc = lib.join("pkgconfig");
    for d in [&lib, &include, &pc] {
        std::fs::create_dir_all(d)?;
    }
    let mut failures = Vec::new();
    if cfg!(windows) {
        let bin = prefix.join("bin");
        std::fs::create_dir_all(&bin)?;
        install_file(&out.join("vds.dll"), &bin.join("vds.dll"), &mut failures);
        install_file(&out.join("libvds.dll.a"), &lib.join("libvds.dll.a"), &mut failures);
    } else {
        install_file(&out.join("libvds.so"), &lib.join("libvds.so"), &mut failures);
    }
    install_file(&root().join("include/vds.h"), &include.join("vds.h"), &mut failures);
    let prefix_str = prefix.display().to_string().replace('\\', "/");
    std::fs::write(
        pc.join("vds-1.pc"),
        format!(
            "prefix={prefix_str}\nlibdir=${{prefix}}/lib\nincludedir=${{prefix}}/include\n\n\
             Name: vds\nDescription: libvds remote display server (RDP)\nVersion: {}\n\
             Libs: -L${{libdir}} -lvds\nCflags: -I${{includedir}}\n",
            version()
        ),
    )?;
    println!("installed libvds {} to {}", version(), prefix.display());
    if !failures.is_empty() {
        bail!("could not update: {}", failures.join("; "));
    }
    Ok(())
}

fn c_smoke() -> Result<()> {
    let out = build_release()?;
    let exe = target_dir().join(if cfg!(windows) { "vds-smoke.exe" } else { "vds-smoke" });
    run(Command::new(std::env::var("CC").unwrap_or_else(|_| String::from("cc")))
        .current_dir(root())
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-Iinclude"])
        .arg("crates/vds-capi/tests/c/smoke.c")
        .arg("-o")
        .arg(&exe)
        .arg(format!("-L{}", out.display()))
        .arg("-lvds"))?;
    let mut cmd = Command::new(&exe);
    if cfg!(windows) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![out.clone()];
        paths.extend(std::env::split_paths(&path));
        cmd.env("PATH", std::env::join_paths(paths)?);
    } else {
        cmd.env("LD_LIBRARY_PATH", &out);
    }
    run(&mut cmd)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("gen-abi") => generate_header(&root().join("include/vds.h")),
        Some("check-abi") => check_abi(),
        Some("install") => {
            let prefix = args
                .iter()
                .position(|a| a == "--prefix")
                .and_then(|i| args.get(i + 1))
                .context("usage: install --prefix DIR")?;
            install(Path::new(prefix))
        }
        Some("c-smoke") => c_smoke(),
        _ => bail!("usage: xtask gen-abi | check-abi | install --prefix DIR | c-smoke"),
    }
}

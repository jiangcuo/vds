//! Builds the optional VA-API shim.
//!
//! With the `vaapi` feature on Linux, FFmpeg is built from the `third_party/ffmpeg` submodule
//! into `OUT_DIR` (static libavcodec with the `h264_vaapi` encoder and libavutil, nothing
//! else) and linked into the crate; libva and libva-drm are the system's, found with
//! pkg-config. The shim is built against them and the `vds_vaapi` cfg is set. A failure is a
//! build error. Without the feature the crate builds with openh264 alone.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// FFmpeg configure options: the encoder and the VA-API hwcontext, as static PIC archives.
const FFMPEG_CONFIGURE: &[&str] = &[
    "--enable-static",
    "--disable-shared",
    "--enable-pic",
    "--disable-programs",
    "--disable-doc",
    "--disable-network",
    "--disable-autodetect",
    "--disable-everything",
    "--disable-avformat",
    "--disable-avfilter",
    "--disable-avdevice",
    "--disable-swscale",
    "--disable-swresample",
    "--enable-vaapi",
    "--enable-encoder=h264_vaapi",
];

fn main() -> Result<()> {
    println!("cargo::rustc-check-cfg=cfg(vds_vaapi)");
    println!("cargo::rerun-if-changed=src/vaapi/shim.c");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_ALLOW_CROSS");
    if env::var_os("CARGO_FEATURE_VAAPI").is_none() {
        return Ok(());
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        println!("cargo::warning=the vaapi feature is Linux-only, building without VA-API");
        return Ok(());
    }
    vaapi()
}

/// Builds FFmpeg and the shim, and links them.
fn vaapi() -> Result<()> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let source = manifest.join("../../third_party/ffmpeg");
    let release = source.join("RELEASE");
    if !release.is_file() {
        return Err(format!(
            "{} is empty: run `git submodule update --init third_party/ffmpeg`",
            source.display()
        )
        .into());
    }
    println!("cargo::rerun-if-changed={}", release.display());
    if env::var("TARGET")? != env::var("HOST")? {
        return Err("the vaapi feature builds FFmpeg for the host only".into());
    }

    let mut libva = Vec::new();
    for name in ["libva", "libva-drm"] {
        libva.push(
            pkg_config::Config::new()
                .atleast_version("1.14")
                .cargo_metadata(false)
                .probe(name)?,
        );
    }

    let mut build = cc::Build::new();
    let compiler = build.get_compiler();
    let prefix = build_ffmpeg(
        &fs::canonicalize(&source)?,
        &PathBuf::from(env::var("OUT_DIR")?).join("ffmpeg"),
        compiler.path(),
    )?;

    build
        .file("src/vaapi/shim.c")
        .warnings(true)
        .include(prefix.join("include"));
    for lib in &libva {
        for path in &lib.include_paths {
            build.include(path);
        }
    }
    build.compile("vds_vaapi_shim");

    println!("cargo::rustc-link-search=native={}", prefix.join("lib").display());
    println!("cargo::rustc-link-lib=static=avcodec");
    println!("cargo::rustc-link-lib=static=avutil");
    let mut libs: Vec<String> = Vec::new();
    for lib in &libva {
        for path in &lib.link_paths {
            println!("cargo::rustc-link-search=native={}", path.display());
        }
        libs.extend(lib.libs.iter().cloned());
    }
    for pc in ["libavcodec", "libavutil"] {
        libs.extend(private_libs(&prefix.join(format!("lib/pkgconfig/{pc}.pc")))?);
    }
    let mut linked = vec![String::from("avcodec"), String::from("avutil")];
    for name in libs {
        if !linked.contains(&name) {
            println!("cargo::rustc-link-lib={name}");
            linked.push(name);
        }
    }
    println!("cargo::rustc-cfg=vds_vaapi");
    Ok(())
}

/// Configures, builds and installs FFmpeg under `dir`, unless the same release was already
/// built there with the same options. Returns the install prefix.
fn build_ffmpeg(source: &Path, dir: &Path, cc: &Path) -> Result<PathBuf> {
    let prefix = dir.join("prefix");
    let stamp = dir.join("stamp");
    let release = fs::read_to_string(source.join("RELEASE"))?;
    let want = format!("{}\n{}\n{}\n", release.trim(), cc.display(), FFMPEG_CONFIGURE.join(" "));
    if fs::read_to_string(&stamp).is_ok_and(|have| have == want) && prefix.join("lib/libavcodec.a").is_file() {
        return Ok(prefix);
    }

    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    let build = dir.join("build");
    fs::create_dir_all(&build)?;
    let log = dir.join("build.log");
    let jobs = env::var("NUM_JOBS").unwrap_or_else(|_| String::from("1"));

    run(
        Command::new(source.join("configure"))
            .current_dir(&build)
            .arg(format!("--prefix={}", prefix.display()))
            .arg(format!("--cc={}", cc.display()))
            .args(FFMPEG_CONFIGURE),
        &log,
    )?;
    // the encoder opens the device through the DRM display (vaGetDisplayDRM)
    if !fs::read_to_string(build.join("config.h"))?.contains("\n#define HAVE_VAAPI_DRM 1\n") {
        return Err(format!(
            "FFmpeg did not enable VA-API DRM (libva-drm missing?), see {}",
            build.join("ffbuild/config.log").display()
        )
        .into());
    }
    run(Command::new("make").current_dir(&build).arg(format!("-j{jobs}")), &log)?;
    run(Command::new("make").current_dir(&build).arg("install"), &log)?;

    fs::write(&stamp, want)?;
    Ok(prefix)
}

/// Runs `cmd` with its output appended to `log`.
fn run(cmd: &mut Command, log: &Path) -> Result<()> {
    let out = fs::OpenOptions::new().create(true).append(true).open(log)?;
    let status = cmd.stdout(out.try_clone()?).stderr(out).status()?;
    if !status.success() {
        return Err(format!(
            "{} failed ({status}), see {}",
            cmd.get_program().to_string_lossy(),
            log.display()
        )
        .into());
    }
    Ok(())
}

/// The libraries a static link of the `.pc` file's library needs (`Libs.private`).
fn private_libs(pc: &Path) -> Result<Vec<String>> {
    let mut libs = Vec::new();
    for line in fs::read_to_string(pc)?.lines() {
        let Some(flags) = line.strip_prefix("Libs.private:") else {
            continue;
        };
        for flag in flags.split_whitespace() {
            if let Some(name) = flag.strip_prefix("-l") {
                libs.push(String::from(name));
            } else if flag == "-pthread" {
                libs.push(String::from("pthread"));
            }
        }
    }
    Ok(libs)
}

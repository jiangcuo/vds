//! Builds the optional VA-API shim.
//!
//! With the `vaapi` feature on Linux, the shim is built against libavcodec, libavutil and libva
//! found with pkg-config, and the `vds_vaapi` cfg is set. Without them the crate builds with
//! openh264 alone.

fn main() {
    vaapi();
    println!("cargo::rerun-if-env-changed=PKG_CONFIG");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_ALLOW_CROSS");
}

/// Builds the VA-API shim when the `vaapi` feature is on and the libraries are found.
fn vaapi() {
    println!("cargo::rustc-check-cfg=cfg(vds_vaapi)");
    println!("cargo::rerun-if-changed=src/vaapi/shim.c");
    if std::env::var_os("CARGO_FEATURE_VAAPI").is_none() {
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        println!("cargo::warning=the vaapi feature is Linux-only, building without VA-API");
        return;
    }
    let mut libs = Vec::new();
    for (name, version) in [("libavcodec", "60"), ("libavutil", "58"), ("libva", "1.14")] {
        match pkg_config::Config::new()
            .atleast_version(version)
            .cargo_metadata(false)
            .probe(name)
        {
            Ok(lib) => libs.push(lib),
            Err(e) => {
                let msg = e.to_string().replace('\n', " ");
                println!("cargo::warning={name} not found with pkg-config, building without VA-API: {msg}");
                return;
            }
        }
    }
    let mut build = cc::Build::new();
    build.file("src/vaapi/shim.c").warnings(true);
    for lib in &libs {
        for path in &lib.include_paths {
            build.include(path);
        }
    }
    build.compile("vds_vaapi_shim");
    for lib in &libs {
        for path in &lib.link_paths {
            println!("cargo::rustc-link-search=native={}", path.display());
        }
        for name in &lib.libs {
            println!("cargo::rustc-link-lib={name}");
        }
    }
    println!("cargo::rustc-cfg=vds_vaapi");
}

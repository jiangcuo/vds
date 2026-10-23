//! Standalone RDP server with an animated test pattern, for testing clients without QEMU.
//!
//! ```text
//! vds-demo [--listen 127.0.0.1:3389] [--user U --password P] [--auth none|tls|nla]
//!          [--size 1920x1080]... [--codecs avc420:remotefx:bitmap] [--fps 60]
//!          [--cert cert.pem --key key.pem] [--log info] [--debug-dir DIR]
//! ```
//!
//! `--debug-dir` writes the same per-session `client.json` / `frames.csv` / `session.log` that
//! QEMU's `-rdp debug-dir=DIR` does, so a client can be checked against it without QEMU or a
//! guest.

#![expect(clippy::print_stderr, reason = "command-line tool output")]

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tracing::info;
use vds_core::Core;
use vds_core::auth::{AuthMode, ConnectedAction, Credentials};
use vds_core::codec::CodecSet;
use vds_core::config::ServerConfig;
use vds_core::log::LogConfig;
use vds_core::secret::Secret;
use vds_rdp::{RdpServer, TlsIdentity};
use vds_test_host::TestHost;

struct Args {
    listen: SocketAddr,
    user: String,
    password: Option<String>,
    sizes: Vec<(u32, u32)>,
    cert: Option<String>,
    key: Option<String>,
    log: Option<String>,
    config: ServerConfig,
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once('x').context("size must be WIDTHxHEIGHT")?;
    Ok((w.parse()?, h.parse()?))
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        listen: "127.0.0.1:3389".parse()?,
        user: String::from("vds"),
        password: None,
        sizes: Vec::new(),
        cert: None,
        key: None,
        log: None,
        config: ServerConfig::default(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--listen" => a.listen = value()?.parse()?,
            "--user" => a.user = value()?,
            "--password" => a.password = Some(value()?),
            "--auth" => a.config.auth = AuthMode::parse(&value()?)?,
            "--size" => a.sizes.push(parse_size(&value()?)?),
            "--codecs" => a.config.codecs = CodecSet::parse(&value()?)?,
            "--fps" => a.config.max_fps = value()?.parse()?,
            "--bitrate" => a.config.bitrate_kbps = value()?.parse()?,
            "--cert" => a.cert = Some(value()?),
            "--key" => a.key = Some(value()?),
            "--log" => a.log = Some(value()?),
            "--debug-dir" => a.config.debug_dir = Some(value()?),
            "--help" | "-h" => {
                eprintln!(
                    "usage: vds-demo [--listen ADDR] [--user U] [--password P] [--auth none|tls|nla] \
                     [--size WxH]... [--codecs LIST] [--fps N] [--bitrate KBPS] \
                     [--cert PEM --key PEM] [--log FILTER] [--debug-dir DIR]"
                );
                std::process::exit(0);
            }
            other => bail!("unknown option {other}"),
        }
    }
    if a.sizes.is_empty() {
        a.sizes.push((1280, 800));
    }
    if a.config.auth != AuthMode::None && a.password.is_none() {
        bail!("--password is required unless --auth none");
    }
    Ok(a)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    vds_core::log::init(&LogConfig {
        filter: args.log.clone(),
        format: None,
    })?;

    let tls = match (&args.cert, &args.key) {
        (Some(c), Some(k)) => TlsIdentity::from_pem(&std::fs::read(c)?, &std::fs::read(k)?)?,
        (None, None) => TlsIdentity::self_signed("localhost")?,
        _ => bail!("--cert and --key go together"),
    };
    let core = Core::new(args.config)?;
    if let Some(p) = args.password {
        core.set_password(
            Some(Credentials {
                username: args.user.clone(),
                password: Secret::new(p),
                domain: None,
            }),
            false,
            ConnectedAction::Keep,
        )?;
    }
    // Both run until the process is interrupted.
    let _host = TestHost::start(core.clone(), &args.sizes, 30)?;
    let server = RdpServer::start(core.clone(), tls)?;
    let addr = server.listen(args.listen)?;
    info!(addr = %addr, user = %args.user, heads = args.sizes.len(), "vds-demo ready");
    eprintln!(
        "vds-demo listening on {addr} (user `{}`); press Ctrl+C to stop",
        args.user
    );

    loop {
        std::thread::sleep(Duration::from_secs(5));
        for c in core.clients() {
            info!(
                sid = c.sid,
                codec = c.codec.unwrap_or("-"),
                fps = c.fps,
                bitrate_kbps = c.bitrate_kbps,
                rtt_ms = c.rtt_ms.unwrap_or(0),
                "client statistics"
            );
        }
    }
}

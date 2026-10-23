//! RDCleanPath bridge for web RDP clients (IronRDP web / iron-remote-desktop).
//!
//! ```text
//! vds-rdcleanpath --connect 127.0.0.1:3389 [--listen 127.0.0.1:9000] [--log info]
//! ```
//!
//! A browser cannot open raw TCP or run its own TLS stack against an RDP server, so the IronRDP
//! web client speaks RDCleanPath over a WebSocket: it sends its X.224 Connection Request wrapped
//! in an RDCleanPath request, and the bridge
//!
//! 1. connects to the RDP server and forwards the X.224 request,
//! 2. reads the X.224 Connection Confirm,
//! 3. performs the TLS handshake with the server itself,
//! 4. answers with the X.224 confirm and the server certificate chain (the client binds CredSSP
//!    to the server public key from it),
//! 5. then forwards the decrypted RDP stream in both directions.
//!
//! The WebSocket itself is terminated by something else (pveproxy on Proxmox VE, websockify for
//! tests) that forwards its binary payload as a plain byte stream. Without `--listen` the bridge
//! serves that stream on stdin/stdout, like `qm vncproxy`; with `--listen` it accepts TCP
//! connections (one bridge each), which is meant for tests.
//!
//! There is no token authorization here: the caller authorizes the connection before the stream reaches us.

#![expect(clippy::print_stderr, reason = "command-line tool output")]

use std::io;

use anyhow::{Context as _, Result, bail};
use ironrdp_rdcleanpath::{DetectionResult, RDCleanPath, RDCleanPathPdu};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

/// Largest RDCleanPath request accepted (it carries the X.224 request and a token).
const MAX_REQUEST: usize = 64 * 1024;
/// Largest X.224 Connection Confirm accepted.
const MAX_X224_RESPONSE: usize = 512;

struct Args {
    connect: String,
    listen: Option<String>,
    log: String,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        connect: String::new(),
        listen: None,
        log: "info".to_owned(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--connect" => args.connect = value()?,
            "--listen" => args.listen = Some(value()?),
            "--log" => args.log = value()?,
            "-h" | "--help" => {
                eprintln!("usage: vds-rdcleanpath --connect HOST:PORT [--listen ADDR:PORT] [--log FILTER]");
                std::process::exit(0);
            }
            _ => bail!("unknown argument {arg}"),
        }
    }
    if args.connect.is_empty() {
        bail!("--connect HOST:PORT is required");
    }
    Ok(args)
}

/// Reads one complete RDCleanPath PDU; nothing may follow it before our response.
async fn read_request(stream: &mut (impl AsyncRead + Unpin)) -> Result<RDCleanPathPdu> {
    let mut buf = Vec::new();
    loop {
        if let DetectionResult::Detected { total_length, .. } = RDCleanPathPdu::detect(&buf) {
            if buf.len() == total_length {
                break;
            }
            if buf.len() > total_length {
                bail!("unexpected data after the RDCleanPath request");
            }
        }
        if buf.len() >= MAX_REQUEST {
            bail!("RDCleanPath request too large");
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.context("read RDCleanPath request")?;
        if n == 0 {
            bail!("client closed the connection before the RDCleanPath request");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    RDCleanPathPdu::from_der(&buf).context("decode RDCleanPath request")
}

/// Reads the server's X.224 Connection Confirm (a single TPKT).
async fn read_x224_confirm(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    loop {
        if let Some(info) = ironrdp_pdu::find_size(&buf).context("X.224 confirm size")? {
            if buf.len() == info.length {
                return Ok(buf);
            }
            if buf.len() > info.length {
                bail!("unexpected data after the X.224 confirm");
            }
        }
        if buf.len() >= MAX_X224_RESPONSE {
            bail!("X.224 confirm too large");
        }
        let mut chunk = [0u8; MAX_X224_RESPONSE];
        let want = MAX_X224_RESPONSE - buf.len();
        let n = stream.read(&mut chunk[..want]).await.context("read X.224 confirm")?;
        if n == 0 {
            bail!("server closed the connection before the X.224 confirm");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn send_pdu(stream: &mut (impl AsyncWrite + Unpin), pdu: &RDCleanPathPdu) -> Result<()> {
    let der = pdu.to_der().context("encode RDCleanPath response")?;
    stream.write_all(&der).await?;
    stream.flush().await?;
    Ok(())
}

/// Bridges one client stream to the RDP server at `target`.
async fn bridge(mut client: impl AsyncRead + AsyncWrite + Unpin, target: &str) -> Result<()> {
    let request = read_request(&mut client).await?;
    let x224_request = match request.into_enum() {
        Ok(RDCleanPath::Request {
            destination,
            x224_connection_request,
            preconnection_blob,
            ..
        }) => {
            // The destination chosen by the client is informational: the bridge only ever
            // connects to the configured target.
            debug!(%destination, pcb = preconnection_blob.is_some(), "RDCleanPath request");
            x224_connection_request
        }
        Ok(_) => {
            send_pdu(&mut client, &RDCleanPathPdu::new_http_error(400)).await?;
            bail!("expected an RDCleanPath request");
        }
        Err(e) => {
            send_pdu(&mut client, &RDCleanPathPdu::new_http_error(400)).await?;
            bail!("malformed RDCleanPath request: {e}");
        }
    };

    let connected = async {
        let mut server = TcpStream::connect(target)
            .await
            .with_context(|| format!("connect to {target}"))?;
        server.set_nodelay(true)?;
        let server_addr = server.peer_addr()?;
        server.write_all(x224_request.as_bytes()).await?;
        server.flush().await?;
        let x224_confirm = read_x224_confirm(&mut server).await?;
        let host = target.rsplit_once(':').map_or(target, |(h, _)| h);
        let (tls, _cert) = ironrdp_tls::upgrade(server, host)
            .await
            .context("TLS handshake with the RDP server")?;
        anyhow::Ok((tls, server_addr, x224_confirm))
    }
    .await;
    let (tls, server_addr, x224_confirm) = match connected {
        Ok(c) => c,
        Err(e) => {
            send_pdu(&mut client, &RDCleanPathPdu::new_http_error(502)).await?;
            return Err(e);
        }
    };

    let chain: Vec<Vec<u8>> = tls
        .get_ref()
        .1
        .peer_certificates()
        .context("no server certificate")?
        .iter()
        .map(|c| c.to_vec())
        .collect();
    let response = RDCleanPathPdu::new_response(server_addr.to_string(), x224_confirm, chain)
        .context("build RDCleanPath response")?;
    send_pdu(&mut client, &response).await?;
    info!(server = %server_addr, "RDCleanPath established, forwarding");

    let mut tls = tls;
    match tokio::io::copy_bidirectional(&mut client, &mut tls).await {
        Ok((up, down)) => info!(client_to_server = up, server_to_client = down, "session ended"),
        // A peer closing the connection abruptly is the normal end of an RDP session.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => info!("session ended"),
        Err(e) => warn!(error = %e, "forwarding failed"),
    }
    Ok(())
}

async fn run(args: Args) -> Result<()> {
    match args.listen {
        None => {
            let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
            bridge(stdio, &args.connect).await
        }
        Some(listen) => {
            let listener = TcpListener::bind(&listen)
                .await
                .with_context(|| format!("listen on {listen}"))?;
            info!(%listen, target = %args.connect, "accepting RDCleanPath connections");
            loop {
                let (stream, peer) = listener.accept().await?;
                stream.set_nodelay(true)?;
                let target = args.connect.clone();
                tokio::spawn(async move {
                    info!(%peer, "client connected");
                    if let Err(e) = bridge(stream, &target).await {
                        warn!(%peer, error = format!("{e:#}"), "bridge failed");
                    }
                });
            }
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vds-rdcleanpath: {e:#}");
            std::process::exit(2);
        }
    };
    // Logs go to stderr: stdout carries the RDP stream in stdio mode.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&args.log))
        .with_writer(io::stderr)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vds-rdcleanpath: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = runtime.block_on(run(args)) {
        eprintln!("vds-rdcleanpath: {e:#}");
        std::process::exit(1);
    }
}

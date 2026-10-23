//! End-to-end test of `debug-dir`: a real IronRDP client connects to a server started with a
//! debug directory, and the files left behind are checked against `docs/spec/logging.md`
//! section 5.
//!
//! This test has its own binary on purpose: the debug directory and the flags live in
//! process-wide state, so a second test in the same process would share them.

#![expect(clippy::unwrap_used, reason = "integration test helpers fail loudly")]
#![expect(clippy::panic, reason = "a helper reporting which file it could not read")]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironrdp::connector::{self, ClientConnector, ConnectionResult};
use ironrdp::dvc::DrdynvcClient;
use ironrdp::pdu::gcc;
use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_async::{Framed, FramedWrite as _};
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::decode::{DecodedFrame, DecoderResult, H264Decoder};
use ironrdp_server::PixelFormat;
use ironrdp_tls::TlsStream;
use ironrdp_tokio::TokioStream;
use parking_lot::Mutex;
use tokio::net::TcpStream;
use vds_core::Core;
use vds_core::auth::{ConnectedAction, Credentials};
use vds_core::config::ServerConfig;
use vds_core::debug::DebugFlags;
use vds_core::log::LogConfig;
use vds_core::secret::Secret;
use vds_rdp::{RdpServer, TlsIdentity};
use vds_test_host::TestHost;

const W: u32 = 640;
const H: u32 = 480;
const USER: &str = "vds";
const PASS: &str = "s3cret-ticket";
const VM: &str = "e2e-debug";

/// Keys sent by the client. `false` is a character key (redacted), the other two are
/// named keys that keep their scancode.
const KEYS: [(u8, bool); 3] = [(0x1e, false), (0x1d, false), (0x53, true)];

type ClientFramed = Framed<TokioStream<TlsStream<TcpStream>>>;

struct Fixture {
    core: Core,
    server: Option<RdpServer>,
    host: Option<TestHost>,
    addr: SocketAddr,
}

impl Fixture {
    fn new(config: ServerConfig) -> Self {
        // The real subscriber, not a test one: `session.log` is written by a layer of it, and
        // the stderr filter must stay independent of the session's (logging.md 5.5).
        let _ = vds_core::log::init(&LogConfig {
            filter: Some(String::from("warn")),
            format: None,
        });
        let core = Core::new(config).unwrap();
        let host = TestHost::start(core.clone(), &[(W, H)], 30).unwrap();
        let server = RdpServer::start(core.clone(), TlsIdentity::self_signed("localhost").unwrap()).unwrap();
        let addr = server.listen("127.0.0.1:0".parse().unwrap()).unwrap();
        Self {
            core,
            server: Some(server),
            host: Some(host),
            addr,
        }
    }

    fn with_password(config: ServerConfig) -> Self {
        let f = Self::new(config);
        f.core
            .set_password(
                Some(Credentials {
                    username: USER.into(),
                    password: Secret::new(PASS.into()),
                    domain: None,
                }),
                false,
                ConnectedAction::Keep,
            )
            .unwrap();
        f
    }

    fn host(&self) -> &TestHost {
        self.host.as_ref().unwrap()
    }

    /// Stops the server and the host: the session object is dropped with the RDP thread, which
    /// is what writes the footer of `session.log`. `shutdown` joins that thread, so the footer
    /// is on disk when it returns.
    fn shutdown(&mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown();
        }
        if let Some(h) = self.host.take() {
            h.stop();
        }
    }
}

#[derive(Default)]
struct Seen {
    frames: usize,
}

/// Counts AVC420 payloads instead of decoding them.
struct Counter(Arc<Mutex<Seen>>);

impl H264Decoder for Counter {
    fn decode(&mut self, _data: &[u8]) -> DecoderResult<DecodedFrame> {
        self.0.lock().frames += 1;
        const DW: u32 = 2048;
        const DH: u32 = 1536;
        Ok(DecodedFrame::new(vec![0; DW as usize * DH as usize * 4], DW, DH))
    }

    fn reset(&mut self) {}
}

struct NoopGfx;
impl GraphicsPipelineHandler for NoopGfx {}

fn client_config(user: &str, pass: &str) -> connector::Config {
    connector::Config {
        desktop_size: connector::DesktopSize {
            width: u16::try_from(W).unwrap(),
            height: u16::try_from(H).unwrap(),
        },
        monitor_layout: None,
        desktop_scale_factor: 0,
        enable_tls: true,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        credentials: connector::Credentials::UsernamePassword {
            username: user.into(),
            password: pass.into(),
        },
        domain: None,
        client_build: 0,
        client_name: "vds-e2e-debug".into(),
        keyboard_type: gcc::KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        connection_type: gcc::ConnectionType::Lan,
        ime_file_name: String::new(),
        bitmap: None,
        dig_product_id: String::new(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".into(),
        platform: MajorPlatformType::WINDOWS,
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        enable_audio_capture: false,
        license_cache: None,
        compression_type: None,
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: true,
        performance_flags: Default::default(),
        timezone_info: Default::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: ironrdp::pdu::rdp::capability_sets::RailSupportLevel::SUPPORTED,
    }
}

async fn connect(addr: SocketAddr, seen: Arc<Mutex<Seen>>) -> anyhow::Result<(ActiveStage, ClientFramed)> {
    let tcp = TcpStream::connect(addr).await?;
    let client_addr = tcp.local_addr()?;
    let mut framed = ironrdp_tokio::TokioFramed::new(tcp);
    let mut connector = ClientConnector::new(client_config(USER, PASS), client_addr);
    let gfx = GraphicsPipelineClient::new(Box::new(NoopGfx), Some(Box::new(Counter(seen))));
    connector = connector.with_static_channel(DrdynvcClient::new().with_dynamic_channel(gfx));
    let should_upgrade = ironrdp_async::connect_begin(&mut framed, &mut connector).await?;
    let (tls, cert) = ironrdp_tls::upgrade(framed.into_inner_no_leftover(), "localhost").await?;
    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = ironrdp_tokio::TokioFramed::new(tls);
    let key = ironrdp_tls::extract_tls_server_public_key(&cert)
        .ok_or_else(|| anyhow::anyhow!("no server key"))?
        .to_owned();
    let result: ConnectionResult = ironrdp_async::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut ironrdp_tokio::reqwest::ReqwestNetworkClient::new(),
        "localhost".into(),
        key,
        None,
    )
    .await?;
    let stage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: None,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();
    Ok((stage, framed))
}

/// Processes server PDUs until `done` returns true.
async fn pump(
    stage: &mut ActiveStage,
    framed: &mut ClientFramed,
    timeout: Duration,
    mut done: impl FnMut(usize) -> bool,
) -> anyhow::Result<()> {
    let mut image = DecodedImage::new(PixelFormat::RgbA32, u16::try_from(W)?, u16::try_from(H)?);
    let mut updates = 0;
    let deadline = tokio::time::Instant::now() + timeout;
    while !done(updates) {
        let (action, payload) = tokio::time::timeout_at(deadline, framed.read_pdu())
            .await
            .map_err(|_| anyhow::anyhow!("timeout"))??;
        for out in stage.process(&mut image, action, &payload)? {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await?,
                ActiveStageOutput::GraphicsUpdate(_) => updates += 1,
                ActiveStageOutput::Terminate(reason) => anyhow::bail!("terminated: {reason:?}"),
                _ => {}
            }
        }
    }
    Ok(())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// The single `<debug-dir>/<vm>-<pid>/session-*/` directory.
fn session_dir(root: &Path) -> Option<PathBuf> {
    let vm_dir = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())?;
    std::fs::read_dir(vm_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
}

/// Number of lines in a debug file, 0 while it does not exist yet.
fn row_count(path: &Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
}

/// Waits until `done` holds for the session directory and returns it.
fn wait_for(root: &Path, what: &str, mut done: impl FnMut(&Path) -> bool) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(dir) = session_dir(root)
            && done(&dir)
        {
            return dir;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} under {}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Rows of `frames.csv` that came back with an acknowledgement (the `ack_us` column is filled).
fn acked(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .skip(1)
                .filter(|line| line.split(',').nth(ACK_US).is_some_and(|c| !c.is_empty()))
                .count()
        })
        .unwrap_or(0)
}

/// Column of `frames.csv` holding the acknowledgement latency, counted from zero.
const ACK_US: usize = 13;

/// Reads a CSV, checked to have exactly `columns` columns on every row.
fn csv(path: &Path, columns: usize) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let rows: Vec<Vec<String>> = text
        .lines()
        .map(|line| line.split(',').map(str::to_owned).collect())
        .collect();
    for row in &rows {
        assert_eq!(row.len(), columns, "{}: bad row {row:?}", path.display());
    }
    rows
}

#[test]
fn debug_directory_records_a_session() {
    let root = std::env::temp_dir().join(format!("vds-e2e-debug-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let config = ServerConfig {
        vm_name: Some(VM.into()),
        debug_dir: Some(root.display().to_string()),
        debug_flags: DebugFlags {
            input: true,
            video: true,
            pdu: false,
        },
        ..ServerConfig::default()
    };
    let mut f = Fixture::with_password(config);
    let seen = Arc::new(Mutex::new(Seen::default()));

    runtime().block_on(async {
        let (mut stage, mut framed) = connect(f.addr, Arc::clone(&seen)).await.unwrap();
        // Send the keys first, then wait for frames: by the time the frames arrive the server
        // has read and handled the input PDUs.
        let mut image = DecodedImage::new(
            PixelFormat::RgbA32,
            u16::try_from(W).unwrap(),
            u16::try_from(H).unwrap(),
        );
        let mut input = Vec::new();
        for down in [true, false] {
            for (scancode, extended) in KEYS {
                let mut flags = KeyboardFlags::empty();
                flags.set(KeyboardFlags::EXTENDED, extended);
                flags.set(KeyboardFlags::RELEASE, !down);
                input.push(FastPathInputEvent::KeyboardEvent(flags, scancode));
            }
        }
        for output in stage.process_fastpath_input(&mut image, &input).unwrap() {
            if let ActiveStageOutput::ResponseFrame(frame) = output {
                framed.write_all(&frame).await.unwrap();
            }
        }
        let s = Arc::clone(&seen);
        pump(&mut stage, &mut framed, Duration::from_secs(20), |_| {
            s.lock().frames >= 5
        })
        .await
        .unwrap();

        // The client is still connected: the session is running, and its files must already be
        // usable. Every record is flushed as it is written, not at teardown, so waiting for the
        // footer here would only hide a buffered file — which is what this test is for.
        let dir = wait_for(&root, "input rows and an acknowledged frame", |d| {
            row_count(&d.join("input.csv")) >= 2 && acked(&d.join("frames.csv")) > 0
        });

        // session.log: the session layer records `debug` events even though stderr is at `warn`.
        let log = std::fs::read_to_string(dir.join("session.log")).unwrap();
        assert!(log.contains("started 20"), "{log}");
        assert!(log.contains("client connected"), "{log}");
        assert!(!log.contains("finished after"), "the session is still up: {log}");

        // client.json is written twice: an early snapshot as soon as EGFX is negotiated, then
        // again with the final state when the session ends. The early one is what a killed
        // session leaves behind, so it has to stand on its own — except for what is not known
        // yet (the transport is only picked once a frame has been encoded).
        let client = std::fs::read_to_string(dir.join("client.json")).unwrap();
        assert!(client.contains(&format!("\"vm\": \"{VM}\"")), "{client}");
        assert!(client.contains(&format!("\"user\": \"{USER}\"")), "{client}");
        assert!(client.contains("\"avc420\": true"), "{client}");
        assert!(
            client.contains("\"width\": 640") && client.contains("\"height\": 480"),
            "{client}"
        );
        assert!(client.contains("\"keyboard_type\":"), "{client}");
        assert!(client.contains("Microsoft::Windows::RDS::Graphics"), "{client}");
        // Fields `ironrdp-server` does not expose must be listed as unavailable, not invented.
        assert!(client.contains("\"unavailable\": ["), "{client}");
        assert!(client.contains("client_build"), "{client}");

        // frames.csv: one row per written frame, sixteen columns.
        let frames = csv(&dir.join("frames.csv"), 16);
        assert_eq!(frames[0][0], "time_ms", "{frames:?}");
        assert!(frames.len() > 1, "no frame row: {frames:?}");
        assert_eq!(frames[1][1], "0", "{frames:?}");
        assert_eq!(frames[1][3], "idr", "the first frame is an IDR: {frames:?}");
        assert!(frames[1][4].parse::<usize>().unwrap() > 0, "{frames:?}");
        assert!(
            frames[1..].iter().any(|row| !row[ACK_US].is_empty()),
            "no frame row carries an acknowledgement latency: {frames:?}"
        );
        // The lossless tiles of a mixed frame are counted after the H.264 columns.
        assert!(
            frames[1..]
                .iter()
                .all(|row| row[14].parse::<usize>().is_ok() && row[15].parse::<usize>().is_ok()),
            "the tile columns are not counts: {frames:?}"
        );
        // Every segment of the frame path is timed, and the columns appear in the order
        // the work happens: age (damage arrival to encode start), conversion, encode, hand-off.
        for row in &frames[1..] {
            assert!(row[ACK_US].is_empty() || row[ACK_US].parse::<u64>().is_ok(), "{row:?}");
            for column in 8..12 {
                assert!(
                    row[column].parse::<u64>().is_ok(),
                    "column {column} is not a time: {row:?}"
                );
            }
        }

        // pointer.csv: at least the header, every row well formed.
        let pointer = csv(&dir.join("pointer.csv"), 9);
        assert_eq!(pointer[0][1], "kind", "{pointer:?}");

        // input.csv: named keys keep their scancode, character keys do not.
        let input = csv(&dir.join("input.csv"), 7);
        assert_eq!(input[0][0], "time_ms", "{input:?}");
        let rows = &input[1..];
        assert!(
            rows.iter()
                .any(|r| r[1] == "ctrl" && r[3] == "29" && r[4] == "0" && r[5] == "1"),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|r| r[1] == "delete" && r[3] == "83" && r[4] == "1"),
            "{rows:?}"
        );
        assert!(rows.iter().any(|r| r[1] == "char"), "{rows:?}");
        for row in rows {
            if row[1] == "char" {
                assert_eq!(row[3], "", "a character key leaked its scancode: {row:?}");
                assert_eq!(row[4], "", "a character key leaked the extended flag: {row:?}");
            }
            // Mouse rows never carry coordinates, whatever their kind.
            assert!(row[6].parse::<i64>().is_ok() || row[6].is_empty(), "{row:?}");
        }

        // audio.csv: header only, nothing was played.
        let audio = csv(&dir.join("audio.csv"), 10);
        assert_eq!(audio[0][1], "kind", "{audio:?}");
        assert_eq!(audio[0][9], "wave_ts", "{audio:?}");

        // video-head0.h264: the raw Annex-B stream, replayed without re-encoding. It is written
        // frame by frame, so it is already readable here, with the client still connected.
        let video = std::fs::read(dir.join("video-head0.h264")).unwrap();
        assert_eq!(video[..4], [0, 0, 0, 1], "the video file must be Annex-B");
        assert!(video.len() > 1024, "the video file is too small: {} bytes", video.len());
        dir
    });

    // The host counts the key events after the debug handler recorded them.
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.host().stats().keys < (KEYS.len() * 2) as u64 {
        assert!(Instant::now() < deadline, "the host did not receive the key events");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The client is gone: the session ends on its own and marks its files complete.
    let dir = wait_for(&root, "the session footer", |d| {
        std::fs::read_to_string(d.join("session.log")).is_ok_and(|log| log.contains("finished after"))
    });
    let log = std::fs::read_to_string(dir.join("session.log")).unwrap();
    assert!(log.contains("finished after"), "{log}");
    // ... and the transport is in the final `client.json`, now that a frame has been encoded.
    let client = std::fs::read_to_string(dir.join("client.json")).unwrap();
    assert!(client.contains("\"transport\": \"avc420\""), "{client}");
    // Which of the channels this server owns the client actually opened. This
    // client asked for the Graphics DVC and finished the EGFX handshake on it, so it is
    // recorded; a channel the client never opened would be absent instead.
    let opened = client
        .split("\"channels_opened\": [")
        .nth(1)
        .unwrap_or_default()
        .split(']')
        .next()
        .unwrap_or_default();
    assert!(opened.contains("Microsoft::Windows::RDS::Graphics"), "{client}");

    f.shutdown();
}

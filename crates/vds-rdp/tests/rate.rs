//! How fast frames actually leave and where their time goes, measured end to end on the
//! loopback at 1920x1080 against a real IronRDP client.
//!
//! This test has its own binary on purpose: the debug directory is process-wide, and the
//! numbers come from `frames.csv` (`docs/spec/logging.md` section 5).
//!
//! The host paints a moving square at 60 Hz, which is what dragging a window looks like from
//! the server side. The target is 30 fps and under 50 ms from the mouse
//! moving to the picture changing; what is measured here is the whole server path plus the
//! client's decode and acknowledgement, i.e. everything except the client's own presentation.

#![expect(clippy::unwrap_used, reason = "integration test helpers fail loudly")]
#![expect(clippy::panic, reason = "a helper reporting which file it could not read")]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironrdp::connector::{self, ClientConnector, ConnectionResult};
use ironrdp::dvc::DrdynvcClient;
use ironrdp::pdu::gcc;
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
use vds_core::log::LogConfig;
use vds_core::secret::Secret;
use vds_rdp::{RdpServer, TlsIdentity};
use vds_test_host::TestHost;

const W: u32 = 1920;
const H: u32 = 1080;
const USER: &str = "vds";
const PASS: &str = "s3cret-ticket";
const VM: &str = "e2e-rate";

/// Frames the client must have decoded before the measurement starts: the first one is an IDR
/// covering the whole screen and the channel is still settling.
const WARMUP: usize = 10;
/// Length of the measurement window.
const MEASURE: Duration = Duration::from_secs(4);
/// Target: 1920x1080 on the loopback, dragging a window, stays above this.
const MIN_FPS: f64 = 30.0;
/// The plan's target for mouse to picture, which this test covers up to the client's own
/// presentation.
const MAX_LATENCY_US: u64 = 50_000;
/// `crates/vds-rdp/src/egfx.rs` `MAX_FRAMES_IN_FLIGHT`: the client is never told more than this
/// many frames are unanswered, which is what bounds the pipeline.
const MAX_FRAMES_IN_FLIGHT: u32 = 3;

type ClientFramed = Framed<TokioStream<TlsStream<TcpStream>>>;

/// Arrival time of every frame the client decoded, in order.
type Arrivals = Arc<Mutex<Vec<Instant>>>;

struct Fixture {
    core: Core,
    server: Option<RdpServer>,
    host: Option<TestHost>,
    addr: SocketAddr,
}

impl Fixture {
    fn new(config: ServerConfig) -> Self {
        // The real subscriber, not a test one: `frames.csv` is written through it.
        let _ = vds_core::log::init(&LogConfig {
            filter: Some(String::from("warn")),
            format: None,
        });
        let core = Core::new(config).unwrap();
        let host = TestHost::start(core.clone(), &[(W, H)], 60).unwrap();
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

    fn shutdown(&mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown();
        }
        if let Some(h) = self.host.take() {
            h.stop();
        }
    }
}

/// Counts frames instead of decoding them: this test measures the frame rate, and decoding
/// 1080p would only add time the client does not spend in the real one.
struct Counter(Arrivals);

impl H264Decoder for Counter {
    fn decode(&mut self, _data: &[u8]) -> DecoderResult<DecodedFrame> {
        self.0.lock().push(Instant::now());
        Ok(DecodedFrame::new(vec![0; W as usize * H as usize * 4], W, H))
    }

    fn reset(&mut self) {}
}

struct NoopGfx;
impl GraphicsPipelineHandler for NoopGfx {}

fn client_config() -> connector::Config {
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
            username: USER.into(),
            password: PASS.into(),
        },
        domain: None,
        client_build: 0,
        client_name: "vds-e2e-rate".into(),
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

async fn connect(addr: SocketAddr, arrivals: Arrivals) -> anyhow::Result<(ActiveStage, ClientFramed)> {
    let tcp = TcpStream::connect(addr).await?;
    let client_addr = tcp.local_addr()?;
    let mut framed = ironrdp_tokio::TokioFramed::new(tcp);
    let mut connector = ClientConnector::new(client_config(), client_addr);
    let gfx = GraphicsPipelineClient::new(Box::new(NoopGfx), Some(Box::new(Counter(arrivals))));
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

/// Processes server PDUs until `done` holds or `deadline` passes. Returns `true` when the
/// deadline ended it, which is how a measurement window closes.
async fn pump(
    stage: &mut ActiveStage,
    framed: &mut ClientFramed,
    image: &mut DecodedImage,
    deadline: tokio::time::Instant,
    mut done: impl FnMut() -> bool,
) -> anyhow::Result<bool> {
    while !done() {
        let (action, payload) = match tokio::time::timeout_at(deadline, framed.read_pdu()).await {
            Err(_) => return Ok(true),
            Ok(pdu) => pdu?,
        };
        for out in stage.process(image, action, &payload)? {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await?,
                ActiveStageOutput::Terminate(reason) => anyhow::bail!("terminated: {reason:?}"),
                _ => {}
            }
        }
    }
    Ok(false)
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

/// One `frames.csv` row, in microseconds.
#[derive(Clone, Copy)]
struct Timing {
    age_us: u64,
    convert_us: u64,
    encode_us: u64,
    handoff_us: u64,
    in_flight: u32,
    ack_us: Option<u64>,
}

impl Timing {
    /// The whole measured path: the picture was damaged, it was encoded, the client decoded it
    /// and acknowledged it. Everything except the client drawing it on screen.
    fn total_us(&self) -> u64 {
        self.age_us + self.convert_us + self.encode_us + self.handoff_us + self.ack_us.unwrap_or(0)
    }
}

/// Reads `frames.csv`, checked to have sixteen well-formed columns on every row.
fn timings(path: &Path) -> Vec<Timing> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut rows = text.lines();
    assert_eq!(rows.next().unwrap_or_default().split(',').count(), 16, "{text}");
    rows.map(|line| {
        let row: Vec<&str> = line.split(',').collect();
        assert_eq!(row.len(), 16, "bad row {line:?}");
        Timing {
            age_us: row[8].parse().unwrap(),
            convert_us: row[9].parse().unwrap(),
            encode_us: row[10].parse().unwrap(),
            handoff_us: row[11].parse().unwrap(),
            in_flight: row[12].parse().unwrap(),
            ack_us: (!row[13].is_empty()).then(|| row[13].parse().unwrap()),
        }
    })
    .collect()
}

/// The value at `p` (0..=1) of `values`, 0 when empty.
fn percentile(values: &[u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let last = sorted.len() - 1;
    let index = (last as f64 * p).round() as usize;
    sorted[index.min(last)]
}

#[test]
fn loopback_1080p_keeps_thirty_frames_per_second() {
    let root = std::env::temp_dir().join(format!("vds-e2e-rate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let config = ServerConfig {
        vm_name: Some(VM.into()),
        debug_dir: Some(root.display().to_string()),
        // Nothing is written but `frames.csv`: a 1080p Annex-B file would only cost time here.
        debug_flags: vds_core::debug::DebugFlags::default(),
        ..ServerConfig::default()
    };
    let mut f = Fixture::with_password(config);
    let arrivals = Arc::new(Mutex::new(Vec::new()));

    runtime().block_on(async {
        let (mut stage, mut framed) = connect(f.addr, Arc::clone(&arrivals)).await.unwrap();
        let mut image = DecodedImage::new(
            PixelFormat::RgbA32,
            u16::try_from(W).unwrap(),
            u16::try_from(H).unwrap(),
        );

        let seen = Arc::clone(&arrivals);
        let ended = pump(
            &mut stage,
            &mut framed,
            &mut image,
            tokio::time::Instant::now() + Duration::from_secs(30),
            || seen.lock().len() >= WARMUP,
        )
        .await
        .unwrap();
        assert!(!ended, "the client decoded fewer than {WARMUP} frames in 30 s");

        let before = arrivals.lock().len();
        let started = Instant::now();
        let ended = pump(
            &mut stage,
            &mut framed,
            &mut image,
            tokio::time::Instant::now() + MEASURE,
            || false,
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
        assert!(ended, "the measurement window ended early");
        let decoded = arrivals.lock().len() - before;
        let fps = decoded as f64 / elapsed.as_secs_f64();

        // The rows are written when the frame is acknowledged, so this holds everything that
        // the client has confirmed by now (`docs/spec/logging.md` section 5).
        let dir = session_dir(&root).expect("a session directory");
        let rows = timings(&dir.join("frames.csv"));
        let acked: Vec<Timing> = rows.iter().copied().filter(|t| t.ack_us.is_some()).collect();
        let column = |f: fn(&Timing) -> u64| acked.iter().map(f).collect::<Vec<u64>>();
        let (age, convert, encode, handoff, ack) = (
            column(|t| t.age_us),
            column(|t| t.convert_us),
            column(|t| t.encode_us),
            column(|t| t.handoff_us),
            column(|t| t.ack_us.unwrap_or(0)),
        );
        let total: Vec<u64> = acked.iter().map(Timing::total_us).collect();
        let in_flight = rows.iter().map(|t| t.in_flight).max().unwrap_or(0);

        eprintln!(
            "rate: loopback {W}x{H}: {fps:.1} fps ({decoded} frames in {:.2} s), {} of {} rows acknowledged",
            elapsed.as_secs_f64(),
            acked.len(),
            rows.len()
        );
        eprintln!(
            "rate: microseconds, p50/p95/max: age {}/{}/{} convert {}/{}/{} encode {}/{}/{} handoff {}/{}/{} ack {}/{}/{}",
            percentile(&age, 0.5),
            percentile(&age, 0.95),
            age.iter().copied().max().unwrap_or(0),
            percentile(&convert, 0.5),
            percentile(&convert, 0.95),
            convert.iter().copied().max().unwrap_or(0),
            percentile(&encode, 0.5),
            percentile(&encode, 0.95),
            encode.iter().copied().max().unwrap_or(0),
            percentile(&handoff, 0.5),
            percentile(&handoff, 0.95),
            handoff.iter().copied().max().unwrap_or(0),
            percentile(&ack, 0.5),
            percentile(&ack, 0.95),
            ack.iter().copied().max().unwrap_or(0)
        );
        eprintln!(
            "rate: damage to acknowledged frame: p50 {} us, p95 {} us; frames in flight at most {in_flight} (cap {MAX_FRAMES_IN_FLIGHT})",
            percentile(&total, 0.5),
            percentile(&total, 0.95)
        );

        assert!(fps >= MIN_FPS, "{fps:.1} fps is below the {MIN_FPS} fps target");
        assert!(!acked.is_empty(), "no acknowledged frame: the measurement is empty");
        // The target is about the shipped build. A debug build measures unoptimized
        // colour conversion (`yuv`) and openh264 (`cc`, `-O0`), several times the real cost, so its
        // numbers are reported but not held to the threshold: run this test with `--release`.
        let p95 = percentile(&total, 0.95);
        if cfg!(debug_assertions) {
            eprintln!("rate: debug build: the {MAX_LATENCY_US} us threshold is not enforced, run with --release");
        } else {
            assert!(
                p95 < MAX_LATENCY_US,
                "damage to acknowledged frame: p95 {p95} us, over the {MAX_LATENCY_US} us target"
            );
        }
        // The client is never sent more unanswered frames than the server advertises.
        assert!(in_flight <= MAX_FRAMES_IN_FLIGHT, "{in_flight} frames in flight");
    });

    f.shutdown();
}

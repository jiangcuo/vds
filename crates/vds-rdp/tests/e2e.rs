//! End-to-end tests: a real IronRDP client connects to `RdpServer` over loopback TLS,
//! with `vds-test-host` producing frames.
//!
//! Codec selection follows `vds_core::codec::select`; credentials follow QMP
//! `set_password` semantics.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration test helpers fail loudly (docs/spec/rust-style.md, section 4)"
)]

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ironrdp::connector::connection_activation::{ConnectionActivationFactory, ConnectionActivationState};
use ironrdp::connector::{self, ClientConnector, ConnectionResult};
use ironrdp::dvc::DrdynvcClient;
use ironrdp::pdu::fast_path::{FastPathHeader, FastPathUpdatePdu, UpdateCode};
use ironrdp::pdu::gcc;
use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::pointer::{ColorPointerAttribute, PointerAttribute};
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_async::{Framed, FramedWrite as _};
use ironrdp_cliprdr::CliprdrClient;
use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
    FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::{Decode as _, ReadCursor, WriteCursor};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::decode::{DecodedFrame, DecoderResult, H264Decoder};
use ironrdp_rdpsnd::client::{Rdpsnd, RdpsndClientHandler};
use ironrdp_rdpsnd::pdu::{AudioFormat, AudioFormatFlags, PitchPdu, VolumePdu, WaveFormat};
use ironrdp_server::PixelFormat;
use ironrdp_tls::TlsStream;
use ironrdp_tokio::TokioStream;
use parking_lot::Mutex;
use tokio::net::TcpStream;
use vds_core::Core;
use vds_core::auth::{ConnectedAction, Credentials};
use vds_core::codec::CodecSet;
use vds_core::config::{SasHotkey, ServerConfig};
use vds_core::event::{ClientEvent, ClipboardType, MouseButton};
use vds_core::secret::Secret;
use vds_rdp::{RdpServer, TlsIdentity};
use vds_test_host::{CLIPBOARD_TEXT, TestHost};

const W: u32 = 640;
const H: u32 = 480;
/// The size the guest switches to when a test changes resolution.
const W2: u32 = 800;
const H2: u32 = 600;
const USER: &str = "vds";
const PASS: &str = "s3cret-ticket";

type ClientFramed = Framed<TokioStream<TlsStream<TcpStream>>>;

struct Fixture {
    core: Core,
    server: Option<RdpServer>,
    host: Option<TestHost>,
    addr: SocketAddr,
}

impl Fixture {
    fn new(config: ServerConfig) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
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

    fn with_password(config: ServerConfig, one_time: bool) -> Self {
        let f = Self::new(config);
        f.core
            .set_password(
                Some(Credentials {
                    username: USER.into(),
                    password: Secret::new(PASS.into()),
                    domain: None,
                }),
                one_time,
                ConnectedAction::Keep,
            )
            .unwrap();
        f
    }

    fn host(&self) -> &TestHost {
        self.host.as_ref().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown();
        }
        if let Some(h) = self.host.take() {
            h.stop();
        }
    }
}

/// What the fake H.264 decoder received.
#[derive(Default)]
struct Seen {
    frames: Vec<Vec<u8>>,
}

/// Records AVC420 payloads instead of decoding them.
struct Recorder(Arc<Mutex<Seen>>);

impl H264Decoder for Recorder {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        self.0.lock().frames.push(data.to_vec());
        // Large enough for any destination rectangle used in these tests (the client crops).
        const DW: u32 = 2048;
        const DH: u32 = 1536;
        Ok(DecodedFrame::new(vec![0; DW as usize * DH as usize * 4], DW, DH))
    }

    fn reset(&mut self) {}
}

struct NoopGfx;
impl GraphicsPipelineHandler for NoopGfx {}

/// The one audio format the server offers (`vds-rdp/src/sound.rs`), as the client must
/// advertise it back to have anything streamed.
const PCM_48K_STEREO: AudioFormat = AudioFormat {
    format: WaveFormat::PCM,
    n_channels: 2,
    n_samples_per_sec: 48_000,
    n_avg_bytes_per_sec: 192_000,
    n_block_align: 4,
    bits_per_sample: 16,
    data: None,
};

/// Waves the audio test waits for: enough that the packing ran for a while past the few
/// chunks the session drops while the rdpsnd channel is still negotiating.
const WAVES: usize = 30;

/// What the fake speaker received.
#[derive(Debug, Default)]
struct Heard {
    /// `(media timestamp, bytes)` of every wave played.
    waves: Vec<(u32, usize)>,
    /// Every volume setting received.
    volumes: Vec<(u16, u16)>,
}

/// Plays into a counter instead of a sound card.
#[derive(Debug)]
struct Speaker(Arc<Mutex<Heard>>);

impl RdpsndClientHandler for Speaker {
    fn get_flags(&self) -> AudioFormatFlags {
        AudioFormatFlags::VOLUME
    }

    fn get_formats(&self) -> &[AudioFormat] {
        std::slice::from_ref(&PCM_48K_STEREO)
    }

    fn wave(&mut self, _format: &AudioFormat, ts: u32, data: Cow<'_, [u8]>) {
        self.0.lock().waves.push((ts, data.len()));
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        self.0.lock().volumes.push((volume.volume_left, volume.volume_right));
    }

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {}
}

/// The guest's PCM, handed to the server one piece at a time. The pieces are not
/// a whole wave — 180 and 100 frames, four to a 10 ms wave, as in the packing test — so whoever
/// drives the session has to keep feeding them for waves to come out.
struct AudioFeeder {
    core: Core,
    pieces: Vec<Vec<i16>>,
    next: usize,
}

impl AudioFeeder {
    fn new(core: Core) -> Self {
        Self {
            core,
            pieces: [180, 100, 100, 100].iter().map(|n| vec![0i16; n * 2]).collect(),
            next: 0,
        }
    }

    /// Hands the next piece to the guest.
    fn feed(&mut self) {
        self.core.audio_out(&self.pieces[self.next % self.pieces.len()]);
        self.next += 1;
    }
}

/// What the fake client clipboard backend saw.
#[derive(Debug, Default)]
struct ClipSeen {
    /// The handshake finished: the server accepted the client's own format list.
    ready: usize,
    /// The client was asked to advertise its formats (the server's Monitor Ready arrived).
    requested: usize,
    /// Format ids the host's clipboard offered, in the order the offers arrived.
    offered: Vec<u32>,
    /// Text that came back as the answer to a paste.
    pasted: Vec<String>,
}

/// Stands in for the client's clipboard: it has nothing of its own to offer, and records what
/// the host offers it.
#[derive(Debug)]
struct ClipBoard(Arc<Mutex<ClipSeen>>);

ironrdp_core::impl_as_any!(ClipBoard);

impl CliprdrBackend for ClipBoard {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        self.0.lock().ready += 1;
    }

    fn on_request_format_list(&mut self) {
        // An empty client clipboard: the handshake still needs the client's format list, so
        // the test answers this through `clipboard_action` (see the clipboard test).
        self.0.lock().requested += 1;
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        self.0
            .lock()
            .offered
            .extend(available_formats.iter().map(|f| f.id().value()));
    }

    fn on_format_data_request(&mut self, _request: FormatDataRequest) {}

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            return;
        }
        self.0.lock().pasted.push(utf16le(response.data()));
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

/// The `CF_UNICODETEXT` payload of a clipboard response (UTF-16LE, NUL-terminated).
fn utf16le(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Bits of FreeRDP's `MouseEventFlags` these tests send. `ironrdp` re-exports neither
/// `ainput` nor its flag type, so both the names and the wire format are repeated here.
const AINPUT_WHEEL: u64 = 0x0000_0001;
const AINPUT_MOVE: u64 = 0x0000_0004;
const AINPUT_DOWN: u64 = 0x0000_0008;
const AINPUT_BUTTON1: u64 = 0x0000_1000;

/// A mouse PDU for the `FreeRDP::Advanced::Input` channel.
///
/// Layout, little-endian: `u16` pdu type (2 = mouse), `u64` time, `u64` flags, `i32` x,
/// `i32` y. Positions are carried by every event, buttons and wheel included.
fn ainput_mouse(flags: u64, x: i32, y: i32) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(26);
    pdu.extend_from_slice(&2u16.to_le_bytes());
    pdu.extend_from_slice(&0u64.to_le_bytes());
    pdu.extend_from_slice(&flags.to_le_bytes());
    pdu.extend_from_slice(&x.to_le_bytes());
    pdu.extend_from_slice(&y.to_le_bytes());
    pdu
}

/// A DVC payload built by hand, for channels this crate has no dependency for.
struct RawDvc(Vec<u8>);

impl ironrdp_core::Encode for RawDvc {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> ironrdp_core::EncodeResult<()> {
        dst.write_slice(&self.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "AInputMousePdu"
    }

    fn size(&self) -> usize {
        self.0.len()
    }
}

impl ironrdp::dvc::DvcEncode for RawDvc {}

/// The client half of `FreeRDP::Advanced::Input`: accepts the DVC the server opens and sends
/// `pdus` on it as soon as it is created.
struct AInputClient {
    pdus: Vec<Vec<u8>>,
}

ironrdp_core::impl_as_any!(AInputClient);

impl ironrdp::dvc::DvcProcessor for AInputClient {
    fn channel_name(&self) -> &str {
        "FreeRDP::Advanced::Input"
    }

    fn start(&mut self, _channel_id: u32) -> ironrdp::pdu::PduResult<Vec<ironrdp::dvc::DvcMessage>> {
        Ok(std::mem::take(&mut self.pdus)
            .into_iter()
            .map(|pdu| Box::new(RawDvc(pdu)) as ironrdp::dvc::DvcMessage)
            .collect())
    }

    fn process(&mut self, _channel_id: u32, _payload: &[u8]) -> ironrdp::pdu::PduResult<Vec<ironrdp::dvc::DvcMessage>> {
        // The server opens with its version PDU; there is nothing to answer.
        Ok(Vec::new())
    }
}

impl ironrdp::dvc::DvcClientProcessor for AInputClient {}

fn client_config(user: &str, pass: &str, egfx: bool, audio: bool) -> connector::Config {
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
        client_name: "vds-e2e".into(),
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
        enable_audio_playback: audio,
        enable_audio_capture: false,
        license_cache: None,
        compression_type: None,
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: egfx,
        performance_flags: Default::default(),
        timezone_info: Default::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: ironrdp::pdu::rdp::capability_sets::RailSupportLevel::SUPPORTED,
    }
}

async fn connect(
    addr: SocketAddr,
    user: &str,
    pass: &str,
    egfx: Option<Arc<Mutex<Seen>>>,
) -> anyhow::Result<(ActiveStage, ClientFramed)> {
    connect_with(addr, user, pass, egfx, None).await
}

async fn connect_with(
    addr: SocketAddr,
    user: &str,
    pass: &str,
    egfx: Option<Arc<Mutex<Seen>>>,
    audio: Option<Arc<Mutex<Heard>>>,
) -> anyhow::Result<(ActiveStage, ClientFramed)> {
    connect_full(addr, user, pass, egfx, audio, None).await
}

/// As [`connect_with`], plus the `FreeRDP::Advanced::Input` DVC carrying `ainput` when given.
async fn connect_full(
    addr: SocketAddr,
    user: &str,
    pass: &str,
    egfx: Option<Arc<Mutex<Seen>>>,
    audio: Option<Arc<Mutex<Heard>>>,
    ainput: Option<Vec<Vec<u8>>>,
) -> anyhow::Result<(ActiveStage, ClientFramed)> {
    let has_egfx = egfx.is_some();
    let has_audio = audio.is_some();
    let mut dvc = DrdynvcClient::new();
    let mut wants_dvc = false;
    if let Some(seen) = egfx {
        let gfx = GraphicsPipelineClient::new(Box::new(NoopGfx), Some(Box::new(Recorder(seen))));
        dvc = dvc.with_dynamic_channel(gfx);
        wants_dvc = true;
    }
    if let Some(pdus) = ainput {
        dvc = dvc.with_dynamic_channel(AInputClient { pdus });
        wants_dvc = true;
    }
    let audio = audio.map(|heard| Rdpsnd::new(Box::new(Speaker(heard))));
    let (stage, framed, _) = connect_generic(
        addr,
        client_config(user, pass, has_egfx, has_audio),
        wants_dvc.then_some(dvc),
        audio,
        None,
    )
    .await?;
    Ok((stage, framed))
}

/// TCP, TLS upgrade, the connector run and the active stage — the plumbing every test client
/// shares. Also returns the Connection Activation factory, which is what the
/// Deactivation-Reactivation Sequence is replayed from.
async fn connect_generic(
    addr: SocketAddr,
    config: connector::Config,
    dvc: Option<DrdynvcClient>,
    audio: Option<Rdpsnd>,
    cliprdr: Option<CliprdrClient>,
) -> anyhow::Result<(ActiveStage, ClientFramed, ConnectionActivationFactory)> {
    let tcp = TcpStream::connect(addr).await?;
    let client_addr = tcp.local_addr()?;
    let mut framed = ironrdp_tokio::TokioFramed::new(tcp);
    let mut connector = ClientConnector::new(config, client_addr);
    if let Some(dvc) = dvc {
        connector = connector.with_static_channel(dvc);
    }
    if let Some(audio) = audio {
        connector = connector.with_static_channel(audio);
    }
    if let Some(cliprdr) = cliprdr {
        connector = connector.with_static_channel(cliprdr);
    }
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
    let factory = result.activation_factory;
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
    Ok((stage, framed, factory))
}

/// Replays the client half of a Deactivation-Reactivation Sequence and returns what the new
/// activation negotiated: the desktop size the server demands, the share id, and the pointer
/// and chunk-size settings the active stage has to adopt.
async fn run_reactivation(
    framed: &mut ClientFramed,
    factory: &ConnectionActivationFactory,
) -> (connector::DesktopSize, u32, bool, bool, usize) {
    let mut sequence = factory.create();
    let mut buf = ironrdp_core::WriteBuf::new();
    loop {
        let written = ironrdp_async::single_sequence_step_read(framed, &mut sequence, &mut buf)
            .await
            .expect("deactivation-reactivation sequence step");
        if written.size().is_some() {
            framed
                .write_all(buf.filled())
                .await
                .expect("write deactivation-reactivation sequence step");
        }
        if let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            enable_server_pointer,
            pointer_software_rendering,
            static_channel_chunk_size,
            ..
        } = sequence.connection_activation_state()
        {
            return (
                desktop_size,
                share_id,
                enable_server_pointer,
                pointer_software_rendering,
                static_channel_chunk_size,
            );
        }
    }
}

/// Processes server PDUs until the session has been through one Deactivation-Reactivation and
/// has produced two graphics updates at the size the server demanded, then returns how many
/// updates it saw afterwards. `tick` runs once per iteration, for tests that must keep the
/// guest producing something (audio) while they wait.
async fn pump_until_reactivated(
    stage: &mut ActiveStage,
    framed: &mut ClientFramed,
    image: &mut DecodedImage,
    factory: &ConnectionActivationFactory,
    expect: (u16, u16),
    mut tick: impl FnMut(),
) -> usize {
    let mut reactivated = false;
    let mut after = 0;
    let limit = tokio::time::Instant::now() + Duration::from_secs(20);
    while after < 2 {
        let (action, payload) = tokio::time::timeout_at(limit, framed.read_pdu())
            .await
            .expect("no update after the resolution change")
            .unwrap();
        for out in stage.process(image, action, &payload).unwrap() {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await.unwrap(),
                ActiveStageOutput::GraphicsUpdate(_) => {
                    if reactivated {
                        after += 1;
                    }
                }
                ActiveStageOutput::DeactivateAll => {
                    assert!(!reactivated, "the server deactivated the session twice");
                    let (size, share_id, server_pointer, software_rendering, chunk) =
                        run_reactivation(framed, factory).await;
                    assert_eq!(
                        (size.width, size.height),
                        expect,
                        "the reactivation must demand the size the host was resized to"
                    );
                    assert!(stage.reactivate(
                        factory.io_channel_id(),
                        factory.user_channel_id(),
                        share_id,
                        server_pointer,
                        software_rendering,
                        chunk,
                    ));
                    *image = DecodedImage::new(PixelFormat::RgbA32, size.width, size.height);
                    reactivated = true;
                }
                _ => {}
            }
        }
        tick();
    }
    assert!(reactivated, "no Deactivation-Reactivation for {expect:?}");
    after
}

/// What the test drives on the client's clipboard channel. Both are user actions, not the
/// channel's own replies: an empty client clipboard still has to be advertised for the
/// handshake to finish, and a paste is what asks the host for its text.
enum ClipAction {
    Advertise,
    Paste,
}

/// Performs one clipboard action and puts the resulting PDUs on the wire.
async fn clipboard_action(stage: &mut ActiveStage, framed: &mut ClientFramed, action: ClipAction) {
    let messages = {
        let cliprdr = stage
            .get_svc_processor_mut::<CliprdrClient>()
            .expect("the clipboard channel is not in the session");
        match action {
            ClipAction::Advertise => cliprdr.initiate_copy(&[]),
            ClipAction::Paste => cliprdr.initiate_paste(ClipboardFormatId::CF_UNICODETEXT),
        }
        .expect("encode the clipboard PDU")
    };
    let frame = stage
        .process_svc_processor_messages::<CliprdrClient>(messages)
        .expect("encode the clipboard channel frame");
    framed
        .write_all(&frame)
        .await
        .expect("send the clipboard channel frame");
}

/// Processes server PDUs until `done` returns true; returns the number of legacy graphics
/// updates. Fails on timeout or when the server closes the connection.
async fn pump(
    stage: &mut ActiveStage,
    framed: &mut ClientFramed,
    timeout: Duration,
    mut done: impl FnMut(usize) -> bool,
) -> anyhow::Result<usize> {
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
    Ok(updates)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn nal_types(annex_b: &[u8]) -> Vec<u8> {
    annex_b
        .windows(4)
        .enumerate()
        .filter(|(_, w)| w[..3] == [0, 0, 1])
        .filter_map(|(i, _)| annex_b.get(i + 3).map(|b| b & 0x1f))
        .collect()
}

#[test]
fn nla_egfx_client_receives_annex_b_avc420_frames() {
    let f = Fixture::with_password(ServerConfig::default(), false);
    let seen = Arc::new(Mutex::new(Seen::default()));
    runtime().block_on(async {
        let (mut stage, mut framed) = connect(f.addr, USER, PASS, Some(Arc::clone(&seen))).await.unwrap();
        let s = Arc::clone(&seen);
        pump(&mut stage, &mut framed, Duration::from_secs(20), |_| {
            s.lock().frames.len() >= 10
        })
        .await
        .unwrap();
        // Checked while the client is still connected.
        let clients = f.core.clients();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].codec, Some("avc420"));
        assert_eq!(clients[0].user.as_deref(), Some(USER));
    });
    let seen = seen.lock();
    let first = &seen.frames[0];
    assert_eq!(&first[..4], &[0, 0, 0, 1], "AVC420 payload must be Annex-B");
    let types = nal_types(first);
    assert!(
        types.contains(&7) && types.contains(&8) && types.contains(&5),
        "first frame is an IDR with SPS/PPS: {types:?}"
    );
    // Later frames are P frames, much smaller than the IDR.
    let later = &seen.frames[seen.frames.len() - 1];
    assert!(!nal_types(later).contains(&5));
    assert!(f.host().stats().initialized >= 1);
}

/// What the client saw on the EGFX channel (for the surface-rebuild checks).
#[derive(Default)]
struct GfxEvents {
    capabilities_confirmed: usize,
    reset_graphics: Vec<(u32, u32)>,
    surfaces_created: usize,
    surfaces_mapped: usize,
}

/// Records the EGFX pipeline's lifecycle events instead of ignoring them.
struct GfxWatch(Arc<Mutex<GfxEvents>>);

impl GraphicsPipelineHandler for GfxWatch {
    fn on_capabilities_confirmed(&mut self, _caps: &ironrdp_egfx::pdu::CapabilitySet) {
        self.0.lock().capabilities_confirmed += 1;
    }

    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        self.0.lock().reset_graphics.push((width, height));
    }

    fn on_surface_created(&mut self, _surface: &ironrdp_egfx::client::Surface) {
        self.0.lock().surfaces_created += 1;
    }

    fn on_surface_mapped(&mut self, _surface_id: u16, _origin_x: u32, _origin_y: u32) {
        self.0.lock().surfaces_mapped += 1;
    }
}

/// Legacy transport: the guest changes resolution, the server sends Deactivate
/// All, and the client that replays the Deactivation-Reactivation Sequence gets updates again
/// at the size the server now demands — the session survives the change instead of stalling.
#[test]
fn legacy_resolution_change_reactivates_the_session() {
    let f = Fixture::with_password(ServerConfig::default(), false);
    runtime().block_on(async {
        let (mut stage, mut framed, factory) =
            connect_generic(f.addr, client_config(USER, PASS, false, false), None, None, None)
                .await
                .unwrap();
        let (w, h) = (u16::try_from(W).unwrap(), u16::try_from(H).unwrap());
        let mut image = DecodedImage::new(PixelFormat::RgbA32, w, h);

        // The client is driving a session at the size it started with.
        let mut seen = 0;
        let first = tokio::time::Instant::now() + Duration::from_secs(20);
        while seen < 2 {
            let (action, payload) = tokio::time::timeout_at(first, framed.read_pdu())
                .await
                .expect("no update before the resolution change")
                .unwrap();
            for out in stage.process(&mut image, action, &payload).unwrap() {
                match out {
                    ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await.unwrap(),
                    ActiveStageOutput::GraphicsUpdate(_) => seen += 1,
                    _ => {}
                }
            }
        }
        assert_eq!((image.width(), image.height()), (w, h));

        // The guest changes resolution.
        f.core.display().head(0).unwrap().resize(W2, H2).expect("resize head");

        let mut reactivated = false;
        let mut after = 0;
        let limit = tokio::time::Instant::now() + Duration::from_secs(20);
        while after < 2 {
            let (action, payload) = tokio::time::timeout_at(limit, framed.read_pdu())
                .await
                .expect("no update after the resolution change")
                .unwrap();
            for out in stage.process(&mut image, action, &payload).unwrap() {
                match out {
                    ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await.unwrap(),
                    ActiveStageOutput::GraphicsUpdate(_) => {
                        if reactivated {
                            after += 1;
                        }
                    }
                    ActiveStageOutput::DeactivateAll => {
                        assert!(!reactivated, "the server deactivated the session twice");
                        let (size, share_id, server_pointer, software_rendering, chunk) =
                            run_reactivation(&mut framed, &factory).await;
                        assert_eq!(
                            (size.width, size.height),
                            (u16::try_from(W2).unwrap(), u16::try_from(H2).unwrap()),
                            "the reactivation must demand the new desktop size"
                        );
                        assert!(stage.reactivate(
                            factory.io_channel_id(),
                            factory.user_channel_id(),
                            share_id,
                            server_pointer,
                            software_rendering,
                            chunk,
                        ));
                        image = DecodedImage::new(PixelFormat::RgbA32, size.width, size.height);
                        reactivated = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(
            reactivated,
            "the resolution change produced no Deactivation-Reactivation"
        );
        assert_eq!(
            (image.width(), image.height()),
            (u16::try_from(W2).unwrap(), u16::try_from(H2).unwrap())
        );
    });
}

/// In mstsc's shape: the client asks for a new resolution over the Display Control
/// channel, the server applies it to the guest display, then deactivates the session and
/// demands the new size. The client replays the sequence, updates resume — and the channel
/// still works afterwards, so a second request is honoured too.
#[test]
fn display_control_resize_reactivates_the_session() {
    let f = Fixture::with_password(ServerConfig::default(), false);
    runtime().block_on(async {
        // The client registers the Display Control channel but does not act on the server's
        // capabilities: this test decides when a resize is asked for.
        let display_control =
            DisplayControlClient::new(|_caps| -> ironrdp::pdu::PduResult<Vec<ironrdp::dvc::DvcMessage>> {
                Ok(Vec::new())
            });
        let dvc = DrdynvcClient::new().with_dynamic_channel(display_control);
        let (mut stage, mut framed, factory) =
            connect_generic(f.addr, client_config(USER, PASS, false, false), Some(dvc), None, None)
                .await
                .unwrap();
        let (w, h) = (u16::try_from(W).unwrap(), u16::try_from(H).unwrap());
        let mut image = DecodedImage::new(PixelFormat::RgbA32, w, h);

        // Until the channel has its capabilities and the session is live.
        let mut seen = 0;
        let limit = tokio::time::Instant::now() + Duration::from_secs(20);
        while seen < 2 || stage.display_control_ready() != Some(true) {
            let (action, payload) = tokio::time::timeout_at(limit, framed.read_pdu())
                .await
                .expect("the Display Control channel never became ready")
                .unwrap();
            for out in stage.process(&mut image, action, &payload).unwrap() {
                match out {
                    ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await.unwrap(),
                    ActiveStageOutput::GraphicsUpdate(_) => seen += 1,
                    _ => {}
                }
            }
        }

        // mstsc resizing its window: the resolution goes over Display Control.
        let request = stage
            .encode_resize(W2, H2, None, None)
            .expect("Display Control channel is not available")
            .expect("encode the resize request");
        framed.write_all(&request).await.unwrap();
        pump_until_reactivated(
            &mut stage,
            &mut framed,
            &mut image,
            &factory,
            (W2 as u16, H2 as u16),
            || {},
        )
        .await;
        assert_eq!(
            f.core.display().head(0).unwrap().size(),
            (W2, H2),
            "the host must have applied the requested resolution"
        );
        assert!(f.host().stats().layouts >= 1, "the request must reach the host");

        // Still usable after the reactivation.
        assert_eq!(stage.display_control_ready(), Some(true));
        let request = stage
            .encode_resize(W, H, None, None)
            .expect("Display Control channel is not available")
            .expect("encode the second resize request");
        framed.write_all(&request).await.unwrap();
        pump_until_reactivated(&mut stage, &mut framed, &mut image, &factory, (w, h), || {}).await;
        assert_eq!(f.core.display().head(0).unwrap().size(), (W, H));
        assert!(
            f.host().stats().layouts >= 2,
            "the second request must reach the host too"
        );
    });
}

/// EGFX transport: a resolution change is not a Deactivation-Reactivation here —
/// the pipeline answers with a new ResetGraphics and rebuilt surfaces on the Graphics channel,
/// and frames keep flowing at the new size.
#[test]
fn egfx_resolution_change_resets_graphics_and_rebuilds_surfaces() {
    let f = Fixture::with_password(ServerConfig::default(), false);
    let events = Arc::new(Mutex::new(GfxEvents::default()));
    let seen = Arc::new(Mutex::new(Seen::default()));
    runtime().block_on(async {
        let gfx = GraphicsPipelineClient::new(
            Box::new(GfxWatch(Arc::clone(&events))),
            Some(Box::new(Recorder(Arc::clone(&seen)))),
        );
        let dvc = DrdynvcClient::new().with_dynamic_channel(gfx);
        let (mut stage, mut framed, _) =
            connect_generic(f.addr, client_config(USER, PASS, true, false), Some(dvc), None, None)
                .await
                .unwrap();

        let e = Arc::clone(&events);
        let s = Arc::clone(&seen);
        let started = move |_| {
            let (e, s) = (e.lock(), s.lock());
            e.capabilities_confirmed > 0
                && !e.reset_graphics.is_empty()
                && e.surfaces_created > 0
                && s.frames.len() >= 5
        };
        // The EGFX session is up: capabilities confirmed, graphics reset to the initial size,
        // and surfaces created and mapped for the head.
        pump(&mut stage, &mut framed, Duration::from_secs(30), started)
            .await
            .unwrap();
        let (resets_before, created_before, mapped_before) = {
            let e = events.lock();
            (e.reset_graphics.len(), e.surfaces_created, e.surfaces_mapped)
        };
        assert_eq!(events.lock().reset_graphics.last().copied(), Some((W, H)));
        assert!(mapped_before >= 1, "the head's surface must be mapped to an output");

        f.core.display().head(0).unwrap().resize(W2, H2).expect("resize head");

        let e = Arc::clone(&events);
        let s = Arc::clone(&seen);
        let rebuilt = move |_| {
            let (e, s) = (e.lock(), s.lock());
            e.reset_graphics.len() > resets_before
                && e.reset_graphics.last() == Some(&(W2, H2))
                && e.surfaces_created > created_before
                && e.surfaces_mapped > mapped_before
                && s.frames.len() >= 5
        };
        pump(&mut stage, &mut framed, Duration::from_secs(30), rebuilt)
            .await
            .unwrap();

        let e = events.lock();
        assert_eq!(
            e.reset_graphics.first().copied(),
            Some((W, H)),
            "the pipeline resets graphics once at connect"
        );
        assert_eq!(e.reset_graphics.last().copied(), Some((W2, H2)));
        assert!(
            e.surfaces_created > created_before,
            "the resize must rebuild the surfaces ({} before, {} after)",
            created_before,
            e.surfaces_created
        );
        assert!(e.surfaces_mapped > mapped_before);
    });
}

/// The client alone closes the Graphics DVC. The server has to notice, drop the
/// EGFX transport and fall back to legacy bitmaps rather than carrying on sending AVC420 into
/// a channel nobody is listening on.
///
/// Reopening is not something this test can drive, and that is a property of DRDYNVC rather
/// than of libvds: the *server* owns channel creation (it sends the Create Request once, when
/// it sees the client's initial DVC Capabilities Response, `ironrdp-dvc/src/server.rs`), and
/// nothing re-requests a channel the client closed. Getting EGFX back means a new connection,
/// which the fixture's server handles by negotiating the DVCs again.
#[test]
fn closing_the_graphics_dvc_falls_back_to_legacy() {
    let f = Fixture::with_password(ServerConfig::default(), false);
    let seen = Arc::new(Mutex::new(Seen::default()));
    runtime().block_on(async {
        let gfx = GraphicsPipelineClient::new(Box::new(NoopGfx), Some(Box::new(Recorder(Arc::clone(&seen)))));
        let dvc = DrdynvcClient::new().with_dynamic_channel(gfx);
        let (mut stage, mut framed, factory) =
            connect_generic(f.addr, client_config(USER, PASS, true, false), Some(dvc), None, None)
                .await
                .unwrap();
        let mut image = DecodedImage::new(
            PixelFormat::RgbA32,
            u16::try_from(W).unwrap(),
            u16::try_from(H).unwrap(),
        );

        // The EGFX session is up …
        let s = Arc::clone(&seen);
        pump(&mut stage, &mut framed, Duration::from_secs(30), move |_| {
            s.lock().frames.len() >= 5
        })
        .await
        .unwrap();
        assert_eq!(f.core.clients()[0].codec, Some("avc420"));

        // … and the client closes the channel on its own.
        //
        // The Close PDU is written by hand rather than with `DrdynvcClient::close_channel`,
        // which also forgets the channel locally: IronRDP's own DVC client treats any later
        // data for a channel it no longer knows as a fatal protocol error, and frames the
        // server had already encoded are still on their way when the close goes out. The
        // server side is what this test is about, so the local registration stays.
        let channel_id = stage
            .get_svc_processor::<DrdynvcClient>()
            .expect("the DRDYNVC channel must be open")
            .get_dvc::<GraphicsPipelineClient>()
            .expect("the graphics channel must be open")
            .channel_id();
        let close = ironrdp_svc::SvcMessage::from(ironrdp_dvc::pdu::DrdynvcClientPdu::Close(
            ironrdp_dvc::pdu::ClosePdu::new(channel_id),
        ))
        .with_flags(ironrdp_svc::ChannelFlags::SHOW_PROTOCOL);
        let frame = stage
            .process_svc_processor_messages::<DrdynvcClient>(ironrdp_svc::SvcProcessorMessages::new(vec![close]))
            .unwrap();
        framed.write_all(&frame).await.unwrap();

        // The server drops EGFX. Because it cannot know the client's legacy desktop any more it
        // deactivates the session first; after the sequence, plain bitmap updates arrive.
        let mut updates = 0;
        let mut deactivations = 0;
        let limit = tokio::time::Instant::now() + Duration::from_secs(30);
        while updates < 2 {
            let (action, payload) = tokio::time::timeout_at(limit, framed.read_pdu())
                .await
                .expect("no legacy fallback after the graphics channel closed")
                .unwrap();
            for out in stage.process(&mut image, action, &payload).unwrap() {
                match out {
                    ActiveStageOutput::ResponseFrame(f) => framed.write_all(&f).await.unwrap(),
                    ActiveStageOutput::GraphicsUpdate(_) => updates += 1,
                    ActiveStageOutput::DeactivateAll => {
                        let (size, share_id, server_pointer, software_rendering, chunk) =
                            run_reactivation(&mut framed, &factory).await;
                        assert_eq!(
                            (size.width, size.height),
                            (u16::try_from(W).unwrap(), u16::try_from(H).unwrap())
                        );
                        assert!(stage.reactivate(
                            factory.io_channel_id(),
                            factory.user_channel_id(),
                            share_id,
                            server_pointer,
                            software_rendering,
                            chunk,
                        ));
                        image = DecodedImage::new(PixelFormat::RgbA32, size.width, size.height);
                        deactivations += 1;
                    }
                    _ => {}
                }
            }
        }
        assert!(
            deactivations <= 1,
            "the fallback deactivated the session {deactivations} times"
        );
        let clients = f.core.clients();
        assert_eq!(
            clients[0].codec,
            Some("legacy"),
            "the session must report the transport it fell back to"
        );
        assert_eq!(f.core.clients().len(), 1, "the fallback must not drop the client");

        // And getting EGFX back means a new connection, as the note on this test explains: the
        // fresh session must negotiate the Graphics DVC and AVC420 all over again.
        let again = Arc::new(Mutex::new(Seen::default()));
        let gfx = GraphicsPipelineClient::new(Box::new(NoopGfx), Some(Box::new(Recorder(Arc::clone(&again)))));
        let dvc = DrdynvcClient::new().with_dynamic_channel(gfx);
        let (mut stage, mut framed, _) =
            connect_generic(f.addr, client_config(USER, PASS, true, false), Some(dvc), None, None)
                .await
                .unwrap();
        let s = Arc::clone(&again);
        pump(&mut stage, &mut framed, Duration::from_secs(30), move |_| {
            s.lock().frames.len() >= 5
        })
        .await
        .unwrap();
        assert!(
            !again.lock().frames.is_empty(),
            "the new connection must carry AVC420 again"
        );
        assert_eq!(f.core.clients().last().unwrap().codec, Some("avc420"));
    });
}

/// The guest's PCM reaches the client as fixed 10 ms waves whose timestamps
/// count samples rather than the wall clock, and a muted guest volume is sent as silence.
#[test]
fn guest_audio_arrives_as_ten_millisecond_waves() {
    let f = Fixture::with_password(ServerConfig::default(), false);
    let heard = Arc::new(Mutex::new(Heard::default()));
    runtime().block_on(async {
        let (mut stage, mut framed) = connect_with(f.addr, USER, PASS, None, Some(Arc::clone(&heard)))
            .await
            .unwrap();
        let core = f.core.clone();
        let h = Arc::clone(&heard);
        // The guest hands its PCM over in pieces that are not a whole wave — 180 and 100
        // frames, four of them to a 10 ms wave — so a wave only reaches the client every
        // fourth time round, and every one of them has to come out 1 920 bytes long.
        //
        // One piece per PDU read: the guest then produces less audio than the client drains,
        // so nothing queues up and neither the server's stale-wave rule nor the backlog rule
        // has anything to do. This test is about the packing, not about catching up.
        let pieces: Vec<Vec<i16>> = [180, 100, 100, 100].iter().map(|n| vec![0i16; n * 2]).collect();
        let mut next = 0;
        // The channel negotiates as PDUs flow, so the guest keeps talking until the client has
        // heard enough; the volume is repeated until it lands, since a setting sent before the
        // stream starts is dropped.
        pump(&mut stage, &mut framed, Duration::from_secs(30), move |_| {
            let (waves, volumes) = {
                let heard = h.lock();
                (heard.waves.len(), heard.volumes.len())
            };
            if waves < WAVES {
                core.audio_out(&pieces[next % pieces.len()]);
                next += 1;
            }
            if volumes < 2 {
                core.audio_volume(0xFFFF, 0x8000, false);
                core.audio_volume(0xFFFF, 0x8000, true);
            }
            waves >= WAVES && volumes >= 2
        })
        .await
        .unwrap();
    });

    let heard = heard.lock();
    assert!(
        heard.waves.len() >= WAVES,
        "the client heard {} waves",
        heard.waves.len()
    );
    let sizes: Vec<usize> = heard.waves.iter().map(|(_, len)| *len).collect();
    assert!(
        sizes.iter().all(|len| *len == 1920),
        "every wave is 10 ms of 48 kHz stereo, whatever the guest's chunk size: {sizes:?}"
    );
    // Every wave carries a whole number of 10 ms waves of media time, wherever the guest's
    // pieces fell: a wall-clock timestamp would carry whatever fraction of a wave had
    // accumulated when the wave happened to be sent. The steps are not all 10 ms — the server
    // withholds audio the client has not played, and drops a burst outright, and the media
    // clock walks over both — but it never moves in anything but whole waves.
    let ts: Vec<u32> = heard.waves.iter().map(|(ts, _)| *ts).collect();
    for (before, after) in ts.iter().zip(&ts[1..]) {
        assert!(
            after > before && (after - before) % 10 == 0,
            "media timestamps advance in whole waves: {ts:?}"
        );
    }
    assert!(
        heard.volumes.contains(&(0xFFFF, 0x8000)),
        "the guest volume reached the client: {:?}",
        heard.volumes
    );
    assert!(
        heard.volumes.contains(&(0, 0)),
        "a muted guest volume is sent as silence: {:?}",
        heard.volumes
    );
}

/// Audio: rdpsnd is a static channel, so a resolution change — which the legacy
/// transport answers with a Deactivation-Reactivation — must leave the stream alone. The guest
/// keeps producing PCM across the resize and the client keeps hearing whole 10 ms waves.
#[test]
fn audio_survives_a_reactivation() {
    /// Waves the client hears before the resolution change, and again after it.
    const WAVES_EACH_SIDE: usize = 5;

    /// Guest audio and server PDUs, one piece per PDU read, until the client has heard `target`
    /// waves in total.
    async fn pump_waves(
        stage: &mut ActiveStage,
        framed: &mut ClientFramed,
        image: &mut DecodedImage,
        feeder: &mut AudioFeeder,
        heard: &Arc<Mutex<Heard>>,
        target: usize,
    ) {
        let limit = tokio::time::Instant::now() + Duration::from_secs(30);
        while heard.lock().waves.len() < target {
            let (action, payload) = tokio::time::timeout_at(limit, framed.read_pdu())
                .await
                .expect("the client never heard enough audio")
                .unwrap();
            for out in stage.process(image, action, &payload).unwrap() {
                if let ActiveStageOutput::ResponseFrame(frame) = out {
                    framed.write_all(&frame).await.unwrap();
                }
            }
            feeder.feed();
        }
    }

    let f = Fixture::with_password(ServerConfig::default(), false);
    let heard = Arc::new(Mutex::new(Heard::default()));
    runtime().block_on(async {
        let (mut stage, mut framed, factory) = connect_generic(
            f.addr,
            client_config(USER, PASS, false, true),
            None,
            Some(Rdpsnd::new(Box::new(Speaker(Arc::clone(&heard))))),
            None,
        )
        .await
        .unwrap();
        let (w, h) = (u16::try_from(W).unwrap(), u16::try_from(H).unwrap());
        let mut image = DecodedImage::new(PixelFormat::RgbA32, w, h);
        let mut feeder = AudioFeeder::new(f.core.clone());

        pump_waves(
            &mut stage,
            &mut framed,
            &mut image,
            &mut feeder,
            &heard,
            WAVES_EACH_SIDE,
        )
        .await;

        // The guest changes resolution; the guest also keeps talking while the session is
        // deactivated and reactivated.
        f.core.display().head(0).unwrap().resize(W2, H2).expect("resize head");
        pump_until_reactivated(
            &mut stage,
            &mut framed,
            &mut image,
            &factory,
            (u16::try_from(W2).unwrap(), u16::try_from(H2).unwrap()),
            || feeder.feed(),
        )
        .await;
        let waves_at_reactivation = heard.lock().waves.len();

        pump_waves(
            &mut stage,
            &mut framed,
            &mut image,
            &mut feeder,
            &heard,
            waves_at_reactivation + WAVES_EACH_SIDE,
        )
        .await;

        let heard = heard.lock();
        let after: Vec<usize> = heard.waves[waves_at_reactivation..]
            .iter()
            .map(|(_, len)| *len)
            .collect();
        assert!(
            after.len() >= WAVES_EACH_SIDE,
            "only {} waves arrived after the reactivation",
            after.len()
        );
        assert!(
            after.iter().all(|len| *len == 1920),
            "audio after the reactivation is still packed into 10 ms waves: {after:?}"
        );
    });
}

/// Clipboard: cliprdr is a static channel too, so after a resolution change and
/// the Deactivation-Reactivation it causes, the host's clipboard must still reach the client and
/// a paste must still come back with the host's text.
#[test]
fn clipboard_survives_a_reactivation() {
    /// `CF_UNICODETEXT`, the one format libvds shares.
    const TEXT: u32 = 13;

    /// Processes PDUs until `until` is satisfied, checking before every read; the channel's
    /// own replies have to go back out, which is what the handshake is made of.
    async fn drive(
        stage: &mut ActiveStage,
        framed: &mut ClientFramed,
        image: &mut DecodedImage,
        mut until: impl FnMut() -> bool,
    ) {
        let limit = tokio::time::Instant::now() + Duration::from_secs(20);
        while !until() {
            let (action, payload) = tokio::time::timeout_at(limit, framed.read_pdu())
                .await
                .expect("the clipboard never got where the test needed it")
                .unwrap();
            for out in stage.process(image, action, &payload).unwrap() {
                if let ActiveStageOutput::ResponseFrame(frame) = out {
                    framed.write_all(&frame).await.unwrap();
                }
            }
        }
    }

    let f = Fixture::with_password(ServerConfig::default(), false);
    let seen = Arc::new(Mutex::new(ClipSeen::default()));
    runtime().block_on(async {
        let (mut stage, mut framed, factory) = connect_generic(
            f.addr,
            client_config(USER, PASS, false, false),
            None,
            None,
            Some(CliprdrClient::new(Box::new(ClipBoard(Arc::clone(&seen))))),
        )
        .await
        .unwrap();
        let (w, h) = (u16::try_from(W).unwrap(), u16::try_from(H).unwrap());
        let mut image = DecodedImage::new(PixelFormat::RgbA32, w, h);

        // The handshake: the server asks for the client's formats, the client answers with its
        // (empty) clipboard, and the server accepts it.
        drive(&mut stage, &mut framed, &mut image, || seen.lock().requested > 0).await;
        clipboard_action(&mut stage, &mut framed, ClipAction::Advertise).await;
        drive(&mut stage, &mut framed, &mut image, || seen.lock().ready > 0).await;

        // The guest copies text: the host's formats reach the client.
        f.core.clipboard_grab(vec![ClipboardType::Text]);
        drive(&mut stage, &mut framed, &mut image, || {
            seen.lock().offered.contains(&TEXT)
        })
        .await;

        // A paste on the client comes back with the host's text.
        let host_events = f.host().stats().clipboard;
        clipboard_action(&mut stage, &mut framed, ClipAction::Paste).await;
        drive(&mut stage, &mut framed, &mut image, || !seen.lock().pasted.is_empty()).await;
        assert_eq!(seen.lock().pasted[0], CLIPBOARD_TEXT);
        assert!(
            f.host().stats().clipboard > host_events,
            "the paste must reach the host"
        );

        // The guest changes resolution: the legacy transport deactivates and reactivates the
        // session.
        f.core.display().head(0).unwrap().resize(W2, H2).expect("resize head");
        pump_until_reactivated(
            &mut stage,
            &mut framed,
            &mut image,
            &factory,
            (u16::try_from(W2).unwrap(), u16::try_from(H2).unwrap()),
            || {},
        )
        .await;

        // The clipboard still works: the host copies again, and a paste comes back with its
        // text, this time in a session that has been through a reactivation.
        let host_events = f.host().stats().clipboard;
        let offers = seen.lock().offered.len();
        f.core.clipboard_grab(vec![ClipboardType::Text]);
        drive(&mut stage, &mut framed, &mut image, || {
            seen.lock().offered.len() > offers
        })
        .await;

        let pasted = seen.lock().pasted.len();
        clipboard_action(&mut stage, &mut framed, ClipAction::Paste).await;
        drive(&mut stage, &mut framed, &mut image, || {
            seen.lock().pasted.len() > pasted
        })
        .await;

        let seen = seen.lock();
        assert!(
            seen.pasted.iter().all(|text| text == CLIPBOARD_TEXT),
            "every paste comes back with the host's text: {:?}",
            seen.pasted
        );
        assert!(
            f.host().stats().clipboard > host_events,
            "the paste after the reactivation must reach the host too"
        );
    });
}

#[test]
fn client_without_egfx_gets_legacy_updates() {
    let config = ServerConfig {
        egfx_grace_ms: 300,
        ..ServerConfig::default()
    };
    let f = Fixture::with_password(config, false);
    runtime().block_on(async {
        let (mut stage, mut framed) = connect(f.addr, USER, PASS, None).await.unwrap();
        let n = pump(&mut stage, &mut framed, Duration::from_secs(20), |n| n >= 5)
            .await
            .unwrap();
        assert!(n >= 5);
        assert_eq!(f.core.clients()[0].codec, Some("legacy"));
    });
}

#[test]
fn client_cursor_is_hidden_until_the_guest_defines_one() {
    // The guest draws its own cursor into the framebuffer when the display device has no
    // hardware cursor; a visible client arrow on top of it shows two cursors.
    let f = Fixture::with_password(ServerConfig::default(), false);
    runtime().block_on(async {
        let (mut stage, mut framed) = connect(f.addr, USER, PASS, None).await.unwrap();
        let mut image = DecodedImage::new(
            PixelFormat::RgbA32,
            u16::try_from(W).unwrap(),
            u16::try_from(H).unwrap(),
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let (action, payload) = tokio::time::timeout_at(deadline, framed.read_pdu())
                .await
                .expect("no pointer update before timeout")
                .unwrap();
            if action == ironrdp::pdu::Action::FastPath {
                let mut cursor = ReadCursor::new(&payload);
                FastPathHeader::decode(&mut cursor).unwrap();
                while !cursor.is_empty() {
                    let update = FastPathUpdatePdu::decode(&mut cursor).unwrap();
                    let mut pointer_cursor = ReadCursor::new(update.data);
                    let pointer = match update.update_code {
                        UpdateCode::ColorPointer => ColorPointerAttribute::decode(&mut pointer_cursor).unwrap(),
                        UpdateCode::NewPointer => PointerAttribute::decode(&mut pointer_cursor).unwrap().color_pointer,
                        _ => continue,
                    };
                    let mask_length = usize::from(pointer.width).div_ceil(16) * 2 * usize::from(pointer.height);
                    assert_eq!(
                        pointer.and_mask.len(),
                        mask_length,
                        "hidden cursor needs a complete AND mask"
                    );
                    assert!(pointer.and_mask.iter().all(|&value| value == 0xff));
                    assert!(pointer.xor_mask.iter().all(|&value| value == 0));
                }
            }
            let (mut hidden, mut shown) = (false, false);
            for out in stage.process(&mut image, action, &payload).unwrap() {
                match out {
                    ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await.unwrap(),
                    ActiveStageOutput::PointerHidden => hidden = true,
                    ActiveStageOutput::PointerBitmap(p) if p.bitmap_data.iter().all(|&b| b == 0) => hidden = true,
                    ActiveStageOutput::PointerDefault | ActiveStageOutput::PointerBitmap(_) => shown = true,
                    _ => {}
                }
            }
            assert!(!shown, "client pointer shown without a guest cursor");
            if hidden {
                break;
            }
        }
    });
}

#[test]
fn keyboard_chords_preserve_modifier_order() {
    let mut fixture = Fixture::with_password(ServerConfig::default(), false);
    fixture.host.take().unwrap().stop();
    let wakeup = Arc::new(tokio::sync::Notify::new());
    let host_wakeup = Arc::clone(&wakeup);
    fixture.core.set_waker(move || host_wakeup.notify_one());
    runtime().block_on(async {
        let (mut stage, mut framed) = connect(fixture.addr, USER, PASS, None).await.unwrap();
        let sequences = [
            vec![(0x1d, false), (0x2e, false)],
            vec![(0x1d, false), (0x38, false), (0x53, true)],
            vec![(0x2a, false), (0x1e, false)],
            vec![(0x1d, true), (0x2e, false)],
            vec![(0x38, false), (0x0f, false)],
            vec![(0x5b, true), (0x13, false)],
        ];
        let mut image = DecodedImage::new(
            PixelFormat::RgbA32,
            u16::try_from(W).unwrap(),
            u16::try_from(H).unwrap(),
        );
        for sequence in sequences {
            let mut expected = Vec::new();
            let mut input = Vec::new();
            for down in [true, false] {
                let keys: Vec<_> = if down {
                    sequence.clone()
                } else {
                    sequence.iter().rev().copied().collect()
                };
                for (scancode, extended) in keys {
                    let mut flags = KeyboardFlags::empty();
                    flags.set(KeyboardFlags::EXTENDED, extended);
                    flags.set(KeyboardFlags::RELEASE, !down);
                    input.push(FastPathInputEvent::KeyboardEvent(flags, scancode));
                    expected.push(ClientEvent::Key {
                        scancode,
                        extended,
                        down,
                    });
                }
            }
            for output in stage.process_fastpath_input(&mut image, &input).unwrap() {
                if let ActiveStageOutput::ResponseFrame(frame) = output {
                    framed.write_all(&frame).await.unwrap();
                }
            }
            let actual = tokio::time::timeout(Duration::from_secs(10), async {
                let mut actual = Vec::new();
                loop {
                    fixture.core.drain_events(|event| {
                        if matches!(event, ClientEvent::Key { .. } | ClientEvent::Unicode { .. }) {
                            actual.push(event);
                        }
                    });
                    if actual.len() >= expected.len() {
                        break actual;
                    }
                    wakeup.notified().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(actual, expected);
        }
    });
}

/// Sends `keys` (flags built from the `extended`/`down` pair) through a fresh session and
/// returns the key events the host received.
fn key_events(config: ServerConfig, keys: &[(u8, bool, bool)]) -> Vec<(u8, bool, bool)> {
    let keys: Vec<(u8, KeyboardFlags, bool)> = keys
        .iter()
        .map(|&(scancode, extended, down)| {
            let mut flags = KeyboardFlags::empty();
            flags.set(KeyboardFlags::EXTENDED, extended);
            flags.set(KeyboardFlags::RELEASE, !down);
            (scancode, flags, down)
        })
        .collect();
    raw_key_events(config, &keys)
}

/// As [`key_events`], with the flags written out (the E1 prefix has no `extended` flag).
fn raw_key_events(config: ServerConfig, keys: &[(u8, KeyboardFlags, bool)]) -> Vec<(u8, bool, bool)> {
    let mut fixture = Fixture::with_password(config, false);
    fixture.host.take().unwrap().stop();
    let wakeup = Arc::new(tokio::sync::Notify::new());
    let host_wakeup = Arc::clone(&wakeup);
    fixture.core.set_waker(move || host_wakeup.notify_one());
    let count = keys.len();
    runtime().block_on(async {
        let (mut stage, mut framed) = connect(fixture.addr, USER, PASS, None).await.unwrap();
        let mut image = DecodedImage::new(
            PixelFormat::RgbA32,
            u16::try_from(W).unwrap(),
            u16::try_from(H).unwrap(),
        );
        let mut input = Vec::new();
        for &(scancode, flags, _) in keys {
            input.push(FastPathInputEvent::KeyboardEvent(flags, scancode));
        }
        for output in stage.process_fastpath_input(&mut image, &input).unwrap() {
            if let ActiveStageOutput::ResponseFrame(frame) = output {
                framed.write_all(&frame).await.unwrap();
            }
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut actual = Vec::new();
            loop {
                fixture.core.drain_events(|event| {
                    if let ClientEvent::Key {
                        scancode,
                        extended,
                        down,
                    } = event
                    {
                        actual.push((scancode, extended, down));
                    }
                });
                if actual.len() >= count {
                    break actual;
                }
                wakeup.notified().await;
            }
        })
        .await
        .unwrap()
    })
}

/// Mouse events a client sends on the `FreeRDP::Advanced::Input` DVC reach the
/// same input handler the fast path uses, so the host sees them as ordinary pointer events
/// FreeRDP may use this channel where mstsc uses the fast path.
#[test]
fn advanced_input_mouse_reaches_the_handler() {
    let mut fixture = Fixture::with_password(ServerConfig::default(), false);
    // `vds-test-host` drains the same event queue (`vds-test-host/src/lib.rs:178`), so it has
    // to go before anything can be observed here — and with it the frames that would otherwise
    // keep the read loop turning.
    fixture.host.take().unwrap().stop();
    let wakeup = Arc::new(tokio::sync::Notify::new());
    let host_wakeup = Arc::clone(&wakeup);
    fixture.core.set_waker(move || host_wakeup.notify_one());

    // One of each kind the handler distinguishes: a move, a button down, that button up, and
    // a wheel notch. The wheel carries its units in `y`, 120 units to the notch.
    let pdus = vec![
        ainput_mouse(AINPUT_MOVE, 100, 200),
        ainput_mouse(AINPUT_BUTTON1 | AINPUT_DOWN, 100, 200),
        ainput_mouse(AINPUT_BUTTON1, 100, 200),
        ainput_mouse(AINPUT_WHEEL, 0, 120),
    ];
    // Three position events (move, press, release), two button events and one wheel.
    const WANTED: usize = 6;
    let mut events = Vec::new();
    runtime().block_on(async {
        let (mut stage, mut framed) = connect_full(fixture.addr, USER, PASS, None, None, Some(pdus))
            .await
            .unwrap();
        let mut image = DecodedImage::new(
            PixelFormat::RgbA32,
            u16::try_from(W).unwrap(),
            u16::try_from(H).unwrap(),
        );
        let collect = async {
            loop {
                fixture.core.drain_events(|event| events.push(event));
                let seen = events
                    .iter()
                    .filter(|e| {
                        matches!(
                            e,
                            ClientEvent::PointerAbs { .. } | ClientEvent::Button { .. } | ClientEvent::Wheel { .. }
                        )
                    })
                    .count();
                if seen >= WANTED {
                    return;
                }
                // The session has to keep running for the client to answer the server's DVC
                // PDUs, but the handshake is over long before the mouse events land, so wait
                // on the host's own wakeup as well as on the wire. `Framed::read_pdu` is
                // cancel safe: bytes already read stay in its buffer.
                tokio::select! {
                    pdu = framed.read_pdu() => {
                        let (action, payload) = pdu.unwrap();
                        for output in stage.process(&mut image, action, &payload).unwrap() {
                            if let ActiveStageOutput::ResponseFrame(frame) = output {
                                framed.write_all(&frame).await.unwrap();
                            }
                        }
                    }
                    () = wakeup.notified() => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), collect)
            .await
            .expect("the advanced-input mouse events never reached the host");
    });

    let at_cursor = ClientEvent::PointerAbs {
        head: 0,
        x: 100,
        y: 200,
        width: W,
        height: H,
    };
    assert_eq!(
        events.iter().filter(|e| **e == at_cursor).count(),
        3,
        "every advanced-input event carries its position, buttons included: {events:?}"
    );
    assert!(
        events.contains(&ClientEvent::Button {
            button: MouseButton::Left,
            down: true
        }) && events.contains(&ClientEvent::Button {
            button: MouseButton::Left,
            down: false
        }),
        "both halves of the click arrived: {events:?}"
    );
    assert!(
        events.contains(&ClientEvent::Wheel { dx: 0, dy: 1 }),
        "120 wheel units are one notch: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ClientEvent::Key { .. } | ClientEvent::Unicode { .. })),
        "a mouse PDU must not produce key events: {events:?}"
    );
}

/// No client can send Ctrl+Alt+Del itself — its own operating system eats it — so it
/// sends Ctrl+Alt+End and expects the server to inject the secure attention sequence. That
/// is the default here; `sas-hotkey=off` injects the keys as sent.
#[test]
fn ctrl_alt_end_becomes_the_secure_attention_sequence() {
    const CTRL: (u8, bool, bool) = (0x1d, false, true);
    const ALT: (u8, bool, bool) = (0x38, false, true);
    const CTRL_UP: (u8, bool, bool) = (0x1d, false, false);
    const ALT_UP: (u8, bool, bool) = (0x38, false, false);
    const END: (u8, bool, bool) = (0x4f, true, true);
    const END_UP: (u8, bool, bool) = (0x4f, true, false);
    const DELETE: (u8, bool, bool) = (0x53, true, true);
    const DELETE_UP: (u8, bool, bool) = (0x53, true, false);

    let chord = [CTRL, ALT, END, END_UP, ALT_UP, CTRL_UP];
    assert_eq!(
        key_events(ServerConfig::default(), &chord),
        vec![CTRL, ALT, DELETE, DELETE_UP, ALT_UP, CTRL_UP]
    );
    // Ctrl+Alt+Del itself is passed through unchanged, whatever the setting.
    assert_eq!(
        key_events(
            ServerConfig::default(),
            &[CTRL, ALT, DELETE, DELETE_UP, ALT_UP, CTRL_UP]
        ),
        vec![CTRL, ALT, DELETE, DELETE_UP, ALT_UP, CTRL_UP]
    );
    // End without both modifiers is just End: Shift+End selects to the end of the line.
    assert_eq!(key_events(ServerConfig::default(), &[END, END_UP]), vec![END, END_UP]);
    assert_eq!(
        key_events(
            ServerConfig {
                sas_hotkey: SasHotkey::Off,
                ..ServerConfig::default()
            },
            &chord
        ),
        vec![CTRL, ALT, END, END_UP, ALT_UP, CTRL_UP]
    );
}

/// The scancodes of the keys that are easy to get wrong — the E0-prefixed ones, where
/// the same number is a different key without the prefix.
#[test]
fn prefixed_keys_arrive_with_their_e0_flag() {
    // PrintScreen is two E0-prefixed events (E0 2A is the size-override, E0 37 the key).
    // Numpad Enter (E0 1C) and numpad / (E0 35) are not Enter and not slash on the main
    // block; the right Alt (E0 38) is AltGr. IronRDP gives us one `extended` flag per event,
    // so each must keep it.
    let keys = [
        (0x2a, true, true),
        (0x37, true, true),
        (0x37, true, false),
        (0x2a, true, false),
        (0x1c, true, true),
        (0x1c, true, false),
        (0x35, true, true),
        (0x35, true, false),
        (0x38, true, true),
        (0x38, true, false),
        (0x5b, true, true),
        (0x5b, true, false),
    ];
    assert_eq!(key_events(ServerConfig::default(), &keys), keys);

    // Pause is the exception: clients send it as EXTENDED1 + 0x45, and `ironrdp-server`
    // copies only the E0 flag into `KeyboardEvent`, so the E1 prefix is lost on the way in
    // and the guest gets NumLock (`README.md`, "Client requirements and known limits"). Asserting the loss keeps it visible: when the IronRDP
    // patch lands, this test fails and the expectation becomes the Pause keycode.
    let mut down = KeyboardFlags::empty();
    down.set(KeyboardFlags::EXTENDED1, true);
    let mut up = down;
    up.set(KeyboardFlags::RELEASE, true);
    assert_eq!(
        raw_key_events(ServerConfig::default(), &[(0x45, down, true), (0x45, up, false)]),
        vec![(0x45, false, true), (0x45, false, false)]
    );
}

#[test]
fn wrong_password_is_rejected() {
    let f = Fixture::with_password(ServerConfig::default(), false);
    runtime().block_on(async {
        assert!(connect(f.addr, USER, "wrong", None).await.is_err());
        assert!(connect(f.addr, "intruder", PASS, None).await.is_err());
    });
    assert_eq!(f.host().stats().initialized, 0);
}

#[test]
fn one_time_password_works_once() {
    let config = ServerConfig {
        egfx_grace_ms: 200,
        ..ServerConfig::default()
    };
    let f = Fixture::with_password(config, true);
    runtime().block_on(async {
        let (mut stage, mut framed) = connect(f.addr, USER, PASS, None).await.unwrap();
        pump(&mut stage, &mut framed, Duration::from_secs(20), |n| n >= 1)
            .await
            .unwrap();
        drop(framed);
        assert!(
            connect(f.addr, USER, PASS, None).await.is_err(),
            "one-time password reused"
        );
    });
}

#[test]
fn expired_password_is_refused() {
    let f = Fixture::with_password(ServerConfig::default(), false);
    f.core.credentials().set_expiry(Some(std::time::SystemTime::now()));
    runtime().block_on(async {
        assert!(connect(f.addr, USER, PASS, None).await.is_err());
    });
}

#[test]
fn avc420_only_server_disconnects_legacy_client() {
    let config = ServerConfig {
        codecs: CodecSet::parse("avc420").unwrap(),
        egfx_grace_ms: 200,
        ..ServerConfig::default()
    };
    let f = Fixture::with_password(config, false);
    runtime().block_on(async {
        let (mut stage, mut framed) = connect(f.addr, USER, PASS, None).await.unwrap();
        let r = pump(&mut stage, &mut framed, Duration::from_secs(10), |_| false).await;
        let err = r.unwrap_err().to_string();
        assert!(
            !err.contains("timeout"),
            "server should close the connection, got {err}"
        );
    });
}

#[test]
fn new_client_replaces_previous_one() {
    let config = ServerConfig {
        egfx_grace_ms: 200,
        ..ServerConfig::default()
    };
    let f = Fixture::with_password(config, false);
    runtime().block_on(async {
        let (mut s1, mut f1) = connect(f.addr, USER, PASS, None).await.unwrap();
        pump(&mut s1, &mut f1, Duration::from_secs(20), |n| n >= 1)
            .await
            .unwrap();
        let (mut s2, mut f2) = connect(f.addr, USER, PASS, None).await.unwrap();
        pump(&mut s2, &mut f2, Duration::from_secs(20), |n| n >= 1)
            .await
            .unwrap();
        let r = pump(&mut s1, &mut f1, Duration::from_secs(10), |_| false).await;
        assert!(
            !r.unwrap_err().to_string().contains("timeout"),
            "first client should be disconnected"
        );
    });
}

/// Connects to an external server, e.g. QEMU started with `-rdp`:
/// `VDS_E2E_ADDR=127.0.0.1:13389 VDS_E2E_USER=u VDS_E2E_PASS=p cargo test -p vds-rdp --test e2e -- --ignored`
#[test]
#[ignore = "needs an external RDP server (VDS_E2E_ADDR/USER/PASS)"]
fn external_server_sends_avc420() {
    let addr: SocketAddr = std::env::var("VDS_E2E_ADDR").unwrap().parse().unwrap();
    let user = std::env::var("VDS_E2E_USER").unwrap();
    let pass = std::env::var("VDS_E2E_PASS").unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    runtime().block_on(async {
        let (mut stage, mut framed) = connect(addr, &user, &pass, Some(Arc::clone(&seen))).await.unwrap();
        let s = Arc::clone(&seen);
        pump(&mut stage, &mut framed, Duration::from_secs(30), |_| {
            !s.lock().frames.is_empty()
        })
        .await
        .unwrap();
    });
    let seen = seen.lock();
    assert_eq!(&seen.frames[0][..4], &[0, 0, 0, 1]);
    eprintln!(
        "received {} AVC420 frame(s), first {} bytes",
        seen.frames.len(),
        seen.frames[0].len()
    );
}

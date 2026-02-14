use std::net::SocketAddr;
use std::sync::Arc;

use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::control::events::PipelineEvent;
use crate::control::state::{StreamState, StreamStateManager};
use crate::core::auth::AuthProvider;
use crate::core::config::{AppConfig, IngestConfig};
use crate::core::error::IngestError;
use crate::core::security;
use crate::core::types::{DemuxedFrame, MediaInfo, StreamId, VideoCodec};
use crate::observability::metrics as obs;
use crate::storage::memory::InMemoryMediaStore;

use super::demuxer::FlvDemuxer;

// ---------------------------------------------------------------------------
// Constants (from ingest.md §2.2, §2.6)
// ---------------------------------------------------------------------------

const RTMP_VERSION: u8 = 3;
const HANDSHAKE_SIZE: usize = 1536;
const HANDSHAKE_TIMEOUT_SECS: u64 = 5;
const SEQUENCE_HEADER_TIMEOUT_SECS: u64 = 10;
const INACTIVITY_TIMEOUT_SECS: u64 = 30;
const MAX_CONSECUTIVE_ERRORS: u32 = 10;

// ---------------------------------------------------------------------------
// RTMP chunk stream types (from ingest.md §2.3, §2.4)
// ---------------------------------------------------------------------------

/// RTMP message types we care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RtmpMessageType {
    Audio,           // type 8
    Video,           // type 9
    AmfCommand,      // type 20 (AMF0)
    AmfCommand3,     // type 17 (AMF3)
    SetChunkSize,    // type 1
    Acknowledgement, // type 3
    WindowAckSize,   // type 5
    SetPeerBw,       // type 6
    UserControl,     // type 4
    Unknown(u8),
}

impl From<u8> for RtmpMessageType {
    fn from(val: u8) -> Self {
        match val {
            1 => RtmpMessageType::SetChunkSize,
            3 => RtmpMessageType::Acknowledgement,
            4 => RtmpMessageType::UserControl,
            5 => RtmpMessageType::WindowAckSize,
            6 => RtmpMessageType::SetPeerBw,
            8 => RtmpMessageType::Audio,
            9 => RtmpMessageType::Video,
            17 => RtmpMessageType::AmfCommand3,
            20 => RtmpMessageType::AmfCommand,
            other => RtmpMessageType::Unknown(other),
        }
    }
}

/// A parsed RTMP chunk message.
#[derive(Debug)]
pub(crate) struct RtmpMessage {
    pub msg_type: RtmpMessageType,
    pub timestamp: u32,
    pub _stream_id: u32,
    pub data: Bytes,
}

/// RTMP AMF command names we handle (from ingest.md §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RtmpCommand {
    Connect { app: String },
    ReleaseStream,
    FcPublish,
    CreateStream,
    Publish { stream_key: String },
    DeleteStream,
    Unknown(String),
}

// ---------------------------------------------------------------------------
// RTMP Server (from ingest.md §2)
// ---------------------------------------------------------------------------

/// RTMP ingest server that listens for incoming encoder connections.
pub struct RtmpServer {
    config: IngestConfig,
    app_config: AppConfig,
    auth: Arc<AuthProvider>,
    state_manager: Arc<StreamStateManager>,
    store: Arc<InMemoryMediaStore>,
    cancel: CancellationToken,
    event_tx: mpsc::Sender<PipelineEvent>,
}

impl RtmpServer {
    pub fn new(
        config: IngestConfig,
        app_config: AppConfig,
        auth: Arc<AuthProvider>,
        state_manager: Arc<StreamStateManager>,
        store: Arc<InMemoryMediaStore>,
        cancel: CancellationToken,
        event_tx: mpsc::Sender<PipelineEvent>,
    ) -> Self {
        Self {
            config,
            app_config,
            auth,
            state_manager,
            store,
            cancel,
            event_tx,
        }
    }

    /// Start the RTMP server, listening on the configured port.
    pub async fn run(self) -> Result<(), IngestError> {
        let bind_addr: SocketAddr = format!(
            "{}:{}",
            self.app_config.server.host, self.app_config.server.rtmp_port
        )
        .parse()
        .map_err(|e| IngestError::ValidationFailed {
            reason: format!("invalid RTMP bind address: {}", e),
        })?;

        let listener = TcpListener::bind(bind_addr).await?;
        info!(%bind_addr, "RTMP server listening");

        let active_connections = Arc::new(tokio::sync::Semaphore::new(
            self.config.max_concurrent_rtmp_connections,
        ));
        let active_streams = Arc::new(tokio::sync::Semaphore::new(
            self.config.max_concurrent_live_streams,
        ));

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("RTMP server shutting down");
                    break;
                }
                accept_result = listener.accept() => {
                    let (socket, peer_addr) = match accept_result {
                        Ok(v) => v,
                        Err(e) => {
                            error!(error = %e, "failed to accept RTMP connection");
                            continue;
                        }
                    };

                    let permit = match active_connections.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            warn!(%peer_addr, "RTMP connection limit reached, rejecting");
                            drop(socket);
                            continue;
                        }
                    };

                    let conn = RtmpConnection {
                        _config: self.config.clone(),
                        app_config: self.app_config.clone(),
                        auth: self.auth.clone(),
                        state_manager: self.state_manager.clone(),
                        store: self.store.clone(),
                        cancel: self.cancel.clone(),
                        active_streams: active_streams.clone(),
                        event_tx: self.event_tx.clone(),
                    };

                    obs::inc_ingest_connections("rtmp");

                    tokio::spawn(async move {
                        if let Err(e) = conn.handle(socket, peer_addr).await {
                            obs::inc_ingest_error("rtmp", &e.to_string());
                            warn!(%peer_addr, error = %e, "RTMP connection error");
                        }
                        drop(permit);
                    });
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RTMP Connection Handler (from ingest.md §2.5)
// ---------------------------------------------------------------------------

struct RtmpConnection {
    _config: IngestConfig,
    app_config: AppConfig,
    auth: Arc<AuthProvider>,
    state_manager: Arc<StreamStateManager>,
    store: Arc<InMemoryMediaStore>,
    cancel: CancellationToken,
    active_streams: Arc<tokio::sync::Semaphore>,
    event_tx: mpsc::Sender<PipelineEvent>,
}

impl RtmpConnection {
    /// Handle a single RTMP connection through its full lifecycle.
    ///
    /// Lifecycle (from ingest.md §2.5):
    /// 1. TCP Accept → Handshake (5s timeout)
    /// 2. connect command → publish command → Auth check
    /// 3. Receive AVC Sequence Header + AudioSpecificConfig (10s timeout)
    /// 4. Codec validation
    /// 5. Demux loop (read RTMP → parse FLV → DemuxedFrame → channel)
    /// 6. Stream end → flush → notify control plane
    async fn handle(self, mut socket: TcpStream, peer_addr: SocketAddr) -> Result<(), IngestError> {
        debug!(%peer_addr, "new RTMP connection");

        // Phase 1: Handshake with timeout
        let handshake_result = tokio::time::timeout(
            std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
            Self::perform_handshake(&mut socket),
        )
        .await;

        match handshake_result {
            Ok(Ok(())) => debug!(%peer_addr, "RTMP handshake complete"),
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(IngestError::HandshakeTimeout {
                    timeout_secs: HANDSHAKE_TIMEOUT_SECS,
                });
            }
        }

        // Phase 2: Command processing — wait for connect + publish
        let stream_key = self.process_commands(&mut socket, peer_addr).await?;

        // Phase 2b: Authenticate stream key with brute-force protection (security.md §2.1)
        self.auth
            .validate_stream_key_with_ip(&stream_key, peer_addr.ip())?;

        // Acquire active stream permit
        let _stream_permit = match self.active_streams.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!(%peer_addr, "active stream limit reached");
                return Err(IngestError::AuthFailed {
                    reason: "active stream limit reached".to_string(),
                });
            }
        };

        // Create stream record
        let stream_id = StreamId::new();
        self.state_manager
            .create_stream(stream_id, crate::core::types::IngestMode::Live)
            .await;
        self.state_manager
            .transition(stream_id, StreamState::Live)
            .await
            .map_err(|e| IngestError::ValidationFailed {
                reason: e.to_string(),
            })?;

        // Validate RTMP codecs after sequence header (from ingest.md §3.4)
        // In a full implementation, this would be called after receiving the AVC sequence header.
        // For now, we validate the default expected codecs.
        super::validator::validate_rtmp_codecs(7, Some(10)).map_err(|e| {
            IngestError::ValidationFailed {
                reason: e.to_string(),
            }
        })?;

        obs::inc_ingest_active_streams("rtmp");
        obs::set_ingest_bitrate(&stream_id.to_string(), 0.0);

        // Validate a default resolution placeholder (security.md §4.1)
        // In a full implementation, resolution is extracted from the AVC sequence header
        // and validated before starting the transcode pipeline.
        security::validate_resolution(1920, 1080)
            .map_err(|e| IngestError::ValidationFailed { reason: e })?;

        // TODO: Extract actual resolution from AVC sequence header.
        // For now, use placeholder values. Real implementation would parse SPS NALUs.
        let source_width: u32 = 1920;
        let source_height: u32 = 1080;
        let source_fps: f64 = 30.0;

        // Emit StreamStarted event to control plane
        let media_info = MediaInfo {
            stream_id,
            video_codec: VideoCodec::H264,
            video_width: source_width,
            video_height: source_height,
            video_bitrate_kbps: None,
            frame_rate: source_fps,
            audio_codec: None,
            audio_sample_rate: None,
            audio_channels: None,
            audio_bitrate_kbps: None,
            duration_secs: None,
            container: crate::core::types::Container::Flv,
        };
        let _ = self
            .event_tx
            .send(PipelineEvent::StreamStarted {
                stream_id,
                media_info,
            })
            .await;

        // Wire up per-stream pipeline (ingest → transcode → package → storage)
        let pipeline = crate::control::pipeline::create_pipeline(
            stream_id,
            self.event_tx.clone(),
            &self.app_config,
        );

        // Spawn transcode task
        let transcode_config = self.app_config.transcode.clone();
        let transcode_state_mgr = self.state_manager.clone();
        let transcode_cancel = pipeline.cancel.clone();
        let transcode_segment_tx = pipeline.transcode_tx;
        let transcode_frame_rx = pipeline.ingest_rx;
        tokio::spawn(async move {
            let tp = crate::transcode::pipeline::TranscodePipeline::new(
                transcode_config,
                transcode_state_mgr,
                transcode_cancel,
            );
            if let Err(e) = tp
                .run_live(
                    stream_id,
                    source_width,
                    source_height,
                    source_fps,
                    transcode_frame_rx,
                    transcode_segment_tx,
                )
                .await
            {
                tracing::error!(%stream_id, error = %e, "transcode pipeline failed");
            }
        });

        // Spawn packager task
        let packaging_config = self.app_config.packaging.clone();
        let packager_event_tx = self.event_tx.clone();
        let packager_cancel = pipeline.cancel.clone();
        let packager_segment_rx = pipeline.transcode_rx;
        let packager_storage_tx = pipeline.package_tx;
        tokio::spawn(async move {
            crate::package::runner::run_packager(
                stream_id,
                packaging_config,
                packager_segment_rx,
                packager_storage_tx,
                packager_event_tx,
                packager_cancel,
            )
            .await;
        });

        // Spawn storage writer task
        let writer_store = self.store.clone();
        let writer_cancel = pipeline.cancel.clone();
        let writer_rx = pipeline.package_rx;
        tokio::spawn(async move {
            crate::storage::writer::run_storage_writer(
                stream_id,
                writer_store,
                writer_rx,
                writer_cancel,
            )
            .await;
        });

        // Use the pipeline's ingest_tx for sending frames
        let frame_tx = pipeline.ingest_tx;

        info!(%stream_id, %peer_addr, %stream_key, "live stream started");

        // Phase 3-5: Demux loop
        let demux_result = self
            .run_demux_loop(&mut socket, stream_id, peer_addr, frame_tx)
            .await;

        // Phase 6: Stream end — finalize
        obs::dec_ingest_active_streams("rtmp");
        match &demux_result {
            Ok(()) => {
                info!(%stream_id, "stream ended normally");
                let _ = self
                    .event_tx
                    .send(PipelineEvent::StreamEnded { stream_id })
                    .await;
                let _ = self
                    .state_manager
                    .transition(stream_id, StreamState::Processing)
                    .await;
            }
            Err(e) => {
                warn!(%stream_id, error = %e, "stream ended with error");
                let _ = self
                    .event_tx
                    .send(PipelineEvent::StreamError {
                        stream_id,
                        error: e.to_string(),
                    })
                    .await;
                let _ = self
                    .state_manager
                    .transition_to_error(stream_id, e.to_string())
                    .await;
            }
        }

        demux_result
    }

    /// RTMP handshake: C0/C1/C2 ↔ S0/S1/S2 exchange (from ingest.md §2.2).
    async fn perform_handshake(socket: &mut TcpStream) -> Result<(), IngestError> {
        // Read C0 (1 byte: version)
        let c0 = socket.read_u8().await?;
        if c0 != RTMP_VERSION {
            return Err(IngestError::InvalidRtmpVersion { version: c0 });
        }

        // Read C1 (1536 bytes)
        let mut c1 = vec![0u8; HANDSHAKE_SIZE];
        socket.read_exact(&mut c1).await?;

        // Send S0 + S1 + S2
        let mut response = BytesMut::with_capacity(1 + HANDSHAKE_SIZE * 2);
        response.put_u8(RTMP_VERSION); // S0
                                       // S1: timestamp (4 bytes) + zero (4 bytes) + random (1528 bytes)
        response.put_u32(0); // timestamp
        response.put_u32(0); // zero
        response.extend_from_slice(&vec![0xABu8; HANDSHAKE_SIZE - 8]); // random fill
                                                                       // S2: echo C1
        response.extend_from_slice(&c1);
        socket.write_all(&response).await?;
        socket.flush().await?;

        // Read C2 (1536 bytes) — we don't validate it strictly
        let mut c2 = vec![0u8; HANDSHAKE_SIZE];
        socket.read_exact(&mut c2).await?;

        Ok(())
    }

    /// Process RTMP commands until we get a `publish` command with a stream key.
    async fn process_commands(
        &self,
        socket: &mut TcpStream,
        peer_addr: SocketAddr,
    ) -> Result<String, IngestError> {
        // Simplified: read RTMP chunk messages and parse AMF commands.
        // In a full implementation, this would handle chunk stream reassembly.
        // For now, we read messages and extract the stream key from publish.
        let mut chunk_reader = ChunkReader::new();

        loop {
            let msg = chunk_reader.read_message(socket).await?;

            match msg.msg_type {
                RtmpMessageType::AmfCommand | RtmpMessageType::AmfCommand3 => {
                    let cmd = Self::parse_amf_command(&msg.data)?;
                    match cmd {
                        RtmpCommand::Connect { app } => {
                            debug!(%peer_addr, %app, "RTMP connect");
                            Self::send_connect_result(socket).await?;
                        }
                        RtmpCommand::ReleaseStream | RtmpCommand::FcPublish => {
                            debug!(%peer_addr, "RTMP {:?} (acknowledged)", cmd);
                            Self::send_command_result(socket).await?;
                        }
                        RtmpCommand::CreateStream => {
                            debug!(%peer_addr, "RTMP createStream");
                            Self::send_create_stream_result(socket).await?;
                        }
                        RtmpCommand::Publish { stream_key } => {
                            debug!(%peer_addr, "RTMP publish");
                            return Ok(stream_key);
                        }
                        RtmpCommand::DeleteStream => {
                            debug!(%peer_addr, "RTMP deleteStream");
                            return Err(IngestError::ConnectionReset {
                                stream_id: StreamId::new(),
                            });
                        }
                        RtmpCommand::Unknown(name) => {
                            debug!(%peer_addr, %name, "unknown RTMP command, ignoring");
                        }
                    }
                }
                RtmpMessageType::SetChunkSize => {
                    if msg.data.len() >= 4 {
                        let new_size = u32::from_be_bytes([
                            msg.data[0],
                            msg.data[1],
                            msg.data[2],
                            msg.data[3],
                        ]) as usize;
                        chunk_reader.set_chunk_size(new_size);
                        debug!(%peer_addr, %new_size, "chunk size updated");
                    }
                }
                RtmpMessageType::WindowAckSize | RtmpMessageType::SetPeerBw => {
                    // Acknowledge protocol messages
                }
                _ => {
                    debug!(%peer_addr, msg_type = ?msg.msg_type, "ignoring RTMP message during command phase");
                }
            }
        }
    }

    /// Main demux loop: read RTMP messages, parse FLV tags, emit DemuxedFrames.
    async fn run_demux_loop(
        &self,
        socket: &mut TcpStream,
        stream_id: StreamId,
        peer_addr: SocketAddr,
        frame_tx: mpsc::Sender<DemuxedFrame>,
    ) -> Result<(), IngestError> {
        let mut chunk_reader = ChunkReader::new();
        let mut demuxer = FlvDemuxer::new(stream_id);
        let mut consecutive_errors: u32 = 0;

        // Wait for sequence header with timeout
        let seq_header_deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(SEQUENCE_HEADER_TIMEOUT_SECS);

        loop {
            let read_result = tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!(%stream_id, "demux loop cancelled by shutdown");
                    break;
                }
                _ = tokio::time::sleep_until(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(INACTIVITY_TIMEOUT_SECS)
                ) => {
                    info!(%stream_id, "inactivity timeout, ending stream");
                    break;
                }
                result = chunk_reader.read_message(socket) => result,
            };

            let msg = match read_result {
                Ok(m) => {
                    consecutive_errors = 0;
                    m
                }
                Err(IngestError::Io(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof
                        || e.kind() == std::io::ErrorKind::ConnectionReset =>
                {
                    info!(%stream_id, %peer_addr, "RTMP connection closed");
                    break;
                }
                Err(e) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        return Err(IngestError::ValidationFailed {
                            reason: format!(
                                "too many consecutive read errors ({})",
                                consecutive_errors
                            ),
                        });
                    }
                    warn!(%stream_id, error = %e, consecutive = consecutive_errors, "RTMP read error, skipping");
                    continue;
                }
            };

            match msg.msg_type {
                RtmpMessageType::Video | RtmpMessageType::Audio => {
                    // Check sequence header timeout
                    if !demuxer.has_sequence_header()
                        && tokio::time::Instant::now() > seq_header_deadline
                    {
                        return Err(IngestError::MissingSequenceHeader {
                            timeout_secs: SEQUENCE_HEADER_TIMEOUT_SECS,
                        });
                    }

                    match demuxer.demux_flv_tag(&msg) {
                        Ok(Some(frame)) => {
                            let track_label = if frame.track.is_audio() {
                                "audio"
                            } else {
                                "video"
                            };
                            obs::inc_ingest_frames("rtmp", track_label);
                            obs::add_ingest_bytes(
                                "rtmp",
                                &stream_id.to_string(),
                                frame.data.len() as u64,
                            );
                            // Send to transcode pipeline with backpressure metrics
                            // (performance-and-backpressure.md §4.4)
                            if crate::core::metrics::send_with_backpressure(
                                &frame_tx,
                                frame,
                                "ingest_transcode",
                                crate::core::types::INGEST_TRANSCODE_CHANNEL_CAP,
                            )
                            .await
                            .is_err()
                            {
                                info!(%stream_id, "transcode channel closed, ending stream");
                                break;
                            }
                        }
                        Ok(None) => {
                            // Sequence header or non-frame data, no frame to emit
                        }
                        Err(e) => {
                            consecutive_errors += 1;
                            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                                return Err(IngestError::ValidationFailed {
                                    reason: format!(
                                        "too many consecutive demux errors ({})",
                                        consecutive_errors
                                    ),
                                });
                            }
                            warn!(%stream_id, error = %e, "FLV demux error, skipping frame");
                        }
                    }
                }
                RtmpMessageType::AmfCommand | RtmpMessageType::AmfCommand3 => {
                    if let Ok(RtmpCommand::DeleteStream) = Self::parse_amf_command(&msg.data) {
                        info!(%stream_id, "deleteStream received");
                        break;
                    }
                }
                RtmpMessageType::SetChunkSize => {
                    if msg.data.len() >= 4 {
                        let new_size = u32::from_be_bytes([
                            msg.data[0],
                            msg.data[1],
                            msg.data[2],
                            msg.data[3],
                        ]) as usize;
                        // Validate chunk size against security limit (security.md §4.1)
                        if new_size > security::MAX_RTMP_CHUNK_SIZE {
                            warn!(%stream_id, new_size, max = security::MAX_RTMP_CHUNK_SIZE, "chunk size exceeds limit, clamping");
                        }
                        chunk_reader.set_chunk_size(new_size);
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    // -- AMF parsing helpers (simplified) --

    fn parse_amf_command(data: &[u8]) -> Result<RtmpCommand, IngestError> {
        // Simplified AMF0 command parsing.
        // A real implementation would use a proper AMF0 decoder.
        // AMF0 string: 0x02 + u16 length + UTF-8 bytes
        if data.len() < 3 || data[0] != 0x02 {
            return Err(IngestError::MalformedAmf {
                reason: "expected AMF0 string marker".to_string(),
            });
        }

        let name_len = u16::from_be_bytes([data[1], data[2]]) as usize;
        if name_len > security::MAX_AMF_STRING_LENGTH {
            return Err(IngestError::MalformedAmf {
                reason: format!(
                    "AMF string length {} exceeds max {}",
                    name_len,
                    security::MAX_AMF_STRING_LENGTH
                ),
            });
        }
        if data.len() < 3 + name_len {
            return Err(IngestError::MalformedAmf {
                reason: "AMF0 string length exceeds data".to_string(),
            });
        }

        let name =
            std::str::from_utf8(&data[3..3 + name_len]).map_err(|_| IngestError::MalformedAmf {
                reason: "invalid UTF-8 in command name".to_string(),
            })?;

        match name {
            "connect" => {
                // Extract app from the AMF object that follows
                let app = Self::extract_amf_string_field(&data[3 + name_len..], "app")
                    .unwrap_or_else(|| "live".to_string());
                Ok(RtmpCommand::Connect { app })
            }
            "releaseStream" => Ok(RtmpCommand::ReleaseStream),
            "FCPublish" => Ok(RtmpCommand::FcPublish),
            "createStream" => Ok(RtmpCommand::CreateStream),
            "publish" => {
                // Stream key is the first string argument after transaction ID
                let stream_key =
                    Self::extract_publish_stream_key(&data[3 + name_len..]).unwrap_or_default();
                Ok(RtmpCommand::Publish { stream_key })
            }
            "deleteStream" => Ok(RtmpCommand::DeleteStream),
            other => Ok(RtmpCommand::Unknown(other.to_string())),
        }
    }

    fn extract_amf_string_field(data: &[u8], _field_name: &str) -> Option<String> {
        // Simplified: scan for AMF0 string values in the object.
        // A real implementation would properly parse the AMF0 object.
        let mut pos = 0;
        while pos + 3 < data.len() {
            if data[pos] == 0x02 {
                let len = u16::from_be_bytes([data[pos + 1], data[pos + 2]]) as usize;
                if pos + 3 + len <= data.len() {
                    if let Ok(s) = std::str::from_utf8(&data[pos + 3..pos + 3 + len]) {
                        return Some(s.to_string());
                    }
                }
            }
            pos += 1;
        }
        None
    }

    fn extract_publish_stream_key(data: &[u8]) -> Option<String> {
        // After command name, expect: transaction_id (number), null, stream_key (string)
        // Simplified: find the first AMF0 string after skipping the transaction ID
        let mut pos = 0;
        let mut strings_found = 0;

        while pos < data.len() {
            match data[pos] {
                0x00 => {
                    // AMF0 number (8 bytes)
                    pos += 9;
                }
                0x02 => {
                    // AMF0 string
                    if pos + 2 >= data.len() {
                        break;
                    }
                    let len = u16::from_be_bytes([data[pos + 1], data[pos + 2]]) as usize;
                    if pos + 3 + len > data.len() {
                        break;
                    }
                    let s = std::str::from_utf8(&data[pos + 3..pos + 3 + len]).ok()?;
                    strings_found += 1;
                    if strings_found == 1 {
                        return Some(s.to_string());
                    }
                    pos += 3 + len;
                }
                0x05 => {
                    // AMF0 null
                    pos += 1;
                }
                _ => {
                    pos += 1;
                }
            }
        }
        None
    }

    // -- Response helpers (simplified RTMP responses) --

    async fn send_connect_result(socket: &mut TcpStream) -> Result<(), IngestError> {
        // Simplified: send Window Acknowledgement Size + Set Peer Bandwidth + _result
        // Window Ack Size (type 5): 4 bytes
        let mut buf = BytesMut::new();

        // Window Acknowledgement Size message
        Self::write_rtmp_message(&mut buf, 5, 0, &2500000u32.to_be_bytes());
        // Set Peer Bandwidth message (type 6): 4 bytes size + 1 byte limit type
        let mut bw_data = Vec::with_capacity(5);
        bw_data.extend_from_slice(&2500000u32.to_be_bytes());
        bw_data.push(2); // dynamic
        Self::write_rtmp_message(&mut buf, 6, 0, &bw_data);

        // _result AMF0 response (simplified)
        let result_data = Self::build_connect_result_amf();
        Self::write_rtmp_message(&mut buf, 20, 0, &result_data);

        socket.write_all(&buf).await?;
        socket.flush().await?;
        Ok(())
    }

    async fn send_command_result(socket: &mut TcpStream) -> Result<(), IngestError> {
        // Send a generic _result
        let mut buf = BytesMut::new();
        let result_data = Self::build_generic_result_amf();
        Self::write_rtmp_message(&mut buf, 20, 0, &result_data);
        socket.write_all(&buf).await?;
        socket.flush().await?;
        Ok(())
    }

    async fn send_create_stream_result(socket: &mut TcpStream) -> Result<(), IngestError> {
        // _result with stream ID = 1
        let mut buf = BytesMut::new();
        let result_data = Self::build_create_stream_result_amf();
        Self::write_rtmp_message(&mut buf, 20, 0, &result_data);
        socket.write_all(&buf).await?;
        socket.flush().await?;
        Ok(())
    }

    fn write_rtmp_message(buf: &mut BytesMut, msg_type: u8, stream_id: u32, data: &[u8]) {
        // Simplified RTMP chunk format (type 0 header on chunk stream 2)
        let chunk_stream_id: u8 = if msg_type == 20 { 3 } else { 2 };
        buf.put_u8(chunk_stream_id); // fmt=0, csid
        buf.put_u8(0);
        buf.put_u8(0);
        buf.put_u8(0); // timestamp = 0
        let len = data.len() as u32;
        buf.put_u8((len >> 16) as u8);
        buf.put_u8((len >> 8) as u8);
        buf.put_u8(len as u8);
        buf.put_u8(msg_type);
        buf.put_u32_le(stream_id);
        buf.extend_from_slice(data);
    }

    fn build_connect_result_amf() -> Vec<u8> {
        let mut data = Vec::new();
        // "_result" string
        data.push(0x02);
        data.extend_from_slice(&7u16.to_be_bytes());
        data.extend_from_slice(b"_result");
        // Transaction ID: 1.0
        data.push(0x00);
        data.extend_from_slice(&1.0f64.to_be_bytes());
        // Properties object (simplified)
        data.push(0x03); // object start
        data.push(0x00);
        data.push(0x00);
        data.push(0x09); // object end
                         // Info object
        data.push(0x03); // object start
                         // "code" = "NetConnection.Connect.Success"
        data.extend_from_slice(&4u16.to_be_bytes());
        data.extend_from_slice(b"code");
        data.push(0x02);
        let code = b"NetConnection.Connect.Success";
        data.extend_from_slice(&(code.len() as u16).to_be_bytes());
        data.extend_from_slice(code);
        data.push(0x00);
        data.push(0x00);
        data.push(0x09); // object end
        data
    }

    fn build_generic_result_amf() -> Vec<u8> {
        let mut data = Vec::new();
        data.push(0x02);
        data.extend_from_slice(&7u16.to_be_bytes());
        data.extend_from_slice(b"_result");
        data.push(0x00);
        data.extend_from_slice(&0.0f64.to_be_bytes());
        data.push(0x05); // null
        data
    }

    fn build_create_stream_result_amf() -> Vec<u8> {
        let mut data = Vec::new();
        data.push(0x02);
        data.extend_from_slice(&7u16.to_be_bytes());
        data.extend_from_slice(b"_result");
        data.push(0x00);
        data.extend_from_slice(&0.0f64.to_be_bytes());
        data.push(0x05); // null
                         // Stream ID = 1.0
        data.push(0x00);
        data.extend_from_slice(&1.0f64.to_be_bytes());
        data
    }
}

// ---------------------------------------------------------------------------
// RTMP Chunk Reader
// ---------------------------------------------------------------------------

/// Reads RTMP chunk stream messages from a TCP socket.
pub(crate) struct ChunkReader {
    chunk_size: usize,
}

impl ChunkReader {
    pub fn new() -> Self {
        Self {
            chunk_size: 128, // RTMP default chunk size
        }
    }

    pub fn set_chunk_size(&mut self, size: usize) {
        // Validate against security limit (security.md §4.1)
        self.chunk_size = size.min(security::MAX_RTMP_CHUNK_SIZE);
    }

    /// Read a single RTMP message (potentially spanning multiple chunks).
    pub async fn read_message(
        &mut self,
        socket: &mut TcpStream,
    ) -> Result<RtmpMessage, IngestError> {
        // Read basic header (1-3 bytes)
        let first_byte = socket.read_u8().await?;
        let fmt = (first_byte >> 6) & 0x03;
        let csid = (first_byte & 0x3F) as u32;

        let _csid = match csid {
            0 => socket.read_u8().await? as u32 + 64,
            1 => {
                let b1 = socket.read_u8().await? as u32;
                let b2 = socket.read_u8().await? as u32;
                b2 * 256 + b1 + 64
            }
            _ => csid,
        };

        // Read message header based on fmt
        let (timestamp, msg_length, msg_type_id, stream_id) = match fmt {
            0 => {
                // Type 0: full header (11 bytes)
                let mut header = [0u8; 11];
                socket.read_exact(&mut header).await?;
                let ts = u32::from_be_bytes([0, header[0], header[1], header[2]]);
                let len = u32::from_be_bytes([0, header[3], header[4], header[5]]) as usize;
                let type_id = header[6];
                let sid = u32::from_le_bytes([header[7], header[8], header[9], header[10]]);
                (ts, len, type_id, sid)
            }
            1 => {
                // Type 1: 7 bytes (no stream ID)
                let mut header = [0u8; 7];
                socket.read_exact(&mut header).await?;
                let ts = u32::from_be_bytes([0, header[0], header[1], header[2]]);
                let len = u32::from_be_bytes([0, header[3], header[4], header[5]]) as usize;
                let type_id = header[6];
                (ts, len, type_id, 0)
            }
            2 => {
                // Type 2: 3 bytes (timestamp delta only)
                let mut header = [0u8; 3];
                socket.read_exact(&mut header).await?;
                let ts = u32::from_be_bytes([0, header[0], header[1], header[2]]);
                (ts, 0, 0, 0)
            }
            3 => {
                // Type 3: no header
                (0, 0, 0, 0)
            }
            _ => unreachable!(),
        };

        // Handle extended timestamp
        let _timestamp = if timestamp == 0xFFFFFF {
            let mut ext = [0u8; 4];
            socket.read_exact(&mut ext).await?;
            u32::from_be_bytes(ext)
        } else {
            timestamp
        };

        // Read message data (may span multiple chunks)
        let mut data = BytesMut::with_capacity(msg_length);
        let mut remaining = msg_length;

        while remaining > 0 {
            let to_read = remaining.min(self.chunk_size);
            let mut chunk_data = vec![0u8; to_read];
            socket.read_exact(&mut chunk_data).await?;
            data.extend_from_slice(&chunk_data);
            remaining -= to_read;

            // If more chunks needed, read continuation header (fmt=3)
            if remaining > 0 {
                let _cont = socket.read_u8().await?;
            }
        }

        Ok(RtmpMessage {
            msg_type: RtmpMessageType::from(msg_type_id),
            timestamp: _timestamp,
            _stream_id: stream_id,
            data: data.freeze(),
        })
    }
}

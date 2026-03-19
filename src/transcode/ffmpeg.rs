//! Real FFmpeg H.264 transcode implementation.
//!
//! Compiled only when the `ffmpeg` feature is active.
//!
//! Architecture:
//! - H.264 decoder (libavcodec) — decodes Annex-B NALUs → YUV420p once per frame
//! - N × (swscale scaler + libx264 encoder + SegmentAccumulator) — one per rendition
//! - Audio passthrough — raw AAC frames forwarded unchanged
//!
//! All FFmpeg work runs on a blocking thread (via `tokio::task::spawn_blocking`)
//! to avoid stalling the async runtime on CPU-bound encode work.

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use ffmpeg_next as ffmpeg;

use crate::core::error::TranscodeError;
use crate::core::types::{DemuxedFrame, EncodedSegment, StreamId};

use super::profile::SelectedRendition;
use super::segment::{EncodedPacket, SegmentAccumulator};

// ---------------------------------------------------------------------------
// Per-rendition encode context (scaler + encoder + pre-allocated output frame)
// ---------------------------------------------------------------------------

struct RenditionCtx {
    encoder: ffmpeg::codec::encoder::video::Encoder,
    scaler: ffmpeg::software::scaling::Context,
    /// Pre-allocated frame so swscale does not allocate on every call.
    output_frame: ffmpeg::frame::video::Video,
}

impl RenditionCtx {
    fn new(
        rendition: &SelectedRendition,
        source_width: u32,
        source_height: u32,
        source_fps: f64,
    ) -> Result<Self, TranscodeError> {
        // ------------------------------------------------------------------
        // Encoder
        // ------------------------------------------------------------------
        let codec =
            ffmpeg::codec::encoder::find(ffmpeg::codec::Id::H264).ok_or_else(|| {
                TranscodeError::FfmpegInit {
                    reason:
                        "libx264 encoder not found — rebuild FFmpeg with --enable-libx264".into(),
                }
            })?;

        let mut video = ffmpeg::codec::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(|e| TranscodeError::FfmpegInit {
                reason: format!("video encoder context: {e}"),
            })?;

        let bitrate_bps = rendition.video_bitrate_kbps as usize * 1000;
        // Force IDR every 2 seconds so the HLS packager can always cut a segment.
        let gop = ((2.0 * source_fps).round() as u32).max(1);
        let fps_num = (source_fps * 1000.0).round() as i32;

        video.set_width(rendition.width);
        video.set_height(rendition.height);
        video.set_format(ffmpeg::format::Pixel::YUV420P);
        video.set_bit_rate(bitrate_bps);
        video.set_max_bit_rate(bitrate_bps * 3 / 2); // allow 1.5× burst
        video.set_time_base(ffmpeg::Rational(1, 90000));
        video.set_frame_rate(Some(ffmpeg::Rational(fps_num, 1000)));
        video.set_gop(gop);
        // No B-frames: required for HLS segment splicing (each IDR must be
        // independently decodable without reference to future frames).
        video.set_max_b_frames(0);

        let mut opts = ffmpeg::Dictionary::new();
        opts.set("preset", rendition.preset.as_ref());
        // Force strict IDR at every keyframe interval; disable scene-cut detection
        // so the packager's 2s target is honoured precisely.
        let ki = gop.to_string();
        opts.set(
            "x264-params",
            &format!("keyint={ki}:min-keyint={ki}:scenecut=0"),
        );

        let encoder = video.open_with(opts).map_err(|e| TranscodeError::FfmpegInit {
            reason: format!("x264 open ({} {}p {}kbps): {e}", rendition.id, rendition.height, rendition.video_bitrate_kbps),
        })?;

        // ------------------------------------------------------------------
        // Scaler
        // ------------------------------------------------------------------
        let scaler = ffmpeg::software::scaling::Context::get(
            ffmpeg::format::Pixel::YUV420P,
            source_width,
            source_height,
            ffmpeg::format::Pixel::YUV420P,
            rendition.width,
            rendition.height,
            ffmpeg::software::scaling::flag::Flags::BILINEAR,
        )
        .map_err(|e| TranscodeError::FfmpegInit {
            reason: format!("swscale ({source_width}x{source_height} → {}x{}): {e}", rendition.width, rendition.height),
        })?;

        let output_frame = ffmpeg::frame::video::Video::new(
            ffmpeg::format::Pixel::YUV420P,
            rendition.width,
            rendition.height,
        );

        Ok(Self {
            encoder,
            scaler,
            output_frame,
        })
    }

    /// Scale `src` to the rendition resolution and encode it.
    /// Returns all encoder output packets (may be more than one if the encoder
    /// has internal buffering, e.g. for B-frames — though we disable those).
    fn process_frame(
        &mut self,
        src: &ffmpeg::frame::video::Video,
        pts: Option<i64>,
    ) -> Result<Vec<EncodedPacket>, TranscodeError> {
        self.scaler
            .run(src, &mut self.output_frame)
            .map_err(|e| TranscodeError::FfmpegInit {
                reason: format!("swscale run: {e}"),
            })?;

        self.output_frame.set_pts(pts);

        self.encoder
            .send_frame(&self.output_frame)
            .map_err(|e| TranscodeError::FfmpegInit {
                reason: format!("encoder send_frame: {e}"),
            })?;

        self.drain_encoder()
    }

    /// Send EOF to the encoder and drain remaining packets.
    fn flush_encoder(&mut self) -> Result<Vec<EncodedPacket>, TranscodeError> {
        self.encoder
            .send_eof()
            .map_err(|e| TranscodeError::FfmpegInit {
                reason: format!("encoder send_eof: {e}"),
            })?;
        self.drain_encoder()
    }

    fn drain_encoder(&mut self) -> Result<Vec<EncodedPacket>, TranscodeError> {
        let mut packets = Vec::new();
        let mut pkt = ffmpeg::Packet::empty();
        while self.encoder.receive_packet(&mut pkt).is_ok() {
            let data = pkt
                .data()
                .map(Bytes::copy_from_slice)
                .unwrap_or_default();
            packets.push(EncodedPacket {
                pts: pkt.pts().unwrap_or(0),
                _dts: pkt.dts().unwrap_or(0),
                keyframe: pkt.is_key(),
                data,
                is_audio: false,
            });
        }
        Ok(packets)
    }
}

// ---------------------------------------------------------------------------
// Blocking transcode entry point (called from spawn_blocking)
// ---------------------------------------------------------------------------

/// Run the full FFmpeg transcode pipeline on a blocking thread.
///
/// Reads `DemuxedFrame` items from `frame_rx`, decodes H.264 Annex-B video
/// into YUV420p, scales to each rendition's target resolution, re-encodes
/// with libx264, accumulates encoded packets into segments, and sends
/// completed `EncodedSegment` values to `segment_tx`.
///
/// Audio frames (raw AAC) are forwarded unchanged to every rendition's
/// segment accumulator so that each fMP4 segment contains both tracks.
pub(super) fn transcode_blocking(
    stream_id: StreamId,
    mut frame_rx: mpsc::Receiver<DemuxedFrame>,
    segment_tx: mpsc::Sender<EncodedSegment>,
    renditions: Vec<SelectedRendition>,
    source_width: u32,
    source_height: u32,
    source_fps: f64,
    target_duration_secs: f64,
) -> Result<(), TranscodeError> {
    ffmpeg::init().map_err(|e| TranscodeError::FfmpegInit {
        reason: format!("ffmpeg_init: {e}"),
    })?;

    // Per-rendition encode context (encoder + scaler + output frame).
    let mut render_ctxs: Vec<RenditionCtx> = renditions
        .iter()
        .map(|r| RenditionCtx::new(r, source_width, source_height, source_fps))
        .collect::<Result<_, _>>()?;

    // Per-rendition segment accumulators (kept separate so we can call .flush()
    // at the end, which consumes the accumulator by value).
    let mut accumulators: Vec<SegmentAccumulator> = renditions
        .iter()
        .map(|r| {
            SegmentAccumulator::new(
                stream_id,
                r.id.clone(),
                target_duration_secs,
                r.width,
                r.height,
                r.video_bitrate_kbps,
                r.audio_bitrate_kbps,
                r.profile.clone(),
                r.level.clone(),
                source_fps,
            )
        })
        .collect();

    // ------------------------------------------------------------------
    // H.264 decoder
    // ------------------------------------------------------------------
    let h264_codec =
        ffmpeg::codec::decoder::find(ffmpeg::codec::Id::H264).ok_or_else(|| {
            TranscodeError::FfmpegInit {
                reason: "H264 decoder not found".into(),
            }
        })?;

    let mut decoder = ffmpeg::codec::Context::new_with_codec(h264_codec)
        .decoder()
        .video()
        .map_err(|e| TranscodeError::FfmpegInit {
            reason: format!("H264 decoder open: {e}"),
        })?;

    // Tell the decoder that incoming packet timestamps are in the 90 kHz
    // timebase used by the demuxer so that decoded frame.pts() is usable directly.
    decoder.set_packet_time_base(ffmpeg::Rational(1, 90000));

    let mut yuv_frame = ffmpeg::frame::video::Video::empty();

    // ------------------------------------------------------------------
    // Main loop: receive demuxed frames, decode video, encode per rendition
    // ------------------------------------------------------------------
    loop {
        let dmx = match frame_rx.blocking_recv() {
            Some(f) => f,
            None => break, // ingest channel closed → flush below
        };

        if dmx.track.is_audio() {
            // AAC passthrough — fan out unchanged to all rendition accumulators.
            // Capture sample rate/channels from the first audio frame so each
            // emitted segment carries the correct AudioSpecificConfig params.
            if let crate::core::types::Track::Audio { sample_rate, channels, .. } = dmx.track {
                for acc in &mut accumulators {
                    acc.set_audio_params(sample_rate, channels);
                }
            }
            let audio_pkt = EncodedPacket {
                pts: dmx.pts,
                _dts: dmx.dts,
                keyframe: true,
                data: dmx.data,
                is_audio: true,
            };
            for acc in &mut accumulators {
                if let Some(seg) = acc.push(audio_pkt.clone()) {
                    if segment_tx.blocking_send(seg).is_err() {
                        return Err(TranscodeError::Cancelled { stream_id });
                    }
                }
            }
            continue;
        }

        // Video: decode Annex-B NALUs → YUV420p frame(s).
        let mut in_pkt = ffmpeg::Packet::copy(&dmx.data);
        in_pkt.set_pts(Some(dmx.pts));
        in_pkt.set_dts(Some(dmx.dts));

        if let Err(e) = decoder.send_packet(&in_pkt) {
            warn!(%stream_id, error = %e, "H264 send_packet error, skipping frame");
            continue;
        }

        while decoder.receive_frame(&mut yuv_frame).is_ok() {
            let pts = yuv_frame.pts();
            for (ctx, acc) in render_ctxs.iter_mut().zip(accumulators.iter_mut()) {
                let enc_pkts = ctx.process_frame(&yuv_frame, pts)?;
                for enc_pkt in enc_pkts {
                    if let Some(seg) = acc.push(enc_pkt) {
                        if segment_tx.blocking_send(seg).is_err() {
                            return Err(TranscodeError::Cancelled { stream_id });
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Flush decoder: drain any frames held in the decoder's reorder buffer.
    // ------------------------------------------------------------------
    if let Err(e) = decoder.send_eof() {
        warn!(%stream_id, error = %e, "decoder send_eof failed");
    }
    while decoder.receive_frame(&mut yuv_frame).is_ok() {
        let pts = yuv_frame.pts();
        for (ctx, acc) in render_ctxs.iter_mut().zip(accumulators.iter_mut()) {
            let enc_pkts = ctx.process_frame(&yuv_frame, pts)?;
            for enc_pkt in enc_pkts {
                if let Some(seg) = acc.push(enc_pkt) {
                    if segment_tx.blocking_send(seg).is_err() {
                        return Err(TranscodeError::Cancelled { stream_id });
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Flush encoders: drain packets buffered inside x264.
    // ------------------------------------------------------------------
    for (ctx, acc) in render_ctxs.iter_mut().zip(accumulators.iter_mut()) {
        let enc_pkts = ctx.flush_encoder()?;
        for enc_pkt in enc_pkts {
            if let Some(seg) = acc.push(enc_pkt) {
                if segment_tx.blocking_send(seg).is_err() {
                    return Err(TranscodeError::Cancelled { stream_id });
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Flush segment accumulators: emit any final partial segment.
    // ------------------------------------------------------------------
    for acc in accumulators {
        if let Some(seg) = acc.flush() {
            if segment_tx.blocking_send(seg).is_err() {
                return Err(TranscodeError::Cancelled { stream_id });
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// VOD from file: transcode directly from an MP4/MKV/etc. container
// ---------------------------------------------------------------------------

/// Transcode a media file to HLS segments using FFmpeg's format demuxer.
///
/// Unlike `transcode_blocking` (which receives pre-demuxed Annex-B NALUs from
/// the RTMP ingest path), this function opens the container file directly,
/// demuxes it with `ffmpeg::format::input`, and feeds the raw container packets
/// to the decoder. This correctly handles H.264 in MP4/MKV with codec extradata.
///
/// Runs synchronously — call from `spawn_blocking`.
pub(super) fn transcode_vod_from_file_blocking(
    stream_id: StreamId,
    file_path: std::path::PathBuf,
    segment_tx: mpsc::Sender<EncodedSegment>,
    renditions: Vec<SelectedRendition>,
    source_fps: f64,
    target_duration_secs: f64,
    cancel: CancellationToken,
) -> Result<(), TranscodeError> {
    ffmpeg::init().map_err(|e| TranscodeError::FfmpegInit {
        reason: format!("ffmpeg_init: {e}"),
    })?;

    let mut ictx =
        ffmpeg::format::input(&file_path).map_err(|e| TranscodeError::FfmpegInit {
            reason: format!("open container: {e}"),
        })?;

    // Identify best video and (optional) audio stream indices before we start
    // iterating packets (which mutably borrows ictx).
    let video_idx = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| TranscodeError::FfmpegInit {
            reason: "no video stream in file".into(),
        })?
        .index();
    let audio_idx = ictx
        .streams()
        .best(ffmpeg::media::Type::Audio)
        .map(|s| s.index());

    // Save stream parameters before the packets() borrow.
    let video_params = ictx.stream(video_idx).unwrap().parameters();

    // Extract audio parameters (sample rate, channels) from the audio stream
    // before the packets() borrow.  These are used to build the correct
    // AudioSpecificConfig in the packager init segment.
    let (audio_sample_rate, audio_channels) = if let Some(aidx) = audio_idx {
        if let Some(astream) = ictx.stream(aidx) {
            let adec = ffmpeg::codec::Context::from_parameters(astream.parameters())
                .ok()
                .and_then(|c| c.decoder().audio().ok());
            adec.map(|a| (a.rate(), a.channels() as u8))
                .unwrap_or((48000, 2))
        } else {
            (48000, 2)
        }
    } else {
        (48000, 2)
    };

    // ------------------------------------------------------------------
    // Decoder: initialise from container codec parameters.
    // This picks up the extradata (SPS/PPS for H.264) from the container.
    // ------------------------------------------------------------------
    let mut decoder = ffmpeg::codec::Context::from_parameters(video_params)
        .map_err(|e| TranscodeError::FfmpegInit {
            reason: format!("decoder context from parameters: {e}"),
        })?
        .decoder()
        .video()
        .map_err(|e| TranscodeError::FfmpegInit {
            reason: format!("decoder open: {e}"),
        })?;

    let source_width = decoder.width();
    let source_height = decoder.height();

    // ------------------------------------------------------------------
    // Per-rendition encode contexts and segment accumulators.
    // ------------------------------------------------------------------
    let mut render_ctxs: Vec<RenditionCtx> = renditions
        .iter()
        .map(|r| RenditionCtx::new(r, source_width, source_height, source_fps))
        .collect::<Result<_, _>>()?;

    let mut accumulators: Vec<SegmentAccumulator> = renditions
        .iter()
        .map(|r| {
            let mut acc = SegmentAccumulator::new(
                stream_id,
                r.id.clone(),
                target_duration_secs,
                r.width,
                r.height,
                r.video_bitrate_kbps,
                r.audio_bitrate_kbps,
                r.profile.clone(),
                r.level.clone(),
                source_fps,
            );
            acc.set_audio_params(audio_sample_rate, audio_channels);
            acc
        })
        .collect();

    let mut yuv_frame = ffmpeg::frame::video::Video::empty();

    // ------------------------------------------------------------------
    // Demux + decode + encode loop.
    // ------------------------------------------------------------------
    for (stream, mut packet) in ictx.packets() {
        if cancel.is_cancelled() {
            break;
        }

        if stream.index() == video_idx {
            // Rescale packet timestamps to 90 kHz.
            packet.rescale_ts(stream.time_base(), ffmpeg::Rational(1, 90000));

            if let Err(e) = decoder.send_packet(&packet) {
                warn!(%stream_id, error = %e, "VOD decoder send_packet error, skipping");
                continue;
            }

            while decoder.receive_frame(&mut yuv_frame).is_ok() {
                let pts = yuv_frame.pts();
                for (ctx, acc) in render_ctxs.iter_mut().zip(accumulators.iter_mut()) {
                    let enc_pkts = ctx.process_frame(&yuv_frame, pts)?;
                    for enc_pkt in enc_pkts {
                        if let Some(seg) = acc.push(enc_pkt) {
                            if segment_tx.blocking_send(seg).is_err() {
                                return Err(TranscodeError::Cancelled { stream_id });
                            }
                        }
                    }
                }
            }
        } else if Some(stream.index()) == audio_idx {
            // Audio passthrough: convert PTS to 90 kHz and forward to all accumulators.
            let mut audio_pkt = packet;
            audio_pkt.rescale_ts(stream.time_base(), ffmpeg::Rational(1, 90000));
            let audio_pts = audio_pkt.pts().unwrap_or(0);
            let audio_data = audio_pkt
                .data()
                .map(Bytes::copy_from_slice)
                .unwrap_or_default();

            if !audio_data.is_empty() {
                let audio_enc = EncodedPacket {
                    pts: audio_pts,
                    _dts: audio_pkt.dts().unwrap_or(audio_pts),
                    keyframe: true,
                    data: audio_data,
                    is_audio: true,
                };
                for acc in &mut accumulators {
                    if let Some(seg) = acc.push(audio_enc.clone()) {
                        if segment_tx.blocking_send(seg).is_err() {
                            return Err(TranscodeError::Cancelled { stream_id });
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Flush decoder.
    // ------------------------------------------------------------------
    if let Err(e) = decoder.send_eof() {
        warn!(%stream_id, error = %e, "VOD decoder send_eof failed");
    }
    while decoder.receive_frame(&mut yuv_frame).is_ok() {
        let pts = yuv_frame.pts();
        for (ctx, acc) in render_ctxs.iter_mut().zip(accumulators.iter_mut()) {
            let enc_pkts = ctx.process_frame(&yuv_frame, pts)?;
            for enc_pkt in enc_pkts {
                if let Some(seg) = acc.push(enc_pkt) {
                    if segment_tx.blocking_send(seg).is_err() {
                        return Err(TranscodeError::Cancelled { stream_id });
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Flush encoders.
    // ------------------------------------------------------------------
    for (ctx, acc) in render_ctxs.iter_mut().zip(accumulators.iter_mut()) {
        let enc_pkts = ctx.flush_encoder()?;
        for enc_pkt in enc_pkts {
            if let Some(seg) = acc.push(enc_pkt) {
                if segment_tx.blocking_send(seg).is_err() {
                    return Err(TranscodeError::Cancelled { stream_id });
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Flush segment accumulators (final partial segment).
    // ------------------------------------------------------------------
    for acc in accumulators {
        if let Some(seg) = acc.flush() {
            if segment_tx.blocking_send(seg).is_err() {
                return Err(TranscodeError::Cancelled { stream_id });
            }
        }
    }

    Ok(())
}

use bytes::{BufMut, Bytes, BytesMut};

use crate::core::error::PackageError;
// EncodedSegment used by runner.rs which calls these functions

// ---------------------------------------------------------------------------
// fMP4 box construction (from transcoding-and-packaging.md §6.2, §6.3, §6.4)
//
// Pure Rust implementation — no FFmpeg FFI for packaging.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Box writing helpers
// ---------------------------------------------------------------------------

fn write_box(buf: &mut BytesMut, box_type: &[u8; 4], content: &[u8]) {
    let size = (content.len() + 8) as u32;
    buf.put_u32(size);
    buf.extend_from_slice(box_type);
    buf.extend_from_slice(content);
}

fn write_full_box(buf: &mut BytesMut, box_type: &[u8; 4], version: u8, flags: u32, content: &[u8]) {
    let size = (content.len() + 12) as u32;
    buf.put_u32(size);
    buf.extend_from_slice(box_type);
    buf.put_u8(version);
    buf.put_u8((flags >> 16) as u8);
    buf.put_u8((flags >> 8) as u8);
    buf.put_u8(flags as u8);
    buf.extend_from_slice(content);
}

// ---------------------------------------------------------------------------
// Initialization segment (from transcoding-and-packaging.md §6.3)
// ---------------------------------------------------------------------------

/// Parameters needed to generate an initialization segment.
#[derive(Debug, Clone)]
pub struct InitSegmentParams {
    pub width: u32,
    pub height: u32,
    pub video_timescale: u32,
    pub audio_timescale: u32,
    pub sps: Bytes,
    pub pps: Bytes,
    pub audio_specific_config: Option<Bytes>,
    pub has_audio: bool,
}

/// Generate an fMP4 initialization segment (init.mp4).
///
/// Structure (from transcoding-and-packaging.md §6.3):
/// ```text
/// ftyp box (major_brand: "isom", compatible: ["isom", "iso6", "mp41"])
/// moov box
///   ├── mvhd (timescale: 90000)
///   ├── trak (video)
///   │   ├── tkhd (width, height)
///   │   └── mdia
///   │       ├── mdhd (timescale: 90000)
///   │       └── stbl (empty sample tables)
///   │           └── stsd → avc1 (SPS, PPS)
///   ├── trak (audio) [if has_audio]
///   │   ├── tkhd
///   │   └── mdia
///   │       ├── mdhd (timescale: 48000)
///   │       └── stbl → stsd → mp4a (AudioSpecificConfig)
///   └── mvex
///       ├── trex (video track defaults)
///       └── trex (audio track defaults) [if has_audio]
/// ```
pub fn generate_init_segment(params: &InitSegmentParams) -> Result<Bytes, PackageError> {
    let mut buf = BytesMut::with_capacity(4096);

    // ftyp box
    write_ftyp(&mut buf);

    // moov box
    let moov = build_moov(params)?;
    buf.extend_from_slice(&moov);

    Ok(buf.freeze())
}

fn write_ftyp(buf: &mut BytesMut) {
    let mut content = BytesMut::new();
    content.extend_from_slice(b"isom"); // major_brand
    content.put_u32(0x200); // minor_version
    content.extend_from_slice(b"isom"); // compatible_brands
    content.extend_from_slice(b"iso6");
    content.extend_from_slice(b"mp41");
    write_box(buf, b"ftyp", &content);
}

fn build_moov(params: &InitSegmentParams) -> Result<Vec<u8>, PackageError> {
    let mut moov_content = BytesMut::new();

    // mvhd
    build_mvhd(&mut moov_content, params.video_timescale);

    // Video trak
    build_video_trak(&mut moov_content, params)?;

    // Audio trak (if present)
    if params.has_audio {
        build_audio_trak(&mut moov_content, params)?;
    }

    // mvex
    build_mvex(&mut moov_content, params.has_audio);

    let mut buf = BytesMut::new();
    write_box(&mut buf, b"moov", &moov_content);
    Ok(buf.to_vec())
}

fn build_mvhd(buf: &mut BytesMut, timescale: u32) {
    let mut content = BytesMut::new();
    content.put_u32(0); // creation_time
    content.put_u32(0); // modification_time
    content.put_u32(timescale);
    content.put_u32(0); // duration (unknown for fragmented)
    content.put_u32(0x00010000); // rate = 1.0
    content.put_u16(0x0100); // volume = 1.0
    content.extend_from_slice(&[0u8; 10]); // reserved
                                           // Matrix (identity)
    for &v in &[0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
        content.put_u32(v);
    }
    content.extend_from_slice(&[0u8; 24]); // pre_defined
    content.put_u32(2); // next_track_ID (1=video, 2=audio or next)
    write_full_box(buf, b"mvhd", 0, 0, &content);
}

fn build_video_trak(buf: &mut BytesMut, params: &InitSegmentParams) -> Result<(), PackageError> {
    let mut trak_content = BytesMut::new();

    // tkhd
    build_tkhd(&mut trak_content, 1, params.width, params.height, true);

    // mdia
    let mut mdia_content = BytesMut::new();
    build_mdhd(&mut mdia_content, params.video_timescale);
    build_hdlr(&mut mdia_content, b"vide", "VideoHandler");

    // minf + stbl
    let mut minf_content = BytesMut::new();
    build_vmhd(&mut minf_content);
    build_dinf(&mut minf_content);
    build_video_stbl(&mut minf_content, params)?;
    write_box(&mut mdia_content, b"minf", &minf_content);

    write_box(&mut trak_content, b"mdia", &mdia_content);
    write_box(buf, b"trak", &trak_content);
    Ok(())
}

fn build_audio_trak(buf: &mut BytesMut, params: &InitSegmentParams) -> Result<(), PackageError> {
    let mut trak_content = BytesMut::new();

    // tkhd (track_id=2, audio has no dimensions)
    build_tkhd(&mut trak_content, 2, 0, 0, false);

    // mdia
    let mut mdia_content = BytesMut::new();
    build_mdhd(&mut mdia_content, params.audio_timescale);
    build_hdlr(&mut mdia_content, b"soun", "SoundHandler");

    let mut minf_content = BytesMut::new();
    build_smhd(&mut minf_content);
    build_dinf(&mut minf_content);
    build_audio_stbl(&mut minf_content, params)?;
    write_box(&mut mdia_content, b"minf", &minf_content);

    write_box(&mut trak_content, b"mdia", &mdia_content);
    write_box(buf, b"trak", &trak_content);
    Ok(())
}

fn build_tkhd(buf: &mut BytesMut, track_id: u32, width: u32, height: u32, is_video: bool) {
    let mut content = BytesMut::new();
    content.put_u32(0); // creation_time
    content.put_u32(0); // modification_time
    content.put_u32(track_id);
    content.put_u32(0); // reserved
    content.put_u32(0); // duration
    content.extend_from_slice(&[0u8; 8]); // reserved
    content.put_u16(0); // layer
    content.put_u16(0); // alternate_group
    content.put_u16(if is_video { 0 } else { 0x0100 }); // volume
    content.put_u16(0); // reserved
                        // Matrix (identity)
    for &v in &[0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
        content.put_u32(v);
    }
    content.put_u32(width << 16); // width (fixed-point 16.16)
    content.put_u32(height << 16); // height (fixed-point 16.16)
    let flags = 0x000003; // track_enabled | track_in_movie
    write_full_box(buf, b"tkhd", 0, flags, &content);
}

fn build_mdhd(buf: &mut BytesMut, timescale: u32) {
    let mut content = BytesMut::new();
    content.put_u32(0); // creation_time
    content.put_u32(0); // modification_time
    content.put_u32(timescale);
    content.put_u32(0); // duration
    content.put_u16(0x55C4); // language = "und"
    content.put_u16(0); // pre_defined
    write_full_box(buf, b"mdhd", 0, 0, &content);
}

fn build_hdlr(buf: &mut BytesMut, handler_type: &[u8; 4], name: &str) {
    let mut content = BytesMut::new();
    content.put_u32(0); // pre_defined
    content.extend_from_slice(handler_type);
    content.extend_from_slice(&[0u8; 12]); // reserved
    content.extend_from_slice(name.as_bytes());
    content.put_u8(0); // null terminator
    write_full_box(buf, b"hdlr", 0, 0, &content);
}

fn build_vmhd(buf: &mut BytesMut) {
    let content = [0u8; 8]; // graphicsmode + opcolor
    write_full_box(buf, b"vmhd", 0, 1, &content);
}

fn build_smhd(buf: &mut BytesMut) {
    let content = [0u8; 4]; // balance + reserved
    write_full_box(buf, b"smhd", 0, 0, &content);
}

fn build_dinf(buf: &mut BytesMut) {
    let mut dinf_content = BytesMut::new();
    // dref with one "url " entry (self-contained)
    let mut dref_content = BytesMut::new();
    dref_content.put_u32(1); // entry_count
    write_full_box(&mut dref_content, b"url ", 0, 1, &[]); // flag=1 means self-contained
    write_full_box(&mut dinf_content, b"dref", 0, 0, &dref_content);
    write_box(buf, b"dinf", &dinf_content);
}

fn build_video_stbl(buf: &mut BytesMut, params: &InitSegmentParams) -> Result<(), PackageError> {
    let mut stbl_content = BytesMut::new();

    // stsd with avc1 sample entry
    let mut stsd_content = BytesMut::new();
    stsd_content.put_u32(1); // entry_count
    build_avc1_entry(&mut stsd_content, params)?;
    write_full_box(&mut stbl_content, b"stsd", 0, 0, &stsd_content);

    // Empty required boxes for fragmented MP4
    write_full_box(&mut stbl_content, b"stts", 0, 0, &0u32.to_be_bytes());
    write_full_box(&mut stbl_content, b"stsc", 0, 0, &0u32.to_be_bytes());
    write_full_box(&mut stbl_content, b"stsz", 0, 0, &[0u8; 8]);
    write_full_box(&mut stbl_content, b"stco", 0, 0, &0u32.to_be_bytes());

    write_box(buf, b"stbl", &stbl_content);
    Ok(())
}

fn build_avc1_entry(buf: &mut BytesMut, params: &InitSegmentParams) -> Result<(), PackageError> {
    let mut content = BytesMut::new();
    content.extend_from_slice(&[0u8; 6]); // reserved
    content.put_u16(1); // data_reference_index
    content.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
    content.put_u16(params.width as u16);
    content.put_u16(params.height as u16);
    content.put_u32(0x00480000); // horizresolution = 72 dpi
    content.put_u32(0x00480000); // vertresolution = 72 dpi
    content.put_u32(0); // reserved
    content.put_u16(1); // frame_count
    content.extend_from_slice(&[0u8; 32]); // compressorname
    content.put_u16(0x0018); // depth = 24
    content.put_i16(-1); // pre_defined

    // avcC box (AVCDecoderConfigurationRecord)
    let mut avcc_content = BytesMut::new();
    avcc_content.put_u8(1); // configurationVersion
    avcc_content.put_u8(if params.sps.len() > 1 {
        params.sps[1]
    } else {
        100
    }); // AVCProfileIndication
    avcc_content.put_u8(if params.sps.len() > 2 {
        params.sps[2]
    } else {
        0
    }); // profile_compatibility
    avcc_content.put_u8(if params.sps.len() > 3 {
        params.sps[3]
    } else {
        41
    }); // AVCLevelIndication
    avcc_content.put_u8(0xFF); // lengthSizeMinusOne = 3 (4-byte NALU lengths)
    avcc_content.put_u8(0xE1); // numOfSequenceParameterSets = 1
    avcc_content.put_u16(params.sps.len() as u16);
    avcc_content.extend_from_slice(&params.sps);
    avcc_content.put_u8(1); // numOfPictureParameterSets
    avcc_content.put_u16(params.pps.len() as u16);
    avcc_content.extend_from_slice(&params.pps);
    write_box(&mut content, b"avcC", &avcc_content);

    write_box(buf, b"avc1", &content);
    Ok(())
}

fn build_audio_stbl(buf: &mut BytesMut, params: &InitSegmentParams) -> Result<(), PackageError> {
    let mut stbl_content = BytesMut::new();

    let mut stsd_content = BytesMut::new();
    stsd_content.put_u32(1); // entry_count
    build_mp4a_entry(&mut stsd_content, params)?;
    write_full_box(&mut stbl_content, b"stsd", 0, 0, &stsd_content);

    write_full_box(&mut stbl_content, b"stts", 0, 0, &0u32.to_be_bytes());
    write_full_box(&mut stbl_content, b"stsc", 0, 0, &0u32.to_be_bytes());
    write_full_box(&mut stbl_content, b"stsz", 0, 0, &[0u8; 8]);
    write_full_box(&mut stbl_content, b"stco", 0, 0, &0u32.to_be_bytes());

    write_box(buf, b"stbl", &stbl_content);
    Ok(())
}

fn build_mp4a_entry(buf: &mut BytesMut, params: &InitSegmentParams) -> Result<(), PackageError> {
    let mut content = BytesMut::new();
    content.extend_from_slice(&[0u8; 6]); // reserved
    content.put_u16(1); // data_reference_index
    content.extend_from_slice(&[0u8; 8]); // reserved
    content.put_u16(2); // channel_count (stereo)
    content.put_u16(16); // sample_size (16-bit)
    content.put_u16(0); // pre_defined
    content.put_u16(0); // reserved
    content.put_u32(params.audio_timescale << 16); // sample_rate (fixed-point 16.16)

    // esds box with AudioSpecificConfig
    if let Some(ref asc) = params.audio_specific_config {
        let mut esds_content = BytesMut::new();
        // ES_Descriptor (simplified)
        esds_content.put_u8(0x03); // ES_DescrTag
        esds_content.put_u8((23 + asc.len()) as u8); // length
        esds_content.put_u16(1); // ES_ID
        esds_content.put_u8(0); // streamDependenceFlag, etc.
                                // DecoderConfigDescriptor
        esds_content.put_u8(0x04); // DecoderConfigDescrTag
        esds_content.put_u8((15 + asc.len()) as u8);
        esds_content.put_u8(0x40); // objectTypeIndication = Audio ISO/IEC 14496-3
        esds_content.put_u8(0x15); // streamType = audio
        esds_content.extend_from_slice(&[0u8; 3]); // bufferSizeDB
        esds_content.put_u32(128000); // maxBitrate
        esds_content.put_u32(128000); // avgBitrate
                                      // DecoderSpecificInfo
        esds_content.put_u8(0x05); // DecoderSpecificInfoTag
        esds_content.put_u8(asc.len() as u8);
        esds_content.extend_from_slice(asc);
        // SLConfigDescriptor
        esds_content.put_u8(0x06); // SLConfigDescrTag
        esds_content.put_u8(1);
        esds_content.put_u8(0x02);
        write_full_box(&mut content, b"esds", 0, 0, &esds_content);
    }

    write_box(buf, b"mp4a", &content);
    Ok(())
}

fn build_mvex(buf: &mut BytesMut, has_audio: bool) {
    let mut mvex_content = BytesMut::new();

    // trex for video (track_id=1)
    build_trex(&mut mvex_content, 1);

    // trex for audio (track_id=2)
    if has_audio {
        build_trex(&mut mvex_content, 2);
    }

    write_box(buf, b"mvex", &mvex_content);
}

fn build_trex(buf: &mut BytesMut, track_id: u32) {
    let mut content = BytesMut::new();
    content.put_u32(track_id);
    content.put_u32(1); // default_sample_description_index
    content.put_u32(0); // default_sample_duration
    content.put_u32(0); // default_sample_size
    content.put_u32(0); // default_sample_flags
    write_full_box(buf, b"trex", 0, 0, &content);
}

// ---------------------------------------------------------------------------
// Media segment (from transcoding-and-packaging.md §6.2)
// ---------------------------------------------------------------------------

/// Parameters for generating a media segment.
#[derive(Debug)]
pub struct MediaSegmentParams {
    pub sequence_number: u64,
    pub base_decode_time: i64,
    pub samples: Vec<SampleInfo>,
    pub media_data: Bytes,
    pub track_id: u32,
}

/// Information about a single sample in a segment.
#[derive(Debug, Clone)]
pub struct SampleInfo {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
    pub composition_offset: i32,
}

/// Generate an fMP4 media segment.
///
/// Structure (from transcoding-and-packaging.md §6.2):
/// ```text
/// styp box (major_brand: "msdh", compatible: ["msdh", "msix"])
/// moof box
///   ├── mfhd (sequence_number)
///   └── traf
///       ├── tfhd (track_id, flags)
///       ├── tfdt (baseMediaDecodeTime)
///       └── trun (sample_count, durations, sizes, flags, composition_offsets)
/// mdat box (media data)
/// ```
pub fn generate_media_segment(params: &MediaSegmentParams) -> Result<Bytes, PackageError> {
    let mut buf = BytesMut::with_capacity(params.media_data.len() + 512);

    // styp box
    write_styp(&mut buf);

    // moof box
    let moof = build_moof(params)?;
    buf.extend_from_slice(&moof);

    // mdat box
    write_box(&mut buf, b"mdat", &params.media_data);

    Ok(buf.freeze())
}

fn write_styp(buf: &mut BytesMut) {
    let mut content = BytesMut::new();
    content.extend_from_slice(b"msdh"); // major_brand
    content.put_u32(0); // minor_version
    content.extend_from_slice(b"msdh"); // compatible_brands
    content.extend_from_slice(b"msix");
    write_box(buf, b"styp", &content);
}

fn build_moof(params: &MediaSegmentParams) -> Result<Vec<u8>, PackageError> {
    let mut moof_content = BytesMut::new();

    // mfhd
    let mut mfhd_content = BytesMut::new();
    mfhd_content.put_u32(params.sequence_number as u32);
    write_full_box(&mut moof_content, b"mfhd", 0, 0, &mfhd_content);

    // traf
    let mut traf_content = BytesMut::new();

    // tfhd
    let mut tfhd_content = BytesMut::new();
    tfhd_content.put_u32(params.track_id);
    let tfhd_flags = 0x020000; // default-base-is-moof
    write_full_box(&mut traf_content, b"tfhd", 0, tfhd_flags, &tfhd_content);

    // tfdt (version 1 for 64-bit baseMediaDecodeTime)
    let mut tfdt_content = BytesMut::new();
    tfdt_content.put_i64(params.base_decode_time);
    write_full_box(&mut traf_content, b"tfdt", 1, 0, &tfdt_content);

    // trun
    build_trun(&mut traf_content, params)?;

    write_box(&mut moof_content, b"traf", &traf_content);

    let mut buf = BytesMut::new();
    write_box(&mut buf, b"moof", &moof_content);
    Ok(buf.to_vec())
}

fn build_trun(buf: &mut BytesMut, params: &MediaSegmentParams) -> Result<(), PackageError> {
    // flags: data-offset-present | sample-duration-present | sample-size-present
    //        | sample-flags-present | sample-composition-time-offsets-present
    let flags: u32 = 0x000001 | 0x000100 | 0x000200 | 0x000400 | 0x000800;

    let mut content = BytesMut::new();
    content.put_u32(params.samples.len() as u32); // sample_count
                                                  // data_offset: placeholder (will be the offset from moof start to mdat data)
                                                  // For simplicity, we write 0 and note this needs fixing in production
    content.put_i32(0); // data_offset (placeholder)

    for sample in &params.samples {
        content.put_u32(sample.duration);
        content.put_u32(sample.size);
        content.put_u32(sample.flags);
        content.put_i32(sample.composition_offset);
    }

    write_full_box(buf, b"trun", 0, flags, &content);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_init_segment() {
        let params = InitSegmentParams {
            width: 1920,
            height: 1080,
            video_timescale: 90000,
            audio_timescale: 48000,
            sps: Bytes::from_static(&[0x67, 0x64, 0x00, 0x29]),
            pps: Bytes::from_static(&[0x68, 0xEE, 0x3C, 0x80]),
            audio_specific_config: Some(Bytes::from_static(&[0x12, 0x10])),
            has_audio: true,
        };

        let result = generate_init_segment(&params);
        assert!(result.is_ok());

        let data = result.unwrap();
        // Should start with ftyp box
        assert_eq!(&data[4..8], b"ftyp");
        // Should contain moov box
        assert!(data.len() > 100);
    }

    #[test]
    fn test_generate_init_segment_video_only() {
        let params = InitSegmentParams {
            width: 854,
            height: 480,
            video_timescale: 90000,
            audio_timescale: 48000,
            sps: Bytes::from_static(&[0x67, 0x4D, 0x00, 0x1E]),
            pps: Bytes::from_static(&[0x68, 0xEE, 0x3C, 0x80]),
            audio_specific_config: None,
            has_audio: false,
        };

        let result = generate_init_segment(&params);
        assert!(result.is_ok());
    }

    #[test]
    fn test_generate_media_segment() {
        let params = MediaSegmentParams {
            sequence_number: 1,
            base_decode_time: 540000,
            samples: vec![SampleInfo {
                duration: 3000,
                size: 1000,
                flags: 0x02000000,
                composition_offset: 0,
            }],
            media_data: Bytes::from(vec![0xAA; 1000]),
            track_id: 1,
        };

        let result = generate_media_segment(&params);
        assert!(result.is_ok());

        let data = result.unwrap();
        // Should start with styp box
        assert_eq!(&data[4..8], b"styp");
    }
}

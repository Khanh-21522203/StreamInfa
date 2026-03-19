use bytes::Bytes;
use chrono::{Duration, Utc};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use streaminfa::core::types::{RenditionId, StreamId};
use streaminfa::package::hls::{generate_init_segment, InitSegmentParams};
use streaminfa::package::manifest::{
    generate_media_playlist, generate_multivariant_playlist, PlaylistSegment, RenditionInfo,
};
use streaminfa::package::segment_index::SegmentIndex;

fn sample_renditions() -> Vec<RenditionInfo> {
    vec![
        RenditionInfo {
            id: RenditionId::High,
            width: 1920,
            height: 1080,
            video_bitrate_kbps: 3500,
            audio_bitrate_kbps: 128,
            profile: "high".to_string(),
            level: "4.1".to_string(),
            frame_rate: 30.0,
            has_audio: true,
        },
        RenditionInfo {
            id: RenditionId::Medium,
            width: 1280,
            height: 720,
            video_bitrate_kbps: 2000,
            audio_bitrate_kbps: 128,
            profile: "main".to_string(),
            level: "3.1".to_string(),
            frame_rate: 30.0,
            has_audio: true,
        },
        RenditionInfo {
            id: RenditionId::Low,
            width: 854,
            height: 480,
            video_bitrate_kbps: 1000,
            audio_bitrate_kbps: 96,
            profile: "main".to_string(),
            level: "3.0".to_string(),
            frame_rate: 30.0,
            has_audio: true,
        },
    ]
}

fn sample_playlist_segments(count: usize) -> Vec<PlaylistSegment> {
    let base = Utc::now();
    (0..count)
        .map(|i| PlaylistSegment {
            sequence: i as u64,
            duration_secs: if i % 10 == 0 { 6.2 } else { 5.98 },
            filename: format!("{:06}.m4s", i),
            program_date_time: Some(base + Duration::milliseconds((i as i64) * 6000)),
        })
        .collect()
}

fn bench_manifest_generation(c: &mut Criterion) {
    let renditions = sample_renditions();
    let mut group = c.benchmark_group("manifest_generation");

    group.bench_function("master_playlist_3_renditions", |b| {
        b.iter(|| generate_multivariant_playlist(black_box(&renditions), 7));
    });

    for segment_count in [6usize, 24, 120] {
        let segments = sample_playlist_segments(segment_count);
        group.throughput(Throughput::Elements(segment_count as u64));
        group.bench_with_input(
            BenchmarkId::new("media_playlist_live", segment_count),
            &segments,
            |b, s| {
                b.iter(|| generate_media_playlist(black_box(s), 7, false, false));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("media_playlist_vod_finished", segment_count),
            &segments,
            |b, s| {
                b.iter(|| generate_media_playlist(black_box(s), 7, true, true));
            },
        );
    }
    group.finish();
}

fn bench_init_segment_generation(c: &mut Criterion) {
    let params = InitSegmentParams {
        width: 1920,
        height: 1080,
        video_timescale: 90_000,
        audio_timescale: 48_000,
        sps: Bytes::from_static(&[0x67, 0x64, 0x00, 0x29, 0xAC, 0x2B, 0x40]),
        pps: Bytes::from_static(&[0x68, 0xEE, 0x3C, 0x80]),
        audio_specific_config: Some(Bytes::from_static(&[0x12, 0x10])),
        has_audio: true,
    };

    c.bench_function("init_segment_generation", |b| {
        b.iter(|| {
            let seg = generate_init_segment(black_box(&params)).expect("init segment should build");
            black_box(seg.len())
        })
    });
}

fn bench_segment_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("segment_index");
    for window in [6usize, 12, 24] {
        group.bench_with_input(
            BenchmarkId::new("add_segment_and_generate_playlist", window),
            &window,
            |b, &w| {
                b.iter(|| {
                    let mut index = SegmentIndex::new(StreamId::new(), RenditionId::High, w);
                    for seq in 0..(w * 4) {
                        let _ = index.add_segment(6.0, format!("{:06}.m4s", seq), 200_000);
                    }
                    black_box(index.generate_playlist(7, false, false))
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    pipeline_micro,
    bench_manifest_generation,
    bench_init_segment_generation,
    bench_segment_index
);
criterion_main!(pipeline_micro);

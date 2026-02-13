use crate::core::config::TranscodeProfile;
use crate::core::types::RenditionId;

// ---------------------------------------------------------------------------
// Rendition selection (from transcoding-and-packaging.md §2.3, §2.4)
// ---------------------------------------------------------------------------

/// A selected rendition with its encoding parameters.
#[derive(Debug, Clone)]
pub struct SelectedRendition {
    pub id: RenditionId,
    pub width: u32,
    pub height: u32,
    pub video_bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
    pub profile: String,
    pub level: String,
    pub preset: String,
}

/// Select which renditions to produce based on input resolution.
///
/// Rules (from transcoding-and-packaging.md §2.3):
/// - No upscaling: skip renditions above the source resolution.
/// - If input is below all rungs (< 480p), produce one rendition at source
///   resolution with the lowest rung's bitrate settings.
pub fn select_renditions(
    input_width: u32,
    input_height: u32,
    ladder: &[TranscodeProfile],
) -> Vec<SelectedRendition> {
    let mut selected: Vec<SelectedRendition> = ladder
        .iter()
        .filter(|r| r.width <= input_width && r.height <= input_height)
        .map(|r| SelectedRendition {
            id: name_to_rendition_id(&r.name),
            width: r.width,
            height: r.height,
            video_bitrate_kbps: r.bitrate_kbps,
            audio_bitrate_kbps: r.audio_bitrate_kbps,
            profile: r.profile.clone(),
            level: r.level.clone(),
            preset: r.preset.clone(),
        })
        .collect();

    // If no renditions match (input smaller than all rungs),
    // produce one rendition at source resolution with lowest rung's settings.
    if selected.is_empty() {
        if let Some(lowest) = ladder.last() {
            selected.push(SelectedRendition {
                id: RenditionId::Source,
                width: input_width,
                height: input_height,
                video_bitrate_kbps: lowest.bitrate_kbps,
                audio_bitrate_kbps: lowest.audio_bitrate_kbps,
                profile: lowest.profile.clone(),
                level: lowest.level.clone(),
                preset: lowest.preset.clone(),
            });
        }
    }

    selected
}

fn name_to_rendition_id(name: &str) -> RenditionId {
    match name {
        "high" => RenditionId::High,
        "medium" => RenditionId::Medium,
        "low" => RenditionId::Low,
        _ => RenditionId::Source,
    }
}

// ---------------------------------------------------------------------------
// Codec string generation (from transcoding-and-packaging.md §7.1)
// ---------------------------------------------------------------------------

/// Generate the HLS codec string for a rendition.
///
/// Format: `avc1.PPCCLL` where:
/// - PP = profile_idc (hex)
/// - CC = constraint_set_flags (hex, 00 for our profiles)
/// - LL = level_idc (hex)
///
/// Examples from the doc:
/// - High L4.1: `avc1.640029` (profile_idc=100, level_idc=41)
/// - Main L3.1: `avc1.4d001f` (profile_idc=77, level_idc=31)
/// - Main L3.0: `avc1.4d001e` (profile_idc=77, level_idc=30)
pub fn codec_string(profile: &str, level: &str) -> String {
    let profile_idc = match profile {
        "baseline" => 66u8,
        "main" => 77,
        "high" => 100,
        _ => 77, // default to main
    };

    let level_idc = parse_level(level);

    format!("avc1.{:02x}00{:02x}", profile_idc, level_idc)
}

/// Generate the full codecs attribute for HLS manifest.
/// e.g., `"avc1.640029,mp4a.40.2"`
pub fn codecs_attribute(profile: &str, level: &str, has_audio: bool) -> String {
    let video = codec_string(profile, level);
    if has_audio {
        format!("{},mp4a.40.2", video)
    } else {
        video
    }
}

/// Calculate BANDWIDTH for HLS manifest.
/// Video bitrate + audio bitrate + 10% overhead (from transcoding-and-packaging.md §7.1).
pub fn bandwidth(video_bitrate_kbps: u32, audio_bitrate_kbps: u32) -> u32 {
    let total_kbps = video_bitrate_kbps + audio_bitrate_kbps;
    (total_kbps as f64 * 1.1 * 1000.0) as u32
}

fn parse_level(level: &str) -> u8 {
    // Level string like "4.1" → level_idc = 41
    let parts: Vec<&str> = level.split('.').collect();
    match parts.as_slice() {
        [major, minor] => {
            let m: u8 = major.parse().unwrap_or(3);
            let n: u8 = minor.parse().unwrap_or(0);
            m * 10 + n
        }
        [major] => {
            let m: u8 = major.parse().unwrap_or(3);
            m * 10
        }
        _ => 31, // default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::TranscodeProfile;

    fn test_ladder() -> Vec<TranscodeProfile> {
        vec![
            TranscodeProfile {
                name: "high".to_string(),
                width: 1920,
                height: 1080,
                bitrate_kbps: 3500,
                audio_bitrate_kbps: 128,
                profile: "high".to_string(),
                level: "4.1".to_string(),
                preset: "medium".to_string(),
            },
            TranscodeProfile {
                name: "medium".to_string(),
                width: 1280,
                height: 720,
                bitrate_kbps: 2000,
                audio_bitrate_kbps: 128,
                profile: "main".to_string(),
                level: "3.1".to_string(),
                preset: "medium".to_string(),
            },
            TranscodeProfile {
                name: "low".to_string(),
                width: 854,
                height: 480,
                bitrate_kbps: 1000,
                audio_bitrate_kbps: 96,
                profile: "main".to_string(),
                level: "3.0".to_string(),
                preset: "medium".to_string(),
            },
        ]
    }

    #[test]
    fn test_select_all_renditions_for_1080p() {
        let ladder = test_ladder();
        let selected = select_renditions(1920, 1080, &ladder);
        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].id, RenditionId::High);
        assert_eq!(selected[1].id, RenditionId::Medium);
        assert_eq!(selected[2].id, RenditionId::Low);
    }

    #[test]
    fn test_select_renditions_for_720p() {
        let ladder = test_ladder();
        let selected = select_renditions(1280, 720, &ladder);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].id, RenditionId::Medium);
        assert_eq!(selected[1].id, RenditionId::Low);
    }

    #[test]
    fn test_select_renditions_for_480p() {
        let ladder = test_ladder();
        let selected = select_renditions(854, 480, &ladder);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, RenditionId::Low);
    }

    #[test]
    fn test_select_renditions_below_minimum() {
        let ladder = test_ladder();
        let selected = select_renditions(640, 360, &ladder);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, RenditionId::Source);
        assert_eq!(selected[0].width, 640);
        assert_eq!(selected[0].height, 360);
    }

    #[test]
    fn test_codec_string_high() {
        assert_eq!(codec_string("high", "4.1"), "avc1.640029");
    }

    #[test]
    fn test_codec_string_main_31() {
        assert_eq!(codec_string("main", "3.1"), "avc1.4d001f");
    }

    #[test]
    fn test_codec_string_main_30() {
        assert_eq!(codec_string("main", "3.0"), "avc1.4d001e");
    }

    #[test]
    fn test_codecs_attribute_with_audio() {
        assert_eq!(
            codecs_attribute("high", "4.1", true),
            "avc1.640029,mp4a.40.2"
        );
    }

    #[test]
    fn test_bandwidth_calculation() {
        // 3500 + 128 = 3628 kbps × 1.1 × 1000 = 3990800
        let bw = bandwidth(3500, 128);
        assert_eq!(bw, 3990800);
    }
}

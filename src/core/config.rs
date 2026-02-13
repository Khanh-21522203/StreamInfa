use serde::{Deserialize, Serialize};
use std::path::Path;

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub ingest: IngestConfig,
    pub transcode: TranscodeConfig,
    pub packaging: PackagingConfig,
    pub storage: StorageConfig,
    pub delivery: DeliveryConfig,
    pub observability: ObservabilityConfig,
    pub auth: AuthConfig,
    pub cache: CacheConfig,
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub control_port: u16,
    pub rtmp_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestConfig {
    pub max_bitrate_kbps: u32,
    pub max_upload_size_bytes: u64,
    pub allowed_codecs: Vec<String>,
    pub allowed_audio_codecs: Vec<String>,
    pub max_duration_secs: f64,
    pub upload_timeout_secs: u64,
    pub max_concurrent_rtmp_connections: usize,
    pub max_concurrent_live_streams: usize,
    pub max_concurrent_uploads: usize,
    pub max_pending_connections: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscodeConfig {
    pub profile_ladder: Vec<TranscodeProfile>,
    pub keyframe_interval_secs: u32,
    pub max_blocking_threads: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscodeProfile {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
    pub profile: String,
    pub level: String,
    pub preset: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackagingConfig {
    pub segment_duration_secs: u32,
    pub live_window_segments: u32,
    pub hls_version: u32,
    pub segment_retention_minutes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub backend: String,
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
    #[serde(default)]
    pub path_style: bool,
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
}

fn default_max_concurrent_requests() -> usize {
    50
}
fn default_request_timeout_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryConfig {
    pub cache_control_live: String,
    pub cache_control_vod: String,
    pub cors_allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    pub log_level: String,
    pub log_format: String,
    pub metrics_enabled: bool,
    pub tracing_enabled: bool,
    pub tracing_endpoint: String,
    #[serde(default)]
    pub tracing_sample_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub ingest_stream_keys: Vec<String>,
    pub admin_bearer_tokens: Vec<String>,
}

/// Security-related configuration (from security.md §4.3, §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    pub max_json_body_bytes: usize,
    #[serde(default)]
    pub max_rtmp_chunk_size: usize,
    #[serde(default)]
    pub max_amf_string_length: usize,
    #[serde(default)]
    pub max_flv_tag_size_bytes: usize,
    #[serde(default)]
    pub brute_force_max_attempts: u32,
    #[serde(default)]
    pub brute_force_window_secs: u64,
    #[serde(default)]
    pub brute_force_block_secs: u64,
}

/// Cache configuration (from storage-and-delivery.md §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Whether caching is enabled.
    pub enabled: bool,
    /// Maximum cache size in bytes.
    pub max_size_bytes: u64,
    /// Default TTL in seconds (used for master playlists and fallback).
    pub ttl_secs: u64,
    /// TTL for segment objects in seconds.
    pub segment_ttl_secs: u64,
}

impl AppConfig {
    /// Load configuration with layered overrides:
    /// 1. config/default.toml
    /// 2. config/{env}.toml (based on STREAMINFA_ENV)
    /// 3. Environment variables (STREAMINFA_* prefix)
    pub fn load() -> anyhow::Result<Self> {
        let default_path = Path::new("config/default.toml");
        let default_content = std::fs::read_to_string(default_path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", default_path.display(), e))?;

        let mut config: AppConfig = toml::from_str(&default_content)
            .map_err(|e| anyhow::anyhow!("failed to parse {}: {}", default_path.display(), e))?;

        // Layer 2: environment-specific overrides
        let env_name =
            std::env::var("STREAMINFA_ENV").unwrap_or_else(|_| "development".to_string());
        let env_path = format!("config/{}.toml", env_name);
        if let Ok(env_content) = std::fs::read_to_string(&env_path) {
            let env_config: AppConfig = toml::from_str(&env_content)
                .map_err(|e| anyhow::anyhow!("failed to parse {}: {}", env_path, e))?;
            config = env_config;
        }

        // Layer 3: environment variable overrides (selected keys)
        Self::apply_env_overrides(&mut config);

        Ok(config)
    }

    fn apply_env_overrides(config: &mut AppConfig) {
        if let Ok(v) = std::env::var("STREAMINFA_SERVER_HOST") {
            config.server.host = v;
        }
        if let Ok(v) = std::env::var("STREAMINFA_SERVER_CONTROL_PORT") {
            if let Ok(port) = v.parse() {
                config.server.control_port = port;
            }
        }
        if let Ok(v) = std::env::var("STREAMINFA_SERVER_RTMP_PORT") {
            if let Ok(port) = v.parse() {
                config.server.rtmp_port = port;
            }
        }
        if let Ok(v) = std::env::var("STREAMINFA_STORAGE_ENDPOINT") {
            config.storage.endpoint = v;
        }
        if let Ok(v) = std::env::var("STREAMINFA_STORAGE_BUCKET") {
            config.storage.bucket = v;
        }
        if let Ok(v) = std::env::var("STREAMINFA_STORAGE_ACCESS_KEY_ID") {
            config.storage.access_key_id = v;
        }
        if let Ok(v) = std::env::var("STREAMINFA_STORAGE_SECRET_ACCESS_KEY") {
            config.storage.secret_access_key = v;
        }
        if let Ok(v) = std::env::var("STREAMINFA_STORAGE_REGION") {
            config.storage.region = v;
        }
        if let Ok(v) = std::env::var("STREAMINFA_AUTH_INGEST_STREAM_KEYS") {
            config.auth.ingest_stream_keys = v.split(',').map(|s| s.trim().to_string()).collect();
        }
        if let Ok(v) = std::env::var("STREAMINFA_AUTH_ADMIN_BEARER_TOKENS") {
            config.auth.admin_bearer_tokens = v.split(',').map(|s| s.trim().to_string()).collect();
        }
        if let Ok(v) = std::env::var("STREAMINFA_OBSERVABILITY_LOG_LEVEL") {
            config.observability.log_level = v;
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                control_port: 8080,
                rtmp_port: 1935,
            },
            ingest: IngestConfig {
                max_bitrate_kbps: 20000,
                max_upload_size_bytes: 10_737_418_240,
                allowed_codecs: vec!["h264".to_string()],
                allowed_audio_codecs: vec!["aac".to_string(), "mp3".to_string()],
                max_duration_secs: 21600.0,
                upload_timeout_secs: 60,
                max_concurrent_rtmp_connections: 50,
                max_concurrent_live_streams: 5,
                max_concurrent_uploads: 10,
                max_pending_connections: 100,
            },
            transcode: TranscodeConfig {
                profile_ladder: vec![
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
                ],
                keyframe_interval_secs: 2,
                max_blocking_threads: 64,
            },
            packaging: PackagingConfig {
                segment_duration_secs: 6,
                live_window_segments: 5,
                hls_version: 7,
                segment_retention_minutes: 60,
            },
            storage: StorageConfig {
                backend: "s3".to_string(),
                endpoint: "http://localhost:9000".to_string(),
                bucket: "streaminfa-media".to_string(),
                access_key_id: String::new(),
                secret_access_key: String::new(),
                region: "us-east-1".to_string(),
                path_style: true,
                max_concurrent_requests: 50,
                request_timeout_secs: 30,
            },
            delivery: DeliveryConfig {
                cache_control_live: "no-cache, no-store".to_string(),
                cache_control_vod: "public, max-age=86400".to_string(),
                cors_allowed_origins: vec!["*".to_string()],
            },
            observability: ObservabilityConfig {
                log_level: "info".to_string(),
                log_format: "json".to_string(),
                metrics_enabled: true,
                tracing_enabled: false,
                tracing_endpoint: "http://localhost:4317".to_string(),
                tracing_sample_rate: 0.1,
            },
            auth: AuthConfig {
                ingest_stream_keys: Vec::new(),
                admin_bearer_tokens: Vec::new(),
            },
            cache: CacheConfig {
                enabled: true,
                max_size_bytes: 536_870_912, // 512 MB
                ttl_secs: 300,
                segment_ttl_secs: 3600,
            },
            security: SecurityConfig {
                max_json_body_bytes: 1_048_576, // 1 MB
                max_rtmp_chunk_size: 65536,
                max_amf_string_length: 4096,
                max_flv_tag_size_bytes: 16_777_216, // 16 MB
                brute_force_max_attempts: 5,
                brute_force_window_secs: 60,
                brute_force_block_secs: 300,
            },
        }
    }
}

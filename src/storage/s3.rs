use std::time::Duration;

use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use tracing::{debug, error, warn};

use crate::core::config::StorageConfig;
use crate::core::error::StorageError;
use crate::core::types::StreamMetadata;

use super::{GetObjectOutput, MediaStore, ObjectInfo};

// ---------------------------------------------------------------------------
// Retry constants (from storage-and-delivery.md §4.3)
// ---------------------------------------------------------------------------

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 100;

// ---------------------------------------------------------------------------
// S3MediaStore (from storage-and-delivery.md §4, §4.1)
// ---------------------------------------------------------------------------

/// Production storage backend wrapping `aws-sdk-s3`.
///
/// Supports both AWS S3 and S3-compatible stores (MinIO, DigitalOcean Spaces, etc.)
/// via configurable endpoint and path-style addressing.
pub struct S3MediaStore {
    client: Client,
    bucket: String,
    request_timeout: Duration,
}

impl S3MediaStore {
    /// Create a new S3MediaStore from configuration.
    pub async fn new(config: &StorageConfig) -> Result<Self, StorageError> {
        let credentials = Credentials::new(
            &config.access_key_id,
            &config.secret_access_key,
            None,
            None,
            "streaminfa-config",
        );

        let mut s3_config_builder = aws_sdk_s3::Config::builder()
            .region(Region::new(config.region.clone()))
            .credentials_provider(credentials)
            .force_path_style(config.path_style);

        if !config.endpoint.is_empty() {
            s3_config_builder = s3_config_builder.endpoint_url(&config.endpoint);
        }

        let s3_config = s3_config_builder.build();
        let client = Client::from_conf(s3_config);

        Ok(Self {
            client,
            bucket: config.bucket.clone(),
            request_timeout: Duration::from_secs(config.request_timeout_secs),
        })
    }

    /// Execute a PUT with retry logic (from storage-and-delivery.md §4.3).
    async fn put_with_retry(
        &self,
        key: &str,
        body: Bytes,
        content_type: &str,
    ) -> Result<(), StorageError> {
        let mut last_err = None;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let backoff = Duration::from_millis(INITIAL_BACKOFF_MS * (1 << (attempt - 1)));
                debug!(
                    key,
                    attempt,
                    backoff_ms = backoff.as_millis(),
                    "retrying S3 PUT"
                );
                tokio::time::sleep(backoff).await;
            }

            match self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(ByteStream::from(body.clone()))
                .content_type(content_type)
                .send()
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let err_str = e.to_string();
                    // Don't retry 403 (forbidden) — likely misconfigured credentials
                    if err_str.contains("403") || err_str.contains("Forbidden") {
                        return Err(StorageError::S3PutFailed {
                            path: key.to_string(),
                            reason: format!("forbidden (credentials issue): {}", err_str),
                        });
                    }
                    warn!(key, attempt, error = %err_str, "S3 PUT failed");
                    last_err = Some(err_str);
                }
            }
        }

        Err(StorageError::RetriesExhausted {
            path: key.to_string(),
        })
    }

    /// Execute a GET with retry logic.
    async fn get_with_retry(&self, key: &str) -> Result<GetObjectOutput, StorageError> {
        let mut last_err = None;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let backoff = Duration::from_millis(INITIAL_BACKOFF_MS * (1 << (attempt - 1)));
                tokio::time::sleep(backoff).await;
            }

            match self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(output) => {
                    let content_length = output.content_length.unwrap_or(0) as u64;
                    let content_type = output
                        .content_type
                        .unwrap_or_else(|| "application/octet-stream".to_string());
                    let etag = output.e_tag.unwrap_or_default();
                    let last_modified = output
                        .last_modified
                        .and_then(|t| DateTime::from_timestamp(t.secs(), t.subsec_nanos()))
                        .unwrap_or_else(Utc::now);

                    let body_bytes = output
                        .body
                        .collect()
                        .await
                        .map_err(|e| StorageError::S3GetFailed {
                            path: key.to_string(),
                            reason: e.to_string(),
                        })?
                        .into_bytes();

                    return Ok(GetObjectOutput {
                        body: Bytes::from(body_bytes),
                        _content_length: content_length,
                        content_type,
                        _last_modified: last_modified,
                        etag,
                    });
                }
                Err(e) => {
                    let err_str = e.to_string();
                    // Don't retry 404 — object doesn't exist
                    if err_str.contains("NoSuchKey") || err_str.contains("404") {
                        return Err(StorageError::S3GetFailed {
                            path: key.to_string(),
                            reason: "not found".to_string(),
                        });
                    }
                    if err_str.contains("403") || err_str.contains("Forbidden") {
                        return Err(StorageError::S3GetFailed {
                            path: key.to_string(),
                            reason: format!("forbidden: {}", err_str),
                        });
                    }
                    warn!(key, attempt, error = %err_str, "S3 GET failed");
                    last_err = Some(err_str);
                }
            }
        }

        Err(StorageError::RetriesExhausted {
            path: key.to_string(),
        })
    }
}

impl MediaStore for S3MediaStore {
    async fn put_segment(
        &self,
        path: &str,
        data: Bytes,
        content_type: &str,
    ) -> Result<(), StorageError> {
        self.put_with_retry(path, data, content_type).await
    }

    async fn put_manifest(&self, path: &str, content: &str) -> Result<(), StorageError> {
        self.put_with_retry(
            path,
            Bytes::from(content.to_string()),
            "application/vnd.apple.mpegurl",
        )
        .await
    }

    async fn get_object(&self, path: &str) -> Result<GetObjectOutput, StorageError> {
        self.get_with_retry(path).await
    }

    async fn get_object_range(
        &self,
        path: &str,
        start: u64,
        end: u64,
    ) -> Result<GetObjectOutput, StorageError> {
        let range = format!("bytes={}-{}", start, end);

        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(path)
            .range(&range)
            .send()
            .await
        {
            Ok(output) => {
                let content_length = output.content_length.unwrap_or(0) as u64;
                let content_type = output
                    .content_type
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                let etag = output.e_tag.unwrap_or_default();
                let last_modified = output
                    .last_modified
                    .and_then(|t| DateTime::from_timestamp(t.secs(), t.subsec_nanos()))
                    .unwrap_or_else(Utc::now);

                let body_bytes = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| StorageError::S3GetFailed {
                        path: path.to_string(),
                        reason: e.to_string(),
                    })?
                    .into_bytes();

                Ok(GetObjectOutput {
                    body: Bytes::from(body_bytes),
                    _content_length: content_length,
                    content_type,
                    _last_modified: last_modified,
                    etag,
                })
            }
            Err(e) => Err(StorageError::S3GetFailed {
                path: path.to_string(),
                reason: e.to_string(),
            }),
        }
    }

    async fn delete_object(&self, path: &str) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
            .map_err(|e| StorageError::S3DeleteFailed {
                path: path.to_string(),
                reason: e.to_string(),
            })?;
        Ok(())
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<u64, StorageError> {
        let mut deleted_count: u64 = 0;
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);

            if let Some(token) = &continuation_token {
                req = req.continuation_token(token);
            }

            let output = req.send().await.map_err(|e| StorageError::S3DeleteFailed {
                path: prefix.to_string(),
                reason: e.to_string(),
            })?;

            if let Some(contents) = output.contents {
                for obj in &contents {
                    if let Some(key) = &obj.key {
                        self.delete_object(key).await?;
                        deleted_count += 1;
                    }
                }
            }

            if output.is_truncated.unwrap_or(false) {
                continuation_token = output.next_continuation_token;
            } else {
                break;
            }
        }

        Ok(deleted_count)
    }

    async fn list_objects(&self, prefix: &str) -> Result<Vec<ObjectInfo>, StorageError> {
        let mut objects = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);

            if let Some(token) = &continuation_token {
                req = req.continuation_token(token);
            }

            let output = req.send().await.map_err(|e| StorageError::S3GetFailed {
                path: prefix.to_string(),
                reason: e.to_string(),
            })?;

            if let Some(contents) = output.contents {
                for obj in contents {
                    let key = obj.key.unwrap_or_default();
                    let size = obj.size.unwrap_or(0) as u64;
                    let last_modified = obj
                        .last_modified
                        .and_then(|t| DateTime::from_timestamp(t.secs(), t.subsec_nanos()))
                        .unwrap_or_else(Utc::now);

                    objects.push(ObjectInfo {
                        key,
                        size,
                        last_modified,
                    });
                }
            }

            if output.is_truncated.unwrap_or(false) {
                continuation_token = output.next_continuation_token;
            } else {
                break;
            }
        }

        Ok(objects)
    }
}

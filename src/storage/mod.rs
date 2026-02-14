pub mod cache;
pub mod cleanup;
pub mod memory;
#[cfg(feature = "s3")]
pub mod s3;
pub mod writer;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::error::StorageError;

// ---------------------------------------------------------------------------
// MediaStore trait (from storage-and-delivery.md §4)
// ---------------------------------------------------------------------------

/// Trait-based abstraction over the storage backend.
///
/// Why trait-based (from storage-and-delivery.md §4):
/// Enables mocking for unit tests. The production implementation (`S3MediaStore`)
/// wraps `aws-sdk-s3`. Tests can use `InMemoryMediaStore` without external deps.
pub trait MediaStore: Send + Sync {
    /// Write a segment or init segment to storage.
    fn put_segment(
        &self,
        path: &str,
        data: Bytes,
        content_type: &str,
    ) -> impl std::future::Future<Output = Result<(), StorageError>> + Send;

    /// Write a manifest (playlist) to storage. Overwrites atomically.
    fn put_manifest(
        &self,
        path: &str,
        content: &str,
    ) -> impl std::future::Future<Output = Result<(), StorageError>> + Send;

    /// Read an object from storage.
    fn get_object(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<GetObjectOutput, StorageError>> + Send;

    /// Read a byte range of an object from storage.
    fn get_object_range(
        &self,
        path: &str,
        start: u64,
        end: u64,
    ) -> impl std::future::Future<Output = Result<GetObjectOutput, StorageError>> + Send;

    /// Delete a single object from storage.
    fn delete_object(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<(), StorageError>> + Send;

    /// Delete all objects under a prefix. Returns count of objects deleted.
    fn delete_prefix(
        &self,
        prefix: &str,
    ) -> impl std::future::Future<Output = Result<u64, StorageError>> + Send;

    /// List objects under a prefix.
    fn list_objects(
        &self,
        prefix: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send;

    /// Write metadata JSON to storage (storage-and-delivery.md §4).
    fn put_metadata(
        &self,
        path: &str,
        metadata: &str,
    ) -> impl std::future::Future<Output = Result<(), StorageError>> + Send;

    /// HEAD an object to get metadata without downloading the body (storage-and-delivery.md §4).
    fn head_object(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<ObjectMeta, StorageError>> + Send;
}

// ---------------------------------------------------------------------------
// Storage types (from storage-and-delivery.md §4)
// ---------------------------------------------------------------------------

/// Output from a GET object operation.
#[derive(Debug, Clone)]
pub struct GetObjectOutput {
    pub body: Bytes,
    pub _content_length: u64,
    pub content_type: String,
    pub _last_modified: DateTime<Utc>,
    pub etag: String,
}

/// Information about an object from a LIST operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
}

/// Metadata returned by a HEAD object operation (storage-and-delivery.md §4).
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub content_length: u64,
    pub content_type: String,
    pub last_modified: DateTime<Utc>,
    pub etag: String,
}

// ---------------------------------------------------------------------------
// Content type helpers
// ---------------------------------------------------------------------------

/// Determine content type from file extension.
pub fn content_type_for_path(path: &str) -> &'static str {
    if path.ends_with(".m4s") || path.ends_with(".mp4") {
        "video/mp4"
    } else if path.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

/// Determine the object type label for metrics.
pub fn object_type_label(path: &str) -> &'static str {
    if path.ends_with(".m4s") {
        "segment"
    } else if path.ends_with("init.mp4") {
        "init_segment"
    } else if path.ends_with("media.m3u8") {
        "media_playlist"
    } else if path.ends_with("master.m3u8") {
        "master_playlist"
    } else if path.ends_with("metadata.json") {
        "metadata"
    } else {
        "other"
    }
}

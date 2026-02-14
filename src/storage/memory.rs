use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use chrono::Utc;
use tokio::sync::RwLock;

use crate::core::error::StorageError;
use crate::core::types::StreamMetadata;

use super::{GetObjectOutput, MediaStore, ObjectInfo};

// ---------------------------------------------------------------------------
// InMemoryMediaStore (from storage-and-delivery.md §4 — for testing)
// ---------------------------------------------------------------------------

/// In-memory storage backend for unit and integration tests.
///
/// Stores all objects in a `HashMap<String, StoredObject>` behind a `RwLock`.
/// No external dependencies required.
pub struct InMemoryMediaStore {
    objects: Arc<RwLock<HashMap<String, StoredObject>>>,
}

#[derive(Debug, Clone)]
struct StoredObject {
    data: Bytes,
    content_type: String,
    created_at: chrono::DateTime<Utc>,
}

impl InMemoryMediaStore {
    pub fn new() -> Self {
        Self {
            objects: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryMediaStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaStore for InMemoryMediaStore {
    async fn put_segment(
        &self,
        path: &str,
        data: Bytes,
        content_type: &str,
    ) -> Result<(), StorageError> {
        let mut objects = self.objects.write().await;
        objects.insert(
            path.to_string(),
            StoredObject {
                data,
                content_type: content_type.to_string(),
                created_at: Utc::now(),
            },
        );
        Ok(())
    }

    async fn put_manifest(&self, path: &str, content: &str) -> Result<(), StorageError> {
        let mut objects = self.objects.write().await;
        objects.insert(
            path.to_string(),
            StoredObject {
                data: Bytes::from(content.to_string()),
                content_type: "application/vnd.apple.mpegurl".to_string(),
                created_at: Utc::now(),
            },
        );
        Ok(())
    }

    async fn get_object(&self, path: &str) -> Result<GetObjectOutput, StorageError> {
        let objects = self.objects.read().await;
        let obj = objects.get(path).ok_or_else(|| StorageError::S3GetFailed {
            path: path.to_string(),
            reason: "not found".to_string(),
        })?;

        Ok(GetObjectOutput {
            body: obj.data.clone(),
            _content_length: obj.data.len() as u64,
            content_type: obj.content_type.clone(),
            _last_modified: obj.created_at,
            etag: format!("\"{}\"", obj.data.len()),
        })
    }

    async fn get_object_range(
        &self,
        path: &str,
        start: u64,
        end: u64,
    ) -> Result<GetObjectOutput, StorageError> {
        let objects = self.objects.read().await;
        let obj = objects.get(path).ok_or_else(|| StorageError::S3GetFailed {
            path: path.to_string(),
            reason: "not found".to_string(),
        })?;

        let start = start as usize;
        let end = (end as usize).min(obj.data.len().saturating_sub(1));

        if start >= obj.data.len() {
            return Err(StorageError::S3GetFailed {
                path: path.to_string(),
                reason: format!(
                    "range start {} exceeds object size {}",
                    start,
                    obj.data.len()
                ),
            });
        }

        let slice = obj.data.slice(start..=end);

        Ok(GetObjectOutput {
            body: slice,
            _content_length: (end - start + 1) as u64,
            content_type: obj.content_type.clone(),
            _last_modified: obj.created_at,
            etag: format!("\"{}\"", obj.data.len()),
        })
    }

    async fn delete_object(&self, path: &str) -> Result<(), StorageError> {
        let mut objects = self.objects.write().await;
        objects.remove(path);
        Ok(())
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<u64, StorageError> {
        let mut objects = self.objects.write().await;
        let keys_to_delete: Vec<String> = objects
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        let count = keys_to_delete.len() as u64;
        for key in keys_to_delete {
            objects.remove(&key);
        }
        Ok(count)
    }

    async fn list_objects(&self, prefix: &str) -> Result<Vec<ObjectInfo>, StorageError> {
        let objects = self.objects.read().await;
        let mut result: Vec<ObjectInfo> = objects
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| ObjectInfo {
                key: k.clone(),
                size: v.data.len() as u64,
                last_modified: v.created_at,
            })
            .collect();
        result.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(result)
    }

    async fn put_metadata(
        &self,
        path: &str,
        metadata: &StreamMetadata,
    ) -> Result<(), StorageError> {
        let json = serde_json::to_string(metadata).map_err(|e| StorageError::S3PutFailed {
            path: path.to_string(),
            reason: format!("failed to serialize metadata: {}", e),
        })?;
        let mut objects = self.objects.write().await;
        objects.insert(
            path.to_string(),
            StoredObject {
                data: Bytes::from(json),
                content_type: "application/json".to_string(),
                created_at: Utc::now(),
            },
        );
        Ok(())
    }

    async fn head_object(&self, path: &str) -> Result<super::ObjectMeta, StorageError> {
        let objects = self.objects.read().await;
        let obj = objects.get(path).ok_or_else(|| StorageError::S3GetFailed {
            path: path.to_string(),
            reason: "not found".to_string(),
        })?;
        Ok(super::ObjectMeta {
            content_length: obj.data.len() as u64,
            content_type: obj.content_type.clone(),
            last_modified: obj.created_at,
            etag: format!("\"{}\"", obj.data.len()),
        })
    }
}

#[cfg(test)]
impl InMemoryMediaStore {
    pub async fn object_count(&self) -> usize {
        self.objects.read().await.len()
    }

    pub async fn exists(&self, path: &str) -> bool {
        self.objects.read().await.contains_key(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_put_and_get_segment() {
        let store = InMemoryMediaStore::new();
        let data = Bytes::from(vec![0xAA; 1000]);

        store
            .put_segment("stream1/high/seg_000000.m4s", data.clone(), "video/mp4")
            .await
            .unwrap();

        let output = store
            .get_object("stream1/high/seg_000000.m4s")
            .await
            .unwrap();
        assert_eq!(output.body, data);
        assert_eq!(output.content_type, "video/mp4");
        assert_eq!(output._content_length, 1000);
    }

    #[tokio::test]
    async fn test_put_and_get_manifest() {
        let store = InMemoryMediaStore::new();
        let content = "#EXTM3U\n#EXT-X-VERSION:7\n";

        store
            .put_manifest("stream1/high/media.m3u8", content)
            .await
            .unwrap();

        let output = store.get_object("stream1/high/media.m3u8").await.unwrap();
        assert_eq!(output.body, Bytes::from(content));
        assert_eq!(output.content_type, "application/vnd.apple.mpegurl");
    }

    #[tokio::test]
    async fn test_get_nonexistent_returns_error() {
        let store = InMemoryMediaStore::new();
        let result = store.get_object("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_range_get() {
        let store = InMemoryMediaStore::new();
        let data = Bytes::from(vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);

        store
            .put_segment("test.m4s", data, "video/mp4")
            .await
            .unwrap();

        let output = store.get_object_range("test.m4s", 2, 5).await.unwrap();
        assert_eq!(output.body.as_ref(), &[2, 3, 4, 5]);
        assert_eq!(output._content_length, 4);
    }

    #[tokio::test]
    async fn test_delete_object() {
        let store = InMemoryMediaStore::new();
        store
            .put_segment("test.m4s", Bytes::from("data"), "video/mp4")
            .await
            .unwrap();

        assert!(store.exists("test.m4s").await);
        store.delete_object("test.m4s").await.unwrap();
        assert!(!store.exists("test.m4s").await);
    }

    #[tokio::test]
    async fn test_delete_prefix() {
        let store = InMemoryMediaStore::new();
        store
            .put_segment("stream1/high/seg_000000.m4s", Bytes::from("a"), "video/mp4")
            .await
            .unwrap();
        store
            .put_segment("stream1/high/seg_000001.m4s", Bytes::from("b"), "video/mp4")
            .await
            .unwrap();
        store
            .put_segment("stream2/high/seg_000000.m4s", Bytes::from("c"), "video/mp4")
            .await
            .unwrap();

        let deleted = store.delete_prefix("stream1/").await.unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(store.object_count().await, 1);
    }

    #[tokio::test]
    async fn test_list_objects() {
        let store = InMemoryMediaStore::new();
        store
            .put_segment("stream1/high/seg_000000.m4s", Bytes::from("a"), "video/mp4")
            .await
            .unwrap();
        store
            .put_segment("stream1/high/seg_000001.m4s", Bytes::from("b"), "video/mp4")
            .await
            .unwrap();
        store
            .put_segment("stream1/low/seg_000000.m4s", Bytes::from("c"), "video/mp4")
            .await
            .unwrap();

        let objects = store.list_objects("stream1/high/").await.unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].key, "stream1/high/seg_000000.m4s");
        assert_eq!(objects[1].key, "stream1/high/seg_000001.m4s");
    }
}

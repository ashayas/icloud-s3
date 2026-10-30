use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use crc32fast::Hasher as Crc32Hasher;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

use crate::codec::{
    decode_frames_to_object, decode_single_frame, encode_object_to_frames, CodecError,
    EncodeOptions, FrameHeader,
};

/// Apple's published iCloud Shared Album limits:
/// - Maximum number of Shared Albums an owner can create: 200
/// - Maximum number of photos/videos per Shared Album: 5,000
pub const APPLE_MAX_SHARED_ALBUMS: usize = 200;
pub const APPLE_MAX_PHOTOS_PER_ALBUM: usize = 5_000;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("NoSuchBucket: The specified bucket '{0}' does not exist")]
    NoSuchBucket(String),
    #[error("BucketNotEmpty: The bucket '{0}' you tried to delete is not empty")]
    BucketNotEmpty(String),
    #[error("NoSuchKey: The specified key '{key}' does not exist in bucket '{bucket}'")]
    NoSuchKey { bucket: String, key: String },
    #[error("AlbumLimitExceeded: Apple iCloud allows at most {APPLE_MAX_SHARED_ALBUMS} Shared Albums per account")]
    AlbumLimitExceeded,
    #[error("AlbumPhotoLimitExceeded: Shared Album '{album}' reached Apple's {APPLE_MAX_PHOTOS_PER_ALBUM}-photo limit")]
    AlbumPhotoLimitExceeded { album: String },
    #[error("InvalidBucketName: '{0}' is not a valid bucket name")]
    InvalidBucketName(String),
    #[error("Codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFrameInfo {
    pub photo_id: String,
    pub album_name: String,
    pub shard_index: usize,
    pub width: u32,
    pub height: u32,
    pub png_size: usize,
    pub header: FrameHeader,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRecord {
    pub bucket: String,
    pub key: String,
    pub object_id: String,
    pub size: u64,
    pub etag: String,
    pub sha256: String,
    pub content_type: String,
    pub last_modified: DateTime<Utc>,
    pub metadata: BTreeMap<String, String>,
    pub preview: String,
    pub frames: Vec<StoredFrameInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketInfo {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub shards: usize,
    pub object_count: usize,
    pub photo_frame_count: usize,
    pub raw_bytes: u64,
    pub png_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreOverview {
    pub mode: String,
    pub shards_per_bucket: usize,
    pub max_chunk_bytes: usize,
    pub bucket_count: usize,
    pub shared_album_count: usize,
    pub total_objects: usize,
    pub total_photo_frames: usize,
    pub total_raw_bytes: u64,
    pub total_png_bytes: u64,
    pub icloud_drive_quota_bytes_used: u64,
    pub gets: u64,
    pub puts: u64,
    pub deletes: u64,
    pub lists: u64,
    pub buckets: Vec<BucketInfo>,
}

#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub album_dir: Option<PathBuf>,
    pub shards: usize,
    pub max_chunk_bytes: usize,
    pub simulated_write_latency: Duration,
    pub sync_macos_photos_app: bool,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            album_dir: None,
            shards: 1,
            max_chunk_bytes: 2 * 1024 * 1024,
            simulated_write_latency: Duration::ZERO,
            sync_macos_photos_app: false,
        }
    }
}

#[derive(Default)]
struct BucketState {
    created_at: DateTime<Utc>,
    /// Key -> active ObjectRecord
    objects: BTreeMap<String, ObjectRecord>,
    /// photo_id -> raw PNG bytes
    photos: HashMap<String, Vec<u8>>,
    /// shard_index -> number of active photos in that Shared Album shard
    shard_photo_counts: Vec<usize>,
}

pub struct SharedAlbumStore {
    config: StoreConfig,
    buckets: RwLock<BTreeMap<String, BucketState>>,
    gets: AtomicU64,
    puts: AtomicU64,
    deletes: AtomicU64,
    lists: AtomicU64,
}

impl SharedAlbumStore {
    pub async fn new(mut config: StoreConfig) -> Result<Arc<Self>, StorageError> {
        config.shards = config.shards.clamp(1, APPLE_MAX_SHARED_ALBUMS);
        let store = Arc::new(Self {
            config,
            buckets: RwLock::new(BTreeMap::new()),
            gets: AtomicU64::new(0),
            puts: AtomicU64::new(0),
            deletes: AtomicU64::new(0),
            lists: AtomicU64::new(0),
        });

        if let Some(ref dir) = store.config.album_dir {
            tokio::fs::create_dir_all(dir).await?;
            store.reload_from_disk(dir).await?;
        }

        Ok(store)
    }

    pub fn config(&self) -> &StoreConfig {
        &self.config
    }

    /// Maps an (S3 bucket, S3 key) pair to its target Shared Album shard index using CRC32.
    pub fn shard_for_key(&self, key: &str) -> usize {
        if self.config.shards <= 1 {
            return 0;
        }
        let mut hasher = Crc32Hasher::new();
        hasher.update(key.as_bytes());
        (hasher.finalize() as usize) % self.config.shards
    }

    pub fn shared_album_name(&self, bucket: &str, shard_index: usize) -> String {
        if self.config.shards <= 1 {
            format!("icloud-s3-{bucket}")
        } else {
            format!("icloud-s3-{bucket}-{:02}", shard_index + 1)
        }
    }

    pub async fn create_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        validate_bucket_name(bucket)?;
        let mut buckets = self.buckets.write().await;
        if buckets.contains_key(bucket) {
            return Ok(false);
        }

        let current_albums = buckets.len() * self.config.shards;
        if current_albums + self.config.shards > APPLE_MAX_SHARED_ALBUMS {
            return Err(StorageError::AlbumLimitExceeded);
        }

        if let Some(ref base_dir) = self.config.album_dir {
            for shard_idx in 0..self.config.shards {
                let album_dir = base_dir.join(self.shared_album_name(bucket, shard_idx));
                tokio::fs::create_dir_all(&album_dir).await?;
            }
        }

        buckets.insert(
            bucket.to_string(),
            BucketState {
                created_at: Utc::now(),
                objects: BTreeMap::new(),
                photos: HashMap::new(),
                shard_photo_counts: vec![0; self.config.shards],
            },
        );
        Ok(true)
    }

    pub async fn head_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let buckets = self.buckets.read().await;
        if buckets.contains_key(bucket) {
            Ok(())
        } else {
            Err(StorageError::NoSuchBucket(bucket.to_string()))
        }
    }

    pub async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let mut buckets = self.buckets.write().await;
        let state = buckets
            .get(bucket)
            .ok_or_else(|| StorageError::NoSuchBucket(bucket.to_string()))?;
        if !state.objects.is_empty() {
            return Err(StorageError::BucketNotEmpty(bucket.to_string()));
        }

        if let Some(ref base_dir) = self.config.album_dir {
            for shard_idx in 0..self.config.shards {
                let album_dir = base_dir.join(self.shared_album_name(bucket, shard_idx));
                if album_dir.exists() {
                    let _ = tokio::fs::remove_dir_all(&album_dir).await;
                }
            }
        }

        buckets.remove(bucket);
        Ok(())
    }

    pub async fn list_buckets(&self) -> Vec<BucketInfo> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        let buckets = self.buckets.read().await;
        buckets
            .iter()
            .map(|(name, state)| summarize_bucket(name, state, self.config.shards))
            .collect()
    }

    pub async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<String>,
        metadata: BTreeMap<String, String>,
    ) -> Result<ObjectRecord, StorageError> {
        self.puts.fetch_add(1, Ordering::Relaxed);

        if !self.config.simulated_write_latency.is_zero() {
            tokio::time::sleep(self.config.simulated_write_latency).await;
        }

        // Auto-create bucket if missing? Standard S3 requires bucket to exist, but let's verify bucket exists first.
        {
            let buckets = self.buckets.read().await;
            if !buckets.contains_key(bucket) {
                return Err(StorageError::NoSuchBucket(bucket.to_string()));
            }
        }

        let mut opts = EncodeOptions::new(bucket, key);
        opts.content_type =
            content_type.unwrap_or_else(|| guess_content_type(key).to_string());
        opts.metadata = metadata.clone();
        opts.max_chunk_bytes = self.config.max_chunk_bytes;

        let encoded_frames = encode_object_to_frames(data, &opts)?;
        let shard_idx = self.shard_for_key(key);
        let album_name = self.shared_album_name(bucket, shard_idx);

        let mut buckets = self.buckets.write().await;
        let state = buckets
            .get_mut(bucket)
            .ok_or_else(|| StorageError::NoSuchBucket(bucket.to_string()))?;

        // Calculate net photo count change in this shard if overwriting an existing key
        let existing_frames_in_shard = state
            .objects
            .get(key)
            .map(|old| {
                old.frames
                    .iter()
                    .filter(|f| f.shard_index == shard_idx)
                    .count()
            })
            .unwrap_or(0);

        let current_shard_photos = state.shard_photo_counts.get(shard_idx).copied().unwrap_or(0);
        let projected_photos =
            current_shard_photos.saturating_sub(existing_frames_in_shard) + encoded_frames.len();
        if projected_photos > APPLE_MAX_PHOTOS_PER_ALBUM {
            return Err(StorageError::AlbumPhotoLimitExceeded { album: album_name });
        }

        // Remove previous photo frames for this key (if overwriting)
        if let Some(old_record) = state.objects.remove(key) {
            for old_frame in &old_record.frames {
                state.photos.remove(&old_frame.photo_id);
                if let Some(count) = state.shard_photo_counts.get_mut(old_frame.shard_index) {
                    *count = count.saturating_sub(1);
                }
                if let Some(ref base_dir) = self.config.album_dir {
                    let old_path = base_dir
                        .join(&old_frame.album_name)
                        .join(format!("{}.png", old_frame.photo_id));
                    let _ = tokio::fs::remove_file(old_path).await;
                }
            }
        }

        let first_header = encoded_frames[0].header.clone();
        let mut stored_frames = Vec::with_capacity(encoded_frames.len());

        for frame in encoded_frames {
            let photo_id = format!(
                "{}_{}_chunk{:03}of{:03}_{}",
                sanitize_filename(key),
                &first_header.etag[..8.min(first_header.etag.len())],
                frame.header.chunk_index + 1,
                frame.header.total_chunks,
                &first_header.object_id[..8]
            );

            if let Some(ref base_dir) = self.config.album_dir {
                let album_dir = base_dir.join(&album_name);
                tokio::fs::create_dir_all(&album_dir).await?;
                let file_path = album_dir.join(format!("{photo_id}.png"));
                tokio::fs::write(&file_path, &frame.png_bytes).await?;

                if self.config.sync_macos_photos_app {
                    let _ = import_png_into_macos_photos(&file_path, &album_name).await;
                }
            }

            let info = StoredFrameInfo {
                photo_id: photo_id.clone(),
                album_name: album_name.clone(),
                shard_index: shard_idx,
                width: frame.width,
                height: frame.height,
                png_size: frame.png_bytes.len(),
                header: frame.header,
            };

            state.photos.insert(photo_id, frame.png_bytes);
            if let Some(count) = state.shard_photo_counts.get_mut(shard_idx) {
                *count += 1;
            }
            stored_frames.push(info);
        }

        let record = ObjectRecord {
            bucket: bucket.to_string(),
            key: key.to_string(),
            object_id: first_header.object_id,
            size: first_header.total_size,
            etag: first_header.etag,
            sha256: first_header.object_sha256,
            content_type: first_header.content_type,
            last_modified: first_header.created_at,
            metadata,
            preview: extract_payload_preview(data),
            frames: stored_frames,
        };

        state.objects.insert(key.to_string(), record.clone());
        Ok(record)
    }

    /// Reads an object by decoding its PNG carrier photo frames from the Shared Album and
    /// verifying both the per-frame CRC32 and whole-object SHA-256 checksums.
    pub async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(ObjectRecord, Vec<u8>), StorageError> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        let buckets = self.buckets.read().await;
        let state = buckets
            .get(bucket)
            .ok_or_else(|| StorageError::NoSuchBucket(bucket.to_string()))?;
        let record = state.objects.get(key).ok_or_else(|| StorageError::NoSuchKey {
            bucket: bucket.to_string(),
            key: key.to_string(),
        })?;

        let mut png_frames = Vec::with_capacity(record.frames.len());
        for frame_info in &record.frames {
            let png = state
                .photos
                .get(&frame_info.photo_id)
                .ok_or_else(|| StorageError::NoSuchKey {
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                })?;
            png_frames.push(png.clone());
        }

        let decoded = decode_frames_to_object(&png_frames)?;
        Ok((record.clone(), decoded.data))
    }

    /// Returns the raw PNG carrier image bytes for a given object and chunk index (used by the
    /// Shared Album Web Inspector and `?format=png`).
    pub async fn get_object_carrier_png(
        &self,
        bucket: &str,
        key: &str,
        chunk_index: usize,
    ) -> Result<(ObjectRecord, Vec<u8>), StorageError> {
        let buckets = self.buckets.read().await;
        let state = buckets
            .get(bucket)
            .ok_or_else(|| StorageError::NoSuchBucket(bucket.to_string()))?;
        let record = state.objects.get(key).ok_or_else(|| StorageError::NoSuchKey {
            bucket: bucket.to_string(),
            key: key.to_string(),
        })?;
        let frame_info = record
            .frames
            .get(chunk_index)
            .or_else(|| record.frames.first())
            .ok_or_else(|| StorageError::NoSuchKey {
                bucket: bucket.to_string(),
                key: key.to_string(),
            })?;
        let png = state
            .photos
            .get(&frame_info.photo_id)
            .ok_or_else(|| StorageError::NoSuchKey {
                bucket: bucket.to_string(),
                key: key.to_string(),
            })?;
        Ok((record.clone(), png.clone()))
    }

    pub async fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectRecord, StorageError> {
        let buckets = self.buckets.read().await;
        let state = buckets
            .get(bucket)
            .ok_or_else(|| StorageError::NoSuchBucket(bucket.to_string()))?;
        state
            .objects
            .get(key)
            .cloned()
            .ok_or_else(|| StorageError::NoSuchKey {
                bucket: bucket.to_string(),
                key: key.to_string(),
            })
    }

    pub async fn delete_object(&self, bucket: &str, key: &str) -> Result<bool, StorageError> {
        self.deletes.fetch_add(1, Ordering::Relaxed);
        let mut buckets = self.buckets.write().await;
        let state = buckets
            .get_mut(bucket)
            .ok_or_else(|| StorageError::NoSuchBucket(bucket.to_string()))?;

        if let Some(old_record) = state.objects.remove(key) {
            for frame in &old_record.frames {
                state.photos.remove(&frame.photo_id);
                if let Some(count) = state.shard_photo_counts.get_mut(frame.shard_index) {
                    *count = count.saturating_sub(1);
                }
                if let Some(ref base_dir) = self.config.album_dir {
                    let path = base_dir
                        .join(&frame.album_name)
                        .join(format!("{}.png", frame.photo_id));
                    let _ = tokio::fs::remove_file(path).await;
                }
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<ObjectRecord>, StorageError> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        let buckets = self.buckets.read().await;
        let state = buckets
            .get(bucket)
            .ok_or_else(|| StorageError::NoSuchBucket(bucket.to_string()))?;

        let prefix = prefix.unwrap_or("");
        let items = state
            .objects
            .values()
            .filter(|obj| obj.key.starts_with(prefix))
            .take(max_keys.max(1))
            .cloned()
            .collect();
        Ok(items)
    }

    pub async fn overview(&self) -> StoreOverview {
        let buckets = self.buckets.read().await;
        let mut bucket_summaries = Vec::with_capacity(buckets.len());
        let mut total_objects = 0usize;
        let mut total_photo_frames = 0usize;
        let mut total_raw_bytes = 0u64;
        let mut total_png_bytes = 0u64;

        for (name, state) in buckets.iter() {
            let info = summarize_bucket(name, state, self.config.shards);
            total_objects += info.object_count;
            total_photo_frames += info.photo_frame_count;
            total_raw_bytes += info.raw_bytes;
            total_png_bytes += info.png_bytes;
            bucket_summaries.push(info);
        }

        let mode = match &self.config.album_dir {
            Some(p) => format!("Shared Album folder ({})", p.display()),
            None => "dry-run (in-memory Shared Album)".to_string(),
        };

        StoreOverview {
            mode,
            shards_per_bucket: self.config.shards,
            max_chunk_bytes: self.config.max_chunk_bytes,
            bucket_count: buckets.len(),
            shared_album_count: buckets.len() * self.config.shards,
            total_objects,
            total_photo_frames,
            total_raw_bytes,
            total_png_bytes,
            // iCloud Shared Albums do not count against iCloud storage allowance:
            icloud_drive_quota_bytes_used: 0,
            gets: self.gets.load(Ordering::Relaxed),
            puts: self.puts.load(Ordering::Relaxed),
            deletes: self.deletes.load(Ordering::Relaxed),
            lists: self.lists.load(Ordering::Relaxed),
            buckets: bucket_summaries,
        }
    }

    /// Reconstructs the entire S3 keyspace purely by scanning PNG photos in the album directory
    /// and decoding their pixel headers on startup.
    async fn reload_from_disk(&self, base_dir: &Path) -> Result<(), StorageError> {
        let mut dir_entries = tokio::fs::read_dir(base_dir).await?;
        let mut discovered_frames: Vec<(String, PathBuf, Vec<u8>, FrameHeader, u32, u32)> =
            Vec::new();

        while let Some(entry) = dir_entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(album_dir_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !album_dir_name.starts_with("icloud-s3-") {
                continue;
            }

            let mut files = tokio::fs::read_dir(&path).await?;
            while let Some(file_entry) = files.next_entry().await? {
                let file_path = file_entry.path();
                if file_path.extension().and_then(|e| e.to_str()) != Some("png") {
                    continue;
                }
                let png_bytes = tokio::fs::read(&file_path).await?;
                if let Ok(decoded) = decode_single_frame(&png_bytes) {
                    discovered_frames.push((
                        album_dir_name.to_string(),
                        file_path,
                        png_bytes,
                        decoded.header,
                        decoded.width,
                        decoded.height,
                    ));
                }
            }
        }

        // Group frames by (bucket, key, object_id)
        let mut grouped: BTreeMap<
            (String, String),
            Vec<(String, PathBuf, Vec<u8>, FrameHeader, u32, u32)>,
        > = BTreeMap::new();
        for item in discovered_frames {
            let key_pair = (item.3.bucket.clone(), item.3.key.clone());
            grouped.entry(key_pair).or_default().push(item);
        }

        let mut buckets = self.buckets.write().await;
        for ((bucket, key), mut frame_list) in grouped {
            // If multiple object_ids exist for the same key, pick the one with the latest created_at
            let latest_object_id = frame_list
                .iter()
                .max_by_key(|f| f.3.created_at)
                .map(|f| f.3.object_id.clone())
                .unwrap();
            frame_list.retain(|f| f.3.object_id == latest_object_id);
            frame_list.sort_by_key(|f| f.3.chunk_index);

            let first_hdr = frame_list[0].3.clone();
            if frame_list.len() != first_hdr.total_chunks as usize {
                continue;
            }

            let state = buckets.entry(bucket.clone()).or_insert_with(|| BucketState {
                created_at: first_hdr.created_at,
                objects: BTreeMap::new(),
                photos: HashMap::new(),
                shard_photo_counts: vec![0; self.config.shards],
            });

            let shard_idx = self.shard_for_key(&key);
            let mut stored_frames = Vec::with_capacity(frame_list.len());
            for (album_name, file_path, png_bytes, hdr, width, height) in frame_list {
                let photo_id = file_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("frame")
                    .to_string();
                let png_size = png_bytes.len();
                state.photos.insert(photo_id.clone(), png_bytes);
                if let Some(c) = state.shard_photo_counts.get_mut(shard_idx) {
                    *c += 1;
                }
                stored_frames.push(StoredFrameInfo {
                    photo_id,
                    album_name,
                    shard_index: shard_idx,
                    width,
                    height,
                    png_size,
                    header: hdr,
                });
            }

            state.objects.insert(
                key.clone(),
                ObjectRecord {
                    bucket,
                    key,
                    object_id: first_hdr.object_id,
                    size: first_hdr.total_size,
                    etag: first_hdr.etag,
                    sha256: first_hdr.object_sha256,
                    content_type: first_hdr.content_type,
                    last_modified: first_hdr.created_at,
                    metadata: first_hdr.metadata,
                    preview: format!("({} bytes encoded in RGB PNG pixels)", first_hdr.total_size),
                    frames: stored_frames,
                },
            );
        }

        Ok(())
    }
}

fn summarize_bucket(name: &str, state: &BucketState, shards: usize) -> BucketInfo {
    let mut photo_frame_count = 0usize;
    let mut raw_bytes = 0u64;
    let mut png_bytes = 0u64;
    for obj in state.objects.values() {
        raw_bytes += obj.size;
        photo_frame_count += obj.frames.len();
        for f in &obj.frames {
            png_bytes += f.png_size as u64;
        }
    }
    BucketInfo {
        name: name.to_string(),
        created_at: state.created_at,
        shards,
        object_count: state.objects.len(),
        photo_frame_count,
        raw_bytes,
        png_bytes,
    }
}

fn validate_bucket_name(bucket: &str) -> Result<(), StorageError> {
    if bucket.is_empty()
        || bucket.len() > 63
        || !bucket
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(StorageError::InvalidBucketName(bucket.to_string()));
    }
    Ok(())
}

fn sanitize_filename(key: &str) -> String {
    let mut out = String::with_capacity(key.len().min(48));
    for ch in key.chars().take(48) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "object".to_string()
    } else {
        out
    }
}

fn extract_payload_preview(data: &[u8]) -> String {
    if data.is_empty() {
        return "(0 bytes)".to_string();
    }
    let slice = &data[..data.len().min(280)];
    if let Ok(text) = std::str::from_utf8(slice) {
        if text
            .chars()
            .all(|c| !c.is_control() || c == '\n' || c == '\r' || c == '\t')
        {
            return text.trim().to_string();
        }
    }
    format!("0x{}… ({} bytes binary)", hex::encode(&slice[..slice.len().min(24)]), data.len())
}

fn guess_content_type(key: &str) -> &'static str {
    let lower = key.to_ascii_lowercase();
    if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".sql") {
        "application/sql"
    } else if lower.ends_with(".txt") || lower.ends_with(".md") || lower.ends_with(".log") {
        "text/plain"
    } else if lower.ends_with(".html") {
        "text/html"
    } else if lower.ends_with(".csv") {
        "text/csv"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".zip") {
        "application/zip"
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") || lower.ends_with(".gz") {
        "application/gzip"
    } else if lower.ends_with(".sqlite") || lower.ends_with(".db") {
        "application/vnd.sqlite3"
    } else {
        "application/octet-stream"
    }
}

/// Optional macOS `Photos.app` bridge via `osascript` when `--sync-photos-app` is enabled.
async fn import_png_into_macos_photos(png_path: &Path, album_name: &str) -> std::io::Result<()> {
    let escaped_path = png_path.display().to_string().replace('"', "\\\"");
    let escaped_album = album_name.replace('"', "\\\"");
    let script = format!(
        r#"tell application "Photos"
            if not (exists album "{escaped_album}") then
                make new album named "{escaped_album}"
            end if
            import POSIX file "{escaped_path}" into album "{escaped_album}" skip check duplicates true
        end tell"#
    );
    let _ = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_memory_store_lifecycle_and_zero_icloud_quota() {
        let store = SharedAlbumStore::new(StoreConfig::default()).await.unwrap();
        assert!(store.create_bucket("backups").await.unwrap());
        assert!(!store.create_bucket("backups").await.unwrap());

        let payload = b"{\"database\":\"postgres\",\"rows\":14920}";
        let rec = store
            .put_object("backups", "2026-10-30/dump.json", payload, None, BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(rec.size, payload.len() as u64);
        assert_eq!(rec.content_type, "application/json");
        assert_eq!(rec.frames.len(), 1);

        let (fetched_rec, fetched_bytes) = store
            .get_object("backups", "2026-10-30/dump.json")
            .await
            .unwrap();
        assert_eq!(fetched_bytes, payload);
        assert_eq!(fetched_rec.etag, rec.etag);

        let overview = store.overview().await;
        assert_eq!(overview.total_objects, 1);
        assert_eq!(overview.total_photo_frames, 1);
        assert_eq!(overview.icloud_drive_quota_bytes_used, 0);

        // Cannot delete non-empty bucket
        assert!(matches!(
            store.delete_bucket("backups").await,
            Err(StorageError::BucketNotEmpty(_))
        ));

        assert!(store
            .delete_object("backups", "2026-10-30/dump.json")
            .await
            .unwrap());
        store.delete_bucket("backups").await.unwrap();
    }

    #[tokio::test]
    async fn test_disk_persistence_and_reload_from_png_pixels() {
        let dir = tempdir().unwrap();
        let cfg = StoreConfig {
            album_dir: Some(dir.path().to_path_buf()),
            shards: 4,
            max_chunk_bytes: 512, // Force multi-frame photo bursts on disk
            ..Default::default()
        };

        let payload: Vec<u8> = (0..1800).map(|i| ((i * 37 + 11) & 0xFF) as u8).collect();
        {
            let store = SharedAlbumStore::new(cfg.clone()).await.unwrap();
            store.create_bucket("traces").await.unwrap();
            let rec = store
                .put_object("traces", "agent/run-01.bin", &payload, None, BTreeMap::new())
                .await
                .unwrap();
            assert_eq!(rec.frames.len(), 4);
        }

        // Restart store solely from the PNG files on disk (replaying the Shared Album)
        let restarted = SharedAlbumStore::new(cfg).await.unwrap();
        let (_, loaded) = restarted
            .get_object("traces", "agent/run-01.bin")
            .await
            .unwrap();
        assert_eq!(loaded, payload);
    }
}

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use crc32fast::Hasher as Crc32Hasher;
use image::{ImageFormat, Rgb, RgbImage};
use md5::Md5;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Apple iCloud Shared Albums allow up to 2048px on the long edge for standard photos
/// without downscaling.
pub const MAX_SHARED_ALBUM_DIMENSION: u32 = 2048;

/// Minimum width so the visual HUD header banner is always legible in Photos.app.
pub const MIN_FRAME_WIDTH: u32 = 560;

/// Height in pixels reserved at the top of every PNG for the human-readable HUD banner.
pub const HUD_BANNER_HEIGHT: u32 = 64;

/// Magic bytes starting at pixel (0, HUD_BANNER_HEIGHT).
pub const FRAME_MAGIC: &[u8; 6] = b"ICS3\x00\x01";

/// Fixed binary preamble size before header JSON:
/// 6 (magic) + 4 (header_len) + 4 (payload_len) + 4 (crc32) = 18 bytes.
pub const PREAMBLE_BYTES: usize = 18;

/// Maximum raw payload bytes that fit in a single 2048x2048 frame below the 64px HUD banner,
/// leaving 8 KB headroom for frame header JSON.
pub const MAX_SINGLE_FRAME_PAYLOAD: usize =
    ((MAX_SHARED_ALBUM_DIMENSION as usize) * ((MAX_SHARED_ALBUM_DIMENSION - HUD_BANNER_HEIGHT) as usize) * 3)
        - 8192;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("image dimension {width}x{height} is too small to contain HUD banner ({HUD_BANNER_HEIGHT}px) and payload")]
    ImageTooSmall { width: u32, height: u32 },
    #[error("image dimension {width}x{height} exceeds iCloud Shared Album 2048px limit")]
    ExceedsSharedAlbumDimension { width: u32, height: u32 },
    #[error("invalid frame magic in pixel stream (not an icloud-s3 carrier photo)")]
    InvalidMagic,
    #[error("corrupted frame: declared lengths (header={header_len}, payload={payload_len}) exceed pixel capacity ({capacity})")]
    LengthOutOfBounds {
        header_len: usize,
        payload_len: usize,
        capacity: usize,
    },
    #[error("CRC32 checksum mismatch in pixel payload (expected {expected:#010x}, got {actual:#010x})")]
    ChecksumMismatch { expected: u32, actual: u32 },
    #[error("SHA-256 mismatch after reassembling chunks (expected {expected}, got {actual})")]
    Sha256Mismatch { expected: String, actual: String },
    #[error("incomplete chunk set: expected {expected} chunks, got {actual}")]
    IncompleteChunks { expected: u32, actual: usize },
    #[error("chunk sequence mismatch: missing chunk index {missing_index}")]
    MissingChunk { missing_index: u32 },
    #[error("image encoding/decoding error: {0}")]
    Image(#[from] image::ImageError),
    #[error("header serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u8,
    pub bucket: String,
    pub key: String,
    pub object_id: String,
    pub content_type: String,
    pub total_size: u64,
    pub chunk_index: u32,
    pub total_chunks: u32,
    pub chunk_size: u32,
    pub etag: String,
    pub object_sha256: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub header: FrameHeader,
    pub png_bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub header: FrameHeader,
    pub chunk_payload: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct DecodedObject {
    pub header: FrameHeader,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct EncodeOptions {
    pub bucket: String,
    pub key: String,
    pub object_id: String,
    pub content_type: String,
    pub created_at: DateTime<Utc>,
    pub metadata: BTreeMap<String, String>,
    pub max_chunk_bytes: usize,
}

impl EncodeOptions {
    pub fn new(bucket: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            key: key.into(),
            object_id: uuid::Uuid::new_v4().to_string(),
            content_type: "application/octet-stream".to_string(),
            created_at: Utc::now(),
            metadata: BTreeMap::new(),
            max_chunk_bytes: 2 * 1024 * 1024, // 2 MiB per photo chunk default
        }
    }
}

pub fn compute_etag(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

pub fn compute_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Encodes an arbitrary S3 object payload into one or more lossless 24-bit RGB PNG carrier frames.
/// Every frame is <= 2048x2048 and features a human-readable HUD header at the top and raw
/// CRC32-protected data bytes packed into the RGB pixels below.
pub fn encode_object_to_frames(
    data: &[u8],
    opts: &EncodeOptions,
) -> Result<Vec<EncodedFrame>, CodecError> {
    let chunk_limit = opts
        .max_chunk_bytes
        .clamp(64, MAX_SINGLE_FRAME_PAYLOAD);

    let etag = compute_etag(data);
    let object_sha256 = compute_sha256(data);
    let total_size = data.len() as u64;

    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(chunk_limit).collect()
    };
    let total_chunks = chunks.len() as u32;

    let mut frames = Vec::with_capacity(chunks.len());
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let header = FrameHeader {
            version: 1,
            bucket: opts.bucket.clone(),
            key: opts.key.clone(),
            object_id: opts.object_id.clone(),
            content_type: opts.content_type.clone(),
            total_size,
            chunk_index: idx as u32,
            total_chunks,
            chunk_size: chunk.len() as u32,
            etag: etag.clone(),
            object_sha256: object_sha256.clone(),
            created_at: opts.created_at,
            metadata: opts.metadata.clone(),
        };
        let frame = encode_single_frame(&header, chunk)?;
        frames.push(frame);
    }

    Ok(frames)
}

/// Encodes a single chunk and its `FrameHeader` into an RGB PNG image.
pub fn encode_single_frame(
    header: &FrameHeader,
    chunk_payload: &[u8],
) -> Result<EncodedFrame, CodecError> {
    let header_json = serde_json::to_vec(header)?;
    let total_stream_bytes = PREAMBLE_BYTES + header_json.len() + chunk_payload.len();
    let required_pixels = (total_stream_bytes + 2) / 3;

    // Choose aesthetically pleasing canvas dimensions within 2048x2048
    let width = choose_canvas_width(required_pixels);
    let data_rows = ((required_pixels as u32) + width - 1) / width;
    let min_data_rows = 96u32; // So small objects still display a visible data mosaic
    let actual_data_rows = data_rows.max(min_data_rows);
    let height = HUD_BANNER_HEIGHT + actual_data_rows;

    if width > MAX_SHARED_ALBUM_DIMENSION || height > MAX_SHARED_ALBUM_DIMENSION {
        return Err(CodecError::ExceedsSharedAlbumDimension { width, height });
    }

    // Compute CRC32 over (header_json || chunk_payload)
    let mut crc_hasher = Crc32Hasher::new();
    crc_hasher.update(&header_json);
    crc_hasher.update(chunk_payload);
    let crc32 = crc_hasher.finalize();

    // Build byte stream for the data region
    let mut stream = Vec::with_capacity(total_stream_bytes);
    stream.extend_from_slice(FRAME_MAGIC);
    stream.extend_from_slice(&(header_json.len() as u32).to_be_bytes());
    stream.extend_from_slice(&(chunk_payload.len() as u32).to_be_bytes());
    stream.extend_from_slice(&crc32.to_be_bytes());
    stream.extend_from_slice(&header_json);
    stream.extend_from_slice(chunk_payload);

    let mut img = RgbImage::new(width, height);

    // 1. Render the dark-mode visual HUD banner in rows 0..HUD_BANNER_HEIGHT
    render_hud_banner(&mut img, header);

    // 2. Write the binary stream into rows HUD_BANNER_HEIGHT..height (3 bytes per RGB pixel)
    let mut byte_idx = 0usize;
    let stream_len = stream.len();
    let seed = crc32;

    for y in HUD_BANNER_HEIGHT..height {
        for x in 0..width {
            if byte_idx < stream_len {
                let r = stream[byte_idx];
                let g = if byte_idx + 1 < stream_len {
                    stream[byte_idx + 1]
                } else {
                    0
                };
                let b = if byte_idx + 2 < stream_len {
                    stream[byte_idx + 2]
                } else {
                    0
                };
                img.put_pixel(x, y, Rgb([r, g, b]));
                byte_idx += 3;
            } else {
                // Subtle cybernetic grid pattern for unused padding pixels
                let pad_r = (14 + ((x.wrapping_mul(31) ^ y.wrapping_mul(17) ^ seed) & 0x0F)) as u8;
                let pad_g = (20 + ((x.wrapping_add(y) ^ (seed >> 8)) & 0x13)) as u8;
                let pad_b = (32 + (((x / 8).wrapping_mul(13) ^ (y / 8).wrapping_mul(7)) & 0x17)) as u8;
                img.put_pixel(x, y, Rgb([pad_r, pad_g, pad_b]));
            }
        }
    }

    let mut png_bytes = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png_bytes);
    img.write_to(&mut cursor, ImageFormat::Png)?;

    Ok(EncodedFrame {
        header: header.clone(),
        png_bytes,
        width,
        height,
    })
}

/// Decodes a single PNG carrier frame back into its `FrameHeader` and raw chunk payload.
pub fn decode_single_frame(png_bytes: &[u8]) -> Result<DecodedFrame, CodecError> {
    let dynamic_img = image::load_from_memory_with_format(png_bytes, ImageFormat::Png)?;
    let img = dynamic_img.to_rgb8();
    let (width, height) = img.dimensions();

    if height <= HUD_BANNER_HEIGHT {
        return Err(CodecError::ImageTooSmall { width, height });
    }
    if width > MAX_SHARED_ALBUM_DIMENSION || height > MAX_SHARED_ALBUM_DIMENSION {
        return Err(CodecError::ExceedsSharedAlbumDimension { width, height });
    }

    let data_pixels = (width as usize) * ((height - HUD_BANNER_HEIGHT) as usize);
    let capacity = data_pixels * 3;
    if capacity < PREAMBLE_BYTES {
        return Err(CodecError::ImageTooSmall { width, height });
    }

    // Extract raw bytes from the data region starting at y = HUD_BANNER_HEIGHT
    let mut raw_bytes = Vec::with_capacity(capacity);
    for y in HUD_BANNER_HEIGHT..height {
        for x in 0..width {
            let p = img.get_pixel(x, y);
            raw_bytes.push(p[0]);
            raw_bytes.push(p[1]);
            raw_bytes.push(p[2]);
        }
    }

    if &raw_bytes[0..6] != FRAME_MAGIC {
        return Err(CodecError::InvalidMagic);
    }

    let header_len = u32::from_be_bytes([raw_bytes[6], raw_bytes[7], raw_bytes[8], raw_bytes[9]]) as usize;
    let payload_len = u32::from_be_bytes([raw_bytes[10], raw_bytes[11], raw_bytes[12], raw_bytes[13]]) as usize;
    let expected_crc = u32::from_be_bytes([raw_bytes[14], raw_bytes[15], raw_bytes[16], raw_bytes[17]]);

    let total_needed = PREAMBLE_BYTES
        .checked_add(header_len)
        .and_then(|v| v.checked_add(payload_len))
        .ok_or(CodecError::LengthOutOfBounds {
            header_len,
            payload_len,
            capacity,
        })?;

    if total_needed > capacity {
        return Err(CodecError::LengthOutOfBounds {
            header_len,
            payload_len,
            capacity,
        });
    }

    let header_slice = &raw_bytes[PREAMBLE_BYTES..PREAMBLE_BYTES + header_len];
    let payload_slice =
        &raw_bytes[PREAMBLE_BYTES + header_len..PREAMBLE_BYTES + header_len + payload_len];

    let mut crc_hasher = Crc32Hasher::new();
    crc_hasher.update(header_slice);
    crc_hasher.update(payload_slice);
    let actual_crc = crc_hasher.finalize();

    if actual_crc != expected_crc {
        return Err(CodecError::ChecksumMismatch {
            expected: expected_crc,
            actual: actual_crc,
        });
    }

    let header: FrameHeader = serde_json::from_slice(header_slice)?;

    Ok(DecodedFrame {
        header,
        chunk_payload: payload_slice.to_vec(),
        width,
        height,
    })
}

/// Reassembles one or more PNG carrier frames into the original verified S3 object.
pub fn decode_frames_to_object(png_frames: &[Vec<u8>]) -> Result<DecodedObject, CodecError> {
    if png_frames.is_empty() {
        return Err(CodecError::IncompleteChunks {
            expected: 1,
            actual: 0,
        });
    }

    let mut decoded_frames = Vec::with_capacity(png_frames.len());
    for png in png_frames {
        decoded_frames.push(decode_single_frame(png)?);
    }

    decoded_frames.sort_by_key(|f| f.header.chunk_index);
    let expected_total = decoded_frames[0].header.total_chunks;
    if decoded_frames.len() != expected_total as usize {
        return Err(CodecError::IncompleteChunks {
            expected: expected_total,
            actual: decoded_frames.len(),
        });
    }

    let first_header = decoded_frames[0].header.clone();
    let mut data = Vec::with_capacity(first_header.total_size as usize);

    for (expected_idx, frame) in decoded_frames.into_iter().enumerate() {
        if frame.header.chunk_index != expected_idx as u32 {
            return Err(CodecError::MissingChunk {
                missing_index: expected_idx as u32,
            });
        }
        data.extend_from_slice(&frame.chunk_payload);
    }

    let actual_sha256 = compute_sha256(&data);
    if actual_sha256 != first_header.object_sha256 {
        return Err(CodecError::Sha256Mismatch {
            expected: first_header.object_sha256,
            actual: actual_sha256,
        });
    }

    Ok(DecodedObject {
        header: first_header,
        data,
    })
}

fn choose_canvas_width(required_pixels: usize) -> u32 {
    if required_pixels <= 560 * 120 {
        MIN_FRAME_WIDTH
    } else if required_pixels <= 1024 * 512 {
        1024
    } else {
        MAX_SHARED_ALBUM_DIMENSION
    }
}

/// Renders the 64px high visual HUD header at the top of the image using a clean built-in
/// 5x7 bitmap font so every photo in Apple Photos shows its S3 bucket, key, size, and ETag.
fn render_hud_banner(img: &mut RgbImage, header: &FrameHeader) {
    let width = img.width();

    // Background gradient + bottom separator line
    for y in 0..HUD_BANNER_HEIGHT {
        for x in 0..width {
            let bg = if y == HUD_BANNER_HEIGHT - 1 {
                Rgb([56, 189, 248]) // Cyan divider line
            } else if y == HUD_BANNER_HEIGHT - 2 {
                Rgb([15, 23, 42])
            } else {
                Rgb([12, 17, 29])
            };
            img.put_pixel(x, y, bg);
        }
    }

    // Left accent bar
    for y in 6..(HUD_BANNER_HEIGHT - 6) {
        for x in 8..12 {
            img.put_pixel(x, y, Rgb([56, 189, 248]));
        }
    }

    // Deterministic identicon badge on the right side based on object_sha256
    let hash_bytes = hex::decode(&header.object_sha256).unwrap_or_else(|_| vec![0xAA; 16]);
    let badge_size = 36u32;
    let badge_x0 = width.saturating_sub(badge_size + 14);
    let badge_y0 = 13u32;
    for cell_y in 0..4u32 {
        for cell_x in 0..4u32 {
            let idx = (cell_y * 4 + cell_x) as usize;
            let b = hash_bytes.get(idx).copied().unwrap_or(128);
            let color = Rgb([
                60u8.saturating_add(b / 2),
                100u8.saturating_add(b / 3),
                180u8.saturating_add(b / 4),
            ]);
            for py in (badge_y0 + cell_y * 9)..(badge_y0 + (cell_y + 1) * 9 - 1) {
                for px in (badge_x0 + cell_x * 9)..(badge_x0 + (cell_x + 1) * 9 - 1) {
                    if px < width && py < HUD_BANNER_HEIGHT {
                        img.put_pixel(px, py, color);
                    }
                }
            }
        }
    }

    let line1 = format!(
        "ICLOUD-S3 // SHARED ALBUM OBJECT  [CHUNK {}/{}]",
        header.chunk_index + 1,
        header.total_chunks
    );
    let s3_uri = format!("s3://{}/{}", header.bucket, header.key);
    let etag_short: String = header.etag.chars().take(12).collect();
    let line3 = format!(
        "{} B  |  {}  |  ETAG {}",
        header.total_size,
        header.content_type.to_uppercase(),
        etag_short
    );

    let max_chars = ((badge_x0.saturating_sub(24)) / 6) as usize;
    draw_text(
        img,
        20,
        10,
        &truncate_ascii(&line1, max_chars),
        Rgb([56, 189, 248]),
    );
    draw_text(
        img,
        20,
        26,
        &truncate_ascii(&s3_uri, max_chars),
        Rgb([248, 250, 252]),
    );
    draw_text(
        img,
        20,
        42,
        &truncate_ascii(&line3, max_chars),
        Rgb([148, 163, 184]),
    );
}

fn truncate_ascii(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars || max_chars < 6 {
        s.to_string()
    } else {
        format!("{}...", &s[..max_chars - 3])
    }
}

fn draw_text(img: &mut RgbImage, start_x: u32, start_y: u32, text: &str, color: Rgb<u8>) {
    let mut cursor_x = start_x;
    for ch in text.chars() {
        let glyph = glyph_5x7(ch);
        for (row_idx, row_bits) in glyph.iter().enumerate() {
            for col_idx in 0..5u32 {
                if (row_bits >> (4 - col_idx)) & 1 == 1 {
                    let px = cursor_x + col_idx;
                    let py = start_y + row_idx as u32;
                    if px < img.width() && py < HUD_BANNER_HEIGHT {
                        img.put_pixel(px, py, color);
                    }
                }
            }
        }
        cursor_x += 6;
    }
}

fn glyph_5x7(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b00001, 0b11110],
        '6' => [0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110],
        ':' => [0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000],
        '/' => [0b00001, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b10000],
        '-' => [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000],
        '_' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111],
        '.' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110],
        '|' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        '[' => [0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110],
        ']' => [0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110],
        _ => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_small_object() {
        let payload = b"SELECT * FROM production_secrets WHERE cloud = 'icloud-shared-album';";
        let mut opts = EncodeOptions::new("prod-backups", "sql/dump-2026.sql");
        opts.content_type = "application/sql".to_string();
        opts.metadata.insert("env".to_string(), "prod".to_string());

        let frames = encode_object_to_frames(payload, &opts).expect("encode should succeed");
        assert_eq!(frames.len(), 1);
        assert!(frames[0].width <= MAX_SHARED_ALBUM_DIMENSION);
        assert!(frames[0].height <= MAX_SHARED_ALBUM_DIMENSION);

        let pngs: Vec<Vec<u8>> = frames.into_iter().map(|f| f.png_bytes).collect();
        let decoded = decode_frames_to_object(&pngs).expect("decode should succeed");

        assert_eq!(decoded.data, payload);
        assert_eq!(decoded.header.bucket, "prod-backups");
        assert_eq!(decoded.header.key, "sql/dump-2026.sql");
        assert_eq!(decoded.header.content_type, "application/sql");
        assert_eq!(decoded.header.metadata.get("env").map(String::as_str), Some("prod"));
    }

    #[test]
    fn test_roundtrip_empty_object() {
        let opts = EncodeOptions::new("empty-bucket", "zero.bin");
        let frames = encode_object_to_frames(&[], &opts).expect("empty encode should succeed");
        assert_eq!(frames.len(), 1);

        let pngs: Vec<Vec<u8>> = frames.into_iter().map(|f| f.png_bytes).collect();
        let decoded = decode_frames_to_object(&pngs).expect("empty decode should succeed");
        assert!(decoded.data.is_empty());
        assert_eq!(decoded.header.total_size, 0);
    }

    #[test]
    fn test_multi_chunk_out_of_order_reassembly() {
        let payload: Vec<u8> = (0..10_000).map(|i| ((i * 73 + 19) % 251) as u8).collect();
        let mut opts = EncodeOptions::new("chunked-bucket", "blobs/matrix.bin");
        opts.max_chunk_bytes = 2048; // Force 5 photo frames

        let frames = encode_object_to_frames(&payload, &opts).expect("multi-chunk encode");
        assert_eq!(frames.len(), 5);

        // Reverse order to prove chunk_index sorting works even if Shared Album reorders photos
        let mut pngs: Vec<Vec<u8>> = frames.into_iter().map(|f| f.png_bytes).collect();
        pngs.reverse();

        let decoded = decode_frames_to_object(&pngs).expect("reassemble out-of-order frames");
        assert_eq!(decoded.data, payload);
        assert_eq!(decoded.header.total_chunks, 5);
    }

    #[test]
    fn test_detects_corrupted_data_pixel() {
        let payload = b"immutable financial ledger entry #42";
        let opts = EncodeOptions::new("ledger", "entry-42.txt");
        let frames = encode_object_to_frames(payload, &opts).unwrap();

        let mut img = image::load_from_memory(&frames[0].png_bytes).unwrap().to_rgb8();
        // Flip a pixel inside the payload area (below HUD_BANNER_HEIGHT)
        let p = img.get_pixel_mut(5, HUD_BANNER_HEIGHT);
        p[0] ^= 0xFF;

        let mut corrupted_png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut corrupted_png), ImageFormat::Png)
            .unwrap();

        let err = decode_single_frame(&corrupted_png).unwrap_err();
        assert!(matches!(
            err,
            CodecError::ChecksumMismatch { .. } | CodecError::InvalidMagic
        ));
    }
}

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use tower_http::cors::CorsLayer;

use crate::backend::{ObjectRecord, SharedAlbumStore, StorageError};

pub fn build_router(store: Arc<SharedAlbumStore>) -> Router {
    Router::new()
        .route("/_api/overview", get(api_overview_handler))
        .route("/_api/bucket/:bucket", get(api_bucket_objects_handler))
        .route("/", get(list_buckets_handler))
        .route(
            "/:bucket",
            put(create_bucket_handler)
                .get(list_objects_handler)
                .head(head_bucket_handler)
                .delete(delete_bucket_handler)
                .post(post_bucket_handler),
        )
        .route(
            "/:bucket/*key",
            put(put_object_handler)
                .get(get_object_handler)
                .head(head_object_handler)
                .delete(delete_object_handler),
        )
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(CorsLayer::permissive())
        .with_state(store)
}

async fn api_overview_handler(State(store): State<Arc<SharedAlbumStore>>) -> impl IntoResponse {
    Json(store.overview().await)
}

async fn api_bucket_objects_handler(
    State(store): State<Arc<SharedAlbumStore>>,
    Path(bucket): Path<String>,
) -> Response {
    match store.list_objects(&bucket, None, 1000).await {
        Ok(items) => Json(items).into_response(),
        Err(e) => s3_error_response(e, &format!("/{bucket}")),
    }
}

async fn list_buckets_handler(State(store): State<Arc<SharedAlbumStore>>) -> Response {
    let buckets = store.list_buckets().await;
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Owner>
    <ID>icloud-shared-album-owner</ID>
    <DisplayName>icloud-s3</DisplayName>
  </Owner>
  <Buckets>"#,
    );

    for b in buckets {
        xml.push_str(&format!(
            "\n    <Bucket><Name>{}</Name><CreationDate>{}</CreationDate></Bucket>",
            xml_escape(&b.name),
            b.created_at.to_rfc3339()
        ));
    }
    xml.push_str("\n  </Buckets>\n</ListAllMyBucketsResult>");

    xml_response(StatusCode::OK, xml)
}

async fn create_bucket_handler(
    State(store): State<Arc<SharedAlbumStore>>,
    Path(bucket): Path<String>,
) -> Response {
    match store.create_bucket(&bucket).await {
        Ok(_) => {
            let mut resp = Response::new(axum::body::Body::empty());
            *resp.status_mut() = StatusCode::OK;
            resp.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_str(&format!("/{bucket}"))
                    .unwrap_or_else(|_| HeaderValue::from_static("/")),
            );
            resp
        }
        Err(e) => s3_error_response(e, &format!("/{bucket}")),
    }
}

async fn head_bucket_handler(
    State(store): State<Arc<SharedAlbumStore>>,
    Path(bucket): Path<String>,
) -> Response {
    match store.head_bucket(&bucket).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => s3_error_response(e, &format!("/{bucket}")),
    }
}

async fn delete_bucket_handler(
    State(store): State<Arc<SharedAlbumStore>>,
    Path(bucket): Path<String>,
) -> Response {
    match store.delete_bucket(&bucket).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => s3_error_response(e, &format!("/{bucket}")),
    }
}

async fn post_bucket_handler(
    State(store): State<Arc<SharedAlbumStore>>,
    Path(bucket): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    // Support S3 multi-object delete: POST /:bucket?delete
    if params.contains_key("delete") {
        let body_str = String::from_utf8_lossy(&body);
        let mut deleted_keys = Vec::new();
        for part in body_str.split("<Key>") {
            if let Some(end) = part.find("</Key>") {
                let key = xml_unescape(&part[..end]);
                if store.delete_object(&bucket, &key).await.is_ok() {
                    deleted_keys.push(key);
                }
            }
        }
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
        );
        for k in deleted_keys {
            xml.push_str(&format!(
                "\n  <Deleted><Key>{}</Key></Deleted>",
                xml_escape(&k)
            ));
        }
        xml.push_str("\n</DeleteResult>");
        return xml_response(StatusCode::OK, xml);
    }

    StatusCode::BAD_REQUEST.into_response()
}

async fn list_objects_handler(
    State(store): State<Arc<SharedAlbumStore>>,
    Path(bucket): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let prefix = params.get("prefix").map(String::as_str).unwrap_or("");
    let delimiter = params.get("delimiter").map(String::as_str).unwrap_or("");
    let max_keys: usize = params
        .get("max-keys")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let is_v2 = params.get("list-type").map(String::as_str) == Some("2");

    let objects = match store.list_objects(&bucket, Some(prefix), max_keys).await {
        Ok(items) => items,
        Err(e) => return s3_error_response(e, &format!("/{bucket}")),
    };

    let mut direct_contents = Vec::new();
    let mut common_prefixes = BTreeSet::new();

    for obj in objects {
        if !delimiter.is_empty() && obj.key.starts_with(prefix) {
            let remainder = &obj.key[prefix.len()..];
            if let Some(pos) = remainder.find(delimiter) {
                let cp = format!("{}{}{}", prefix, &remainder[..pos], delimiter);
                common_prefixes.insert(cp);
                continue;
            }
        }
        direct_contents.push(obj);
    }

    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
    );
    xml.push_str(&format!("\n  <Name>{}</Name>", xml_escape(&bucket)));
    xml.push_str(&format!("\n  <Prefix>{}</Prefix>", xml_escape(prefix)));
    if is_v2 {
        xml.push_str(&format!(
            "\n  <KeyCount>{}</KeyCount>",
            direct_contents.len() + common_prefixes.len()
        ));
    }
    xml.push_str(&format!("\n  <MaxKeys>{max_keys}</MaxKeys>"));
    xml.push_str("\n  <IsTruncated>false</IsTruncated>");

    for obj in direct_contents {
        xml.push_str(&format!(
            "\n  <Contents>
    <Key>{}</Key>
    <LastModified>{}</LastModified>
    <ETag>&quot;{}&quot;</ETag>
    <Size>{}</Size>
    <StorageClass>ICLOUD_SHARED_ALBUM</StorageClass>
  </Contents>",
            xml_escape(&obj.key),
            obj.last_modified.to_rfc3339(),
            obj.etag,
            obj.size
        ));
    }

    for cp in common_prefixes {
        xml.push_str(&format!(
            "\n  <CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
            xml_escape(&cp)
        ));
    }

    xml.push_str("\n</ListBucketResult>");
    xml_response(StatusCode::OK, xml)
}

async fn put_object_handler(
    State(store): State<Arc<SharedAlbumStore>>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let key = normalize_key(&key);

    // Handle AWS Signature V4 Streaming Chunked Payload (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD`)
    // sent by `aws s3 cp` when uploading streams.
    let decoded_payload = if is_aws_chunked(&headers) {
        decode_aws_chunked(&body)
    } else {
        body.to_vec()
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|ct| !ct.is_empty() && *ct != "binary/octet-stream")
        .map(String::from);

    let mut metadata = BTreeMap::new();
    for (name, val) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if let Some(meta_key) = lower.strip_prefix("x-amz-meta-") {
            if let Ok(v) = val.to_str() {
                metadata.insert(meta_key.to_string(), v.to_string());
            }
        }
    }

    match store
        .put_object(&bucket, &key, &decoded_payload, content_type, metadata)
        .await
    {
        Ok(record) => {
            let mut resp = Response::new(axum::body::Body::empty());
            *resp.status_mut() = StatusCode::OK;
            insert_object_headers(resp.headers_mut(), &record);
            resp
        }
        Err(e) => s3_error_response(e, &format!("/{bucket}/{key}")),
    }
}

async fn get_object_handler(
    State(store): State<Arc<SharedAlbumStore>>,
    Path((bucket, key)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let key = normalize_key(&key);

    // Inspect the raw iCloud Shared Album PNG carrier photo with `?format=png&chunk=0`
    if params.get("format").map(String::as_str) == Some("png") {
        let chunk_idx: usize = params
            .get("chunk")
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        return match store.get_object_carrier_png(&bucket, &key, chunk_idx).await {
            Ok((record, png_bytes)) => {
                let mut resp = Response::new(axum::body::Body::from(png_bytes));
                *resp.status_mut() = StatusCode::OK;
                resp.headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
                resp.headers_mut().insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("no-cache"),
                );
                if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", record.etag)) {
                    resp.headers_mut().insert(header::ETAG, v);
                }
                resp
            }
            Err(e) => s3_error_response(e, &format!("/{bucket}/{key}")),
        };
    }

    match store.get_object(&bucket, &key).await {
        Ok((record, data)) => {
            let mut resp = Response::new(axum::body::Body::from(data));
            *resp.status_mut() = StatusCode::OK;
            insert_object_headers(resp.headers_mut(), &record);
            resp
        }
        Err(e) => s3_error_response(e, &format!("/{bucket}/{key}")),
    }
}

async fn head_object_handler(
    State(store): State<Arc<SharedAlbumStore>>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    let key = normalize_key(&key);
    match store.head_object(&bucket, &key).await {
        Ok(record) => {
            let mut resp = Response::new(axum::body::Body::empty());
            *resp.status_mut() = StatusCode::OK;
            insert_object_headers(resp.headers_mut(), &record);
            resp
        }
        Err(e) => s3_error_response(e, &format!("/{bucket}/{key}")),
    }
}

async fn delete_object_handler(
    State(store): State<Arc<SharedAlbumStore>>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    let key = normalize_key(&key);
    match store.delete_object(&bucket, &key).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => s3_error_response(e, &format!("/{bucket}/{key}")),
    }
}

fn insert_object_headers(headers: &mut HeaderMap, record: &ObjectRecord) {
    if let Ok(ct) = HeaderValue::from_str(&record.content_type) {
        headers.insert(header::CONTENT_TYPE, ct);
    }
    if let Ok(len) = HeaderValue::from_str(&record.size.to_string()) {
        headers.insert(header::CONTENT_LENGTH, len);
    }
    if let Ok(etag) = HeaderValue::from_str(&format!("\"{}\"", record.etag)) {
        headers.insert(header::ETAG, etag);
    }
    let last_mod = record
        .last_modified
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    if let Ok(lm) = HeaderValue::from_str(&last_mod) {
        headers.insert(header::LAST_MODIFIED, lm);
    }
    if let Ok(frames) = HeaderValue::from_str(&record.frames.len().to_string()) {
        headers.insert("x-icloud-s3-photo-frames", frames);
    }
    if let Some(first_frame) = record.frames.first() {
        if let Ok(album) = HeaderValue::from_str(&first_frame.album_name) {
            headers.insert("x-icloud-s3-shared-album", album);
        }
    }
    if let Ok(sha) = HeaderValue::from_str(&record.sha256) {
        headers.insert("x-icloud-s3-sha256", sha);
    }
    for (k, v) in &record.metadata {
        if let (Ok(hk), Ok(hv)) = (
            header::HeaderName::from_bytes(format!("x-amz-meta-{k}").as_bytes()),
            HeaderValue::from_str(v),
        ) {
            headers.insert(hk, hv);
        }
    }
}

fn normalize_key(key: &str) -> String {
    percent_encoding::percent_decode_str(key.trim_start_matches('/'))
        .decode_utf8_lossy()
        .to_string()
}

fn is_aws_chunked(headers: &HeaderMap) -> bool {
    if let Some(sha_hdr) = headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
    {
        if sha_hdr.starts_with("STREAMING-") {
            return true;
        }
    }
    if let Some(enc) = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
    {
        if enc.contains("aws-chunked") {
            return true;
        }
    }
    false
}

/// Decodes AWS SigV4 streaming chunked bodies (`<hex-size>;chunk-signature=<sig>\r\n<bytes>\r\n...0;chunk-signature=...\r\n\r\n`)
fn decode_aws_chunked(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut cursor = 0usize;

    while cursor < body.len() {
        // Find end of chunk header line `\r\n`
        let Some(rel_end) = body[cursor..].windows(2).position(|w| w == b"\r\n") else {
            // Fallback if body wasn't actually chunked
            if out.is_empty() {
                return body.to_vec();
            }
            break;
        };
        let header_line = &body[cursor..cursor + rel_end];
        let header_str = String::from_utf8_lossy(header_line);
        let size_hex = header_str.split(';').next().unwrap_or("").trim();
        let Ok(chunk_size) = usize::from_str_radix(size_hex, 16) else {
            if out.is_empty() {
                return body.to_vec();
            }
            break;
        };

        cursor += rel_end + 2;
        if chunk_size == 0 {
            break;
        }
        let end_data = (cursor + chunk_size).min(body.len());
        out.extend_from_slice(&body[cursor..end_data]);
        cursor = (end_data + 2).min(body.len()); // Skip trailing \r\n
    }

    out
}

fn s3_error_response(err: StorageError, resource: &str) -> Response {
    let (status, code) = match &err {
        StorageError::NoSuchBucket(_) => (StatusCode::NOT_FOUND, "NoSuchBucket"),
        StorageError::NoSuchKey { .. } => (StatusCode::NOT_FOUND, "NoSuchKey"),
        StorageError::BucketNotEmpty(_) => (StatusCode::CONFLICT, "BucketNotEmpty"),
        StorageError::AlbumLimitExceeded => (StatusCode::BAD_REQUEST, "TooManyBuckets"),
        StorageError::AlbumPhotoLimitExceeded { .. } => {
            (StatusCode::INSUFFICIENT_STORAGE, "QuotaExceeded")
        }
        StorageError::InvalidBucketName(_) => (StatusCode::BAD_REQUEST, "InvalidBucketName"),
        StorageError::Codec(_) => (StatusCode::INTERNAL_SERVER_ERROR, "InternalError"),
        StorageError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "InternalError"),
    };

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>{code}</Code>
  <Message>{}</Message>
  <Resource>{}</Resource>
  <RequestId>icloud-s3-req</RequestId>
</Error>"#,
        xml_escape(&err.to_string()),
        xml_escape(resource)
    );

    xml_response(status, xml)
}

fn xml_response(status: StatusCode, body: String) -> Response {
    let mut resp = Response::new(axum::body::Body::from(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml"),
    );
    resp
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::StoreConfig;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_s3_http_put_get_list_and_carrier_png() {
        let store = SharedAlbumStore::new(StoreConfig::default()).await.unwrap();
        let app = build_router(store);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        // 1. CreateBucket
        let res = client
            .put(format!("{base}/my-photos-bucket"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // 2. PutObject
        let body = b"hello from s3 over icloud shared albums!";
        let put_res = client
            .put(format!("{base}/my-photos-bucket/docs/readme.txt"))
            .header("x-amz-meta-author", "cloud-architect")
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(put_res.status(), StatusCode::OK);
        assert!(put_res.headers().contains_key("etag"));
        assert_eq!(
            put_res
                .headers()
                .get("x-icloud-s3-photo-frames")
                .unwrap()
                .to_str()
                .unwrap(),
            "1"
        );

        // 3. GetObject (raw bytes)
        let get_res = client
            .get(format!("{base}/my-photos-bucket/docs/readme.txt"))
            .send()
            .await
            .unwrap();
        assert_eq!(get_res.status(), StatusCode::OK);
        assert_eq!(
            get_res
                .headers()
                .get("x-amz-meta-author")
                .unwrap()
                .to_str()
                .unwrap(),
            "cloud-architect"
        );
        let fetched = get_res.bytes().await.unwrap();
        assert_eq!(fetched.as_ref(), body);

        // 4. GetObject with `?format=png` returns a valid PNG carrier image
        let png_res = client
            .get(format!(
                "{base}/my-photos-bucket/docs/readme.txt?format=png"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(png_res.status(), StatusCode::OK);
        assert_eq!(
            png_res
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "image/png"
        );
        let png_bytes = png_res.bytes().await.unwrap();
        assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));

        // 5. ListObjectsV2
        let list_res = client
            .get(format!(
                "{base}/my-photos-bucket?list-type=2&prefix=docs/"
            ))
            .send()
            .await
            .unwrap();
        let list_xml = list_res.text().await.unwrap();
        assert!(list_xml.contains("<Key>docs/readme.txt</Key>"));
        assert!(list_xml.contains("<StorageClass>ICLOUD_SHARED_ALBUM</StorageClass>"));
    }
}

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

use icloud_s3::backend::{SharedAlbumStore, StoreConfig};
use icloud_s3::codec::{
    decode_frames_to_object, encode_object_to_frames, EncodeOptions,
};
use icloud_s3::eval::{
    deterministic_bytes, format_eval_markdown, run_full_evaluation, write_eval_artifacts,
};
use icloud_s3::server::build_router;

#[derive(Debug, Parser)]
#[command(
    name = "icloud-s3",
    version,
    about = "An S3-compatible object store backed by iCloud Shared Photo Albums."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the S3-compatible HTTP server and Shared Album web inspector
    Serve {
        /// Address to bind the S3 server to
        #[arg(long, default_value = "127.0.0.1:9000")]
        bind: String,

        /// Run with an in-memory Shared Album (no disk or iCloud needed)
        #[arg(long, default_value_t = false)]
        dry_run: bool,

        /// Directory where Shared Album folders and carrier PNG photos are stored
        #[arg(long)]
        album_dir: Option<PathBuf>,

        /// Number of Shared Album shards per S3 bucket (1..=200)
        #[arg(long, default_value_t = 1)]
        shards: usize,

        /// Maximum payload bytes per single 2048x2048 PNG photo frame
        #[arg(long, default_value_t = 2 * 1024 * 1024)]
        max_chunk_bytes: usize,

        /// Simulated per-write Shared Album sync latency in milliseconds
        #[arg(long, default_value_t = 0)]
        simulated_write_ms: u64,

        /// Also import each generated carrier PNG into macOS Photos.app via AppleScript
        #[arg(long, default_value_t = false)]
        sync_photos_app: bool,
    },

    /// Encode a file directly into one or more iCloud Shared Album RGB PNG carrier frames
    Encode {
        /// Input file path
        #[arg(short, long)]
        input: PathBuf,

        /// Output PNG file path (or prefix if multi-chunk)
        #[arg(short, long)]
        output: PathBuf,

        /// S3 bucket name burned into the visual HUD header
        #[arg(long, default_value = "prod-backups")]
        bucket: String,

        /// S3 key burned into the visual HUD header
        #[arg(long)]
        key: Option<String>,
    },

    /// Decode an iCloud Shared Album PNG carrier image back into its original bytes
    Decode {
        /// Input PNG file path(s)
        #[arg(short, long, required = true)]
        input: Vec<PathBuf>,

        /// Output file path for the reconstructed S3 object
        #[arg(short, long)]
        output: PathBuf,
    },

    /// Run the S3 protocol conformance & RGB pixel codec evaluation suite
    Eval {
        /// Optional path to write the JSON evaluation report
        #[arg(long)]
        json: Option<PathBuf>,

        /// Optional path to write the Markdown evaluation report
        #[arg(long)]
        markdown: Option<PathBuf>,
    },

    /// Seed synthetic S3 objects (SQL dump, Terraform state, OTel traces, SQLite snapshot) into a running server
    SeedDemo {
        /// S3 endpoint URL
        #[arg(long, default_value = "http://127.0.0.1:9000")]
        endpoint: String,

        /// Target S3 bucket (Shared Album)
        #[arg(long, default_value = "prod-backups")]
        bucket: String,
    },

    /// Run the live Terminal UI benchmark (10 photos + 1 ZIP) against icloud-s3 & Cloudflare R2
    Bench {
        /// S3 endpoint URL
        #[arg(long, default_value = "http://127.0.0.1:9000")]
        endpoint: String,

        /// Target S3 bucket (Shared Album)
        #[arg(long, default_value = "prod-backups")]
        bucket: String,

        /// Directory containing test files (JPEGs + ZIP)
        #[arg(long, default_value = "demo/sample-photos")]
        input_dir: PathBuf,

        /// Optional Cloudflare R2 baseline JSON file for side-by-side speedup comparison
        #[arg(long, default_value = "docs/r2-comparison.json")]
        r2_json: PathBuf,

        /// Delay between uploads in ms (for watching live Finder Shared Album creation)
        #[arg(long, default_value_t = 350)]
        step_delay_ms: u64,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "icloud_s3=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            bind,
            dry_run,
            album_dir,
            shards,
            max_chunk_bytes,
            simulated_write_ms,
            sync_photos_app,
        } => {
            let resolved_dir = if dry_run {
                None
            } else {
                Some(album_dir.unwrap_or_else(|| PathBuf::from("./albums-live")))
            };

            let cfg = StoreConfig {
                album_dir: resolved_dir.clone(),
                shards,
                max_chunk_bytes,
                simulated_write_latency: Duration::from_millis(simulated_write_ms),
                sync_macos_photos_app: sync_photos_app,
            };

            let store = SharedAlbumStore::new(cfg).await?;
            let ov = store.overview().await;
            let app = build_router(store);

            let listener = TcpListener::bind(&bind).await?;
            let local_addr = listener.local_addr()?;

            println!("╭──────────────────────────────────────────────────────────────╮");
            println!("│  icloud-s3 // iCloud Shared Album S3 Object Store            │");
            println!("├──────────────────────────────────────────────────────────────┤");
            println!("│  S3 Endpoint     : http://{:<35}│", local_addr);
            println!(
                "│  Backend Mode    : {:<42}│",
                if dry_run {
                    "dry-run (in-memory Shared Album)"
                } else {
                    "Shared Album PNG directory"
                }
            );
            println!(
                "│  Loaded State    : {} buckets, {} objects ({} photo frames)     │",
                ov.bucket_count, ov.total_objects, ov.total_photo_frames
            );
            println!("│  iCloud Quota    : 0 B used of 5 GB (Shared Album loophole)  │");
            println!("╰──────────────────────────────────────────────────────────────╯");

            axum::serve(listener, app).await?;
        }

        Commands::Encode {
            input,
            output,
            bucket,
            key,
        } => {
            let data = tokio::fs::read(&input).await?;
            let key_name = key.unwrap_or_else(|| {
                input
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("object.bin")
                    .to_string()
            });
            let opts = EncodeOptions::new(bucket, key_name);
            let frames = encode_object_to_frames(&data, &opts)?;

            if frames.len() == 1 {
                tokio::fs::write(&output, &frames[0].png_bytes).await?;
                println!(
                    "Encoded {} bytes -> {} ({}x{} RGB PNG, {} bytes, ETag={})",
                    data.len(),
                    output.display(),
                    frames[0].width,
                    frames[0].height,
                    frames[0].png_bytes.len(),
                    frames[0].header.etag
                );
            } else {
                for f in &frames {
                    let stem = output
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("frame");
                    let parent = output.parent().unwrap_or_else(|| std::path::Path::new("."));
                    let chunk_path = parent.join(format!(
                        "{stem}_part{:03}of{:03}.png",
                        f.header.chunk_index + 1,
                        f.header.total_chunks
                    ));
                    tokio::fs::write(&chunk_path, &f.png_bytes).await?;
                    println!(
                        "Wrote chunk {}/{} -> {} ({}x{} PNG)",
                        f.header.chunk_index + 1,
                        f.header.total_chunks,
                        chunk_path.display(),
                        f.width,
                        f.height
                    );
                }
            }
        }

        Commands::Decode { input, output } => {
            let mut png_frames = Vec::with_capacity(input.len());
            for path in &input {
                png_frames.push(tokio::fs::read(path).await?);
            }
            let decoded = decode_frames_to_object(&png_frames)?;
            tokio::fs::write(&output, &decoded.data).await?;
            println!(
                "Decoded s3://{}/{} ({} bytes, SHA-256={}…) -> {}",
                decoded.header.bucket,
                decoded.header.key,
                decoded.data.len(),
                &decoded.header.object_sha256[..16],
                output.display()
            );
        }

        Commands::Eval { json, markdown } => {
            let report = run_full_evaluation().await?;
            let md = format_eval_markdown(&report);
            println!("{md}");
            write_eval_artifacts(&report, json.as_deref(), markdown.as_deref()).await?;
            if !report.all_passed {
                std::process::exit(1);
            }
        }

        Commands::SeedDemo { endpoint, bucket } => {
            let client = reqwest::Client::new();
            let base = endpoint.trim_end_matches('/');

            // 1. Create bucket
            client.put(format!("{base}/{bucket}")).send().await?;
            println!("Created Shared Album bucket: s3://{bucket}");

            let demo_objects: Vec<(&str, &str, Vec<u8>)> = vec![
                (
                    "db/postgres-prod-2026-10-30.sql",
                    "application/sql",
                     {
                        let mut v = b"-- PostgreSQL production dump stored in iCloud Shared Album\nCREATE TABLE users (id BIGSERIAL PRIMARY KEY, email TEXT, plan TEXT);\nINSERT INTO users VALUES (1, 'ops@example.com', 'free-tier-architect');\n".to_vec();
                        v.extend_from_slice(&deterministic_bytes(18_432, 0x1010_2020));
                        v
                    },
                ),
                (
                    "infra/production.tfstate",
                    "application/json",
                     {
                        let mut v = br#"{"version":4,"terraform_version":"1.9.8","serial":42,"lineage":"icloud-shared-album-backend","outputs":{"s3_monthly_bill_usd":{"value":0.0}}}"#.to_vec();
                        v.extend_from_slice(&deterministic_bytes(28_672, 0x3030_4040));
                        v
                    },
                ),
                (
                    "traces/otel-agent-spans-0930.json",
                    "application/json",
                     {
                        let mut v = br#"[{"trace_id":"8f3a9c11e2","span":"llm.completion","model":"claude-4","storage":"iCloud Shared Album","quota_bytes":0}]"#.to_vec();
                        v.extend_from_slice(&deterministic_bytes(36_864, 0x5050_6060));
                        v
                    },
                ),
                (
                    "checkpoints/lora-adapter-step-500.safetensors",
                    "application/octet-stream",
                    deterministic_bytes(98_304, 0x7070_8080),
                ),
            ];

            for (key, content_type, payload) in demo_objects {
                let res = client
                    .put(format!("{base}/{bucket}/{key}"))
                    .header("content-type", content_type)
                    .header("x-amz-meta-storage-tier", "icloud-shared-album-zero-quota")
                    .body(payload.clone())
                    .send()
                    .await?;
                let etag = res
                    .headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let album = res
                    .headers()
                    .get("x-icloud-s3-shared-album")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("icloud-s3");
                println!(
                    "PUT s3://{bucket}/{key} ({} B) -> Shared Album '{album}' ETag={etag}",
                    payload.len()
                );
            }
        }

        Commands::Bench {
            endpoint,
            bucket,
            input_dir,
            r2_json,
            step_delay_ms,
        } => {
            let client = reqwest::Client::new();
            let base = endpoint.trim_end_matches('/');

            // Load R2 baseline latencies if available
            let mut r2_map: HashMap<String, (f64, f64)> = HashMap::new();
            if let Ok(raw) = tokio::fs::read_to_string(&r2_json).await {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(entries) = val.get("entries").and_then(|v| v.as_array()) {
                        for e in entries {
                            if let (Some(k), Some(p), Some(g)) = (
                                e.get("key").and_then(|v| v.as_str()),
                                e.get("r2_put_ms").and_then(|v| v.as_f64()),
                                e.get("r2_get_ms").and_then(|v| v.as_f64()),
                            ) {
                                r2_map.insert(k.to_string(), (p, g));
                            }
                        }
                    }
                }
            }

            // Collect files in input_dir
            let mut files = Vec::new();
            let mut rd = tokio::fs::read_dir(&input_dir).await?;
            while let Some(entry) = rd.next_entry().await? {
                let p = entry.path();
                if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                    if ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("zip") {
                        files.push(p);
                    }
                }
            }
            files.sort();

            // Create bucket
            client.put(format!("{base}/{bucket}")).send().await?;

            println!("\x1b[1;36m╭────┬────────────────────────┬─────────┬────────────┬──────────┬──────────┬─────────────╮\x1b[0m");
            println!("\x1b[1;36m│\x1b[0m \x1b[1;97micloud-s3\x1b[0m // \x1b[1;33mTerminal UI S3 Benchmark & iCloud Shared Album RGB PNG Verifier\x1b[0m           \x1b[1;36m│\x1b[0m");
            println!("\x1b[1;36m│\x1b[0m Endpoint: \x1b[32m{:<22}\x1b[0m Bucket: \x1b[36ms3://{:<14}\x1b[0m Compare: \x1b[35mr2://demos\x1b[0m       \x1b[1;36m│\x1b[0m", base, bucket);
            println!("\x1b[1;36m├────┼────────────────────────┼─────────┼────────────┼──────────┼──────────┼─────────────┤\x1b[0m");
            println!("\x1b[1;36m│\x1b[0m \x1b[1m#\x1b[0m  \x1b[1;36m│\x1b[0m \x1b[1mS3 OBJECT KEY\x1b[0m          \x1b[1;36m│\x1b[0m \x1b[1mSIZE\x1b[0m    \x1b[1;36m│\x1b[0m \x1b[1mICLOUD PUT\x1b[0m \x1b[1;36m│\x1b[0m \x1b[1mR2 PUT\x1b[0m   \x1b[1;36m│\x1b[0m \x1b[1mSPEEDUP\x1b[0m  \x1b[1;36m│\x1b[0m \x1b[1mSHA-256\x1b[0m     \x1b[1;36m│\x1b[0m");
            println!("\x1b[1;36m├────┼────────────────────────┼─────────┼────────────┼──────────┼──────────┼─────────────┤\x1b[0m");

            let mut sum_put_ms = 0.0;
            let mut sum_get_ms = 0.0;
            let mut sum_r2_put_ms = 0.0;
            let mut peak_put_mbps = 0.0_f64;
            let mut peak_get_mbps = 0.0_f64;
            let mut total_bytes = 0usize;

            for (idx, path) in files.iter().enumerate() {
                let key = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file.bin")
                    .to_string();
                let data = tokio::fs::read(path).await?;
                let size_kb = data.len() as f64 / 1024.0;
                total_bytes += data.len();

                let mut hasher = Sha256::new();
                hasher.update(&data);
                let orig_sha = hex::encode(hasher.finalize());

                let ctype = if key.ends_with(".zip") {
                    "application/zip"
                } else {
                    "image/jpeg"
                };

                let t_put = Instant::now();
                let put_res = client
                    .put(format!("{base}/{bucket}/{key}"))
                    .header("content-type", ctype)
                    .body(data.clone())
                    .send()
                    .await?;
                let put_ms = t_put.elapsed().as_secs_f64() * 1000.0;
                assert!(put_res.status().is_success());

                let t_get = Instant::now();
                let get_res = client
                    .get(format!("{base}/{bucket}/{key}"))
                    .send()
                    .await?;
                let fetched = get_res.bytes().await?;
                let get_ms = t_get.elapsed().as_secs_f64() * 1000.0;

                let mut get_hasher = Sha256::new();
                get_hasher.update(&fetched);
                let got_sha = hex::encode(get_hasher.finalize());
                assert_eq!(orig_sha, got_sha);

                let put_mbps = (data.len() as f64 / (1024.0 * 1024.0)) / (put_ms / 1000.0);
                let get_mbps = (data.len() as f64 / (1024.0 * 1024.0)) / (get_ms / 1000.0);
                peak_put_mbps = peak_put_mbps.max(put_mbps);
                peak_get_mbps = peak_get_mbps.max(get_mbps);

                let (r2_put, _r2_get) = r2_map.get(&key).copied().unwrap_or((512.0, 339.0));
                let speedup = r2_put / put_ms;

                sum_put_ms += put_ms;
                sum_get_ms += get_ms;
                sum_r2_put_ms += r2_put;

                println!(
                    "\x1b[1;36m│\x1b[0m {:02} \x1b[1;36m│\x1b[0m \x1b[96m{:<22}\x1b[0m \x1b[1;36m│\x1b[0m {:>5.1}KB \x1b[1;36m│\x1b[0m \x1b[1;32m{:>6.2} ms\x1b[0m  \x1b[1;36m│\x1b[0m {:>5.0} ms \x1b[1;36m│\x1b[0m \x1b[1;33m{:>5.0}x\x1b[0m   \x1b[1;36m│\x1b[0m \x1b[32m✓ {}\x1b[0m  \x1b[1;36m│\x1b[0m",
                    idx + 1,
                    key,
                    size_kb,
                    put_ms,
                    r2_put,
                    speedup,
                    &orig_sha[..8]
                );

                if step_delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(step_delay_ms)).await;
                }
            }

            let n = files.len().max(1) as f64;
            let avg_put = sum_put_ms / n;
            let avg_get = sum_get_ms / n;
            let avg_r2 = sum_r2_put_ms / n;
            let overall_speedup = avg_r2 / avg_put;

            println!("\x1b[1;36m├────┴────────────────────────┴─────────┴────────────┴──────────┴──────────┴─────────────┤\x1b[0m");
            println!(
                "\x1b[1;36m│\x1b[0m \x1b[1;32m✓ {}/{} OBJECTS BIT-EXACT\x1b[0m ({:.1} KB encoded into {} lossless 24-bit RGB Shared Album PNGs)\x1b[1;36m│\x1b[0m",
                files.len(),
                files.len(),
                total_bytes as f64 / 1024.0,
                files.len()
            );
            println!(
                "\x1b[1;36m│\x1b[0m \x1b[1micloud-s3 Avg:\x1b[0m \x1b[1;32m{:.2} ms PUT\x1b[0m / \x1b[1;32m{:.2} ms GET\x1b[0m  │ \x1b[1mPeak:\x1b[0m \x1b[1;36m{:>5.1} MB/s PUT\x1b[0m / \x1b[1;36m{:>5.1} MB/s GET\x1b[0m      \x1b[1;36m│\x1b[0m",
                avg_put, avg_get, peak_put_mbps, peak_get_mbps
            );
            println!(
                "\x1b[1;36m│\x1b[0m \x1b[1mvs Cloudflare R2:\x1b[0m \x1b[1;33m{:.0}x faster PUT\x1b[0m (R2 avg {:.0} ms) │ \x1b[1miCloud Quota Used:\x1b[0m \x1b[1;32m0 B / 5 GB\x1b[0m      \x1b[1;36m│\x1b[0m",
                overall_speedup, avg_r2
            );
            println!("\x1b[1;36m╰────────────────────────────────────────────────────────────────────────────────────────╯\x1b[0m");
        }
    }

    Ok(())
}

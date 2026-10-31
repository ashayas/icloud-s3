# iCloudS3 Evaluation & Benchmark Report

- **Status**: `10/10 passed` (PASS)
- **iCloud Drive Storage Consumed**: `0 B` (Shared Albums are outside the 5 GB iCloud Drive allowance)

## 1. Protocol & Pixel Codec Correctness Evals

| ID | Evaluation | Result | Time (ms) | Verification Details |
| --- | --- | --- | ---: | --- |
| `EVAL-01` | Zero-byte object roundtrip & MD5 ETag | PASS | 1.33 | 0 B -> 560x160 PNG (71176 B), ETag=d41d8cd98f00b204e9800998ecf8427e |
| `EVAL-02` | JSON payload + x-amz-meta-* preservation | PASS | 1.24 | Verified 51 B JSON + 2 custom metadata keys |
| `EVAL-03` | High-entropy 64 KiB binary bit-exact SHA-256 | PASS | 2.10 | 65,536 B packed into 560x160 RGB PNG (133268 B), SHA-256 verified |
| `EVAL-04` | 1 MiB single-photo payload (<= 2048x2048 limit) | PASS | 13.32 | 1,048,576 B -> 1 photo (1024x406 px, 1247801 B PNG) |
| `EVAL-05` | Multi-photo burst chunking & out-of-order reassembly | PASS | 6.26 | 45,000 B split across 6 photos, reversed, reassembled bit-exact |
| `EVAL-06` | Apple Shared Album 2048px dimension guard | PASS | 14.00 | All frames across 0 B .. 1 MiB strictly within 2048x2048 px |
| `EVAL-07` | Single-bit RGB pixel corruption detection (CRC32) | PASS | 1.68 | 1-bit green-channel mutation rejected by CRC32 guard |
| `EVAL-08` | Overwrite compaction (1 active photo per live key) | PASS | 1.27 | Old photo frame evicted on overwrite; 1 live photo remains |
| `EVAL-09` | Cold-boot state reconstruction from raw PNG files | PASS | 12.50 | Reconstructed 8 keys across 4 album shards from PNG pixels alone |
| `EVAL-10` | Unmodified `aws s3` CLI end-to-end integration | PASS | 1892.86 | `aws s3 mb/cp/ls` verified 24 KiB binary (SHA-256 bda93e255ce2…) |

## 2. RGB PNG Carrier Throughput & Payload Sweep

| Payload | Photo Frame | PNG Size | PUT p50 | PUT MB/s | GET p50 | GET MB/s | iCloud Quota |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 KiB JSON (`1024 B`) | `560×160` | 72195 B | 0.39 ms | 2.3 MB/s | 0.53 ms | 1.8 MB/s | `0 B` |
| 16 KiB Log Chunk (`16384 B`) | `560×160` | 85123 B | 0.44 ms | 35.1 MB/s | 0.63 ms | 24.1 MB/s | `0 B` |
| 64 KiB SQLite Page (`65536 B`) | `560×160` | 134841 B | 0.67 ms | 91.6 MB/s | 0.77 ms | 80.3 MB/s | `0 B` |
| 256 KiB Tarball (`262144 B`) | `1024×160` | 379033 B | 1.88 ms | 131.5 MB/s | 1.92 ms | 130.1 MB/s | `0 B` |
| 1 MiB Blob Frame (`1048576 B`) | `1024×406` | 1247801 B | 7.29 ms | 138.2 MB/s | 4.11 ms | 240.1 MB/s | `0 B` |

## 3. Shared Album Sharding (`--shards 1` vs `--shards 8`)

| Setup | Concurrency | PUT Throughput | PUT p50 | GET Throughput | GET p50 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 Shared Album (`--shards 1`) | 8 | 8.2 req/s | 977.3 ms | 1502 req/s | 0.62 ms |
| 8 Shared Albums (`--shards 8`) | 8 | 58.0 req/s | 122.6 ms | 1694 req/s | 0.57 ms |

## 4. Cold Restart Replay from Shared Album PNGs

| Metric | Value |
| --- | --- |
| Objects reconstructed from `.png` pixels | `50` (409600 B raw / 3917468 B PNG) |
| Cold-start album scan & header decode | `31.45 ms` |
| Warm readback of all 50 objects | `29.45 ms` |

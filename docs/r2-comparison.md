# Head-to-Head Benchmark: `icloud-s3` vs Cloudflare R2 (`10 JPEGs + 1 ZIP Archive`)
- **Workload**: 10 public test photographs (`560×360` JPEG) + `11-photos-bundle.zip` (ZIP archive containing all 10 images + `manifest.json`)
- **`icloud-s3` Average PUT**: `1.80 ms` (`23.3 MB/s`)
- **`icloud-s3` Average GET**: `1.44 ms` (`30.1 MB/s`)
- **Cloudflare R2 (`r2://demos`) Average PUT**: `512.4 ms`
- **Cloudflare R2 (`r2://demos`) Average GET**: `339.4 ms`
- **Speedup**: **`284× faster PUT`**, **`236× faster GET`**
- **iCloud Drive Quota Used**: `0 B` (Shared Album loophole)

| Object Key | Type | Size | Carrier PNG | `icloud-s3` PUT | Cloudflare R2 PUT | `icloud-s3` GET | Cloudflare R2 GET | Speedup | SHA-256 |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | --- | --- |
| `01-alpine-peak.jpg` | JPEG | 46,054 B | `560×160` | **1.63 ms** (26.9 MB/s) | 480 ms | **1.11 ms** (39.6 MB/s) | 296 ms | **294× PUT / 267× GET** | `45d3f44682f3…` ✓ |
| `02-pacific-reef.jpg` | JPEG | 21,908 B | `560×160` | **1.49 ms** (14.0 MB/s) | 893 ms | **1.23 ms** (17.0 MB/s) | 329 ms | **599× PUT / 268× GET** | `5a38b25aad52…` ✓ |
| `03-desert-canyon.jpg` | JPEG | 15,737 B | `560×160` | **2.63 ms** (5.7 MB/s) | 453 ms | **2.07 ms** (7.3 MB/s) | 405 ms | **172× PUT / 196× GET** | `1fb9c7e30ad5…` ✓ |
| `04-northern-lights.jpg` | JPEG | 48,274 B | `560×160` | **1.75 ms** (26.2 MB/s) | 457 ms | **1.30 ms** (35.4 MB/s) | 352 ms | **261× PUT / 270× GET** | `9fb9b7991eb2…` ✓ |
| `05-misty-redwoods.jpg` | JPEG | 46,086 B | `560×160` | **2.00 ms** (22.0 MB/s) | 392 ms | **1.31 ms** (33.5 MB/s) | 320 ms | **196× PUT / 244× GET** | `9e92349ac5a9…` ✓ |
| `06-volcanic-ridge.jpg` | JPEG | 31,012 B | `560×160` | **1.46 ms** (20.3 MB/s) | 427 ms | **2.09 ms** (14.2 MB/s) | 293 ms | **293× PUT / 140× GET** | `5139a1619dd7…` ✓ |
| `07-glacier-lagoon.jpg` | JPEG | 27,504 B | `560×160` | **1.67 ms** (15.7 MB/s) | 417 ms | **1.10 ms** (23.9 MB/s) | 359 ms | **249× PUT / 327× GET** | `de4347a000c3…` ✓ |
| `08-autumn-fjord.jpg` | JPEG | 28,634 B | `560×160` | **1.39 ms** (19.7 MB/s) | 554 ms | **1.04 ms** (26.4 MB/s) | 367 ms | **399× PUT / 355× GET** | `3a5a4177958e…` ✓ |
| `09-starlight-dome.jpg` | JPEG | 13,915 B | `560×160` | **1.44 ms** (9.2 MB/s) | 371 ms | **1.06 ms** (12.5 MB/s) | 262 ms | **257× PUT / 247× GET** | `343d2d9a9727…` ✓ |
| `10-coastal-arch.jpg` | JPEG | 9,688 B | `560×160` | **1.30 ms** (7.1 MB/s) | 417 ms | **1.05 ms** (8.8 MB/s) | 313 ms | **320× PUT / 297× GET** | `dbb8420a6e88…` ✓ |
| `11-photos-bundle.zip` | ZIP | 287,190 B | `1024×160` | **3.06 ms** (89.5 MB/s) | 775 ms | **2.44 ms** (112.4 MB/s) | 438 ms | **253× PUT / 180× GET** | `64686ca2fc2f…` ✓ |

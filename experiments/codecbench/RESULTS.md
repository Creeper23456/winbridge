<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# codecbench results: guest decode cost of candidate frame-return codecs (1080p BGRA)

Question: which format should the Linux host send to the Windows guest (4 vCPU, no GPU, software desktop)
so that the guest only pays a tiny decode + `memcpy`/`BitBlt`? All guest numbers below were measured
inside the disposable build VM. Host encoders are CPU stand-ins for the planned GPU compute shader.

## Method

- Crate `experiments/codecbench` (standalone, own `[workspace]`). `codecbench gen` (host, feature `gen`)
  synthesises frames, encodes them, decodes them back with the same decoder code the guest uses
  (PSNR on B,G,R vs the original; mean/min over the frames) and writes one stream file per
  (class, codec) plus `gen.jsonl`. `codecbench decode` loads the streams and times decode into a
  pre-allocated 1920x1080 BGRA surface (**DIB section via `CreateDIBSection` on Windows**, `Vec` on
  Linux), `std::time::Instant` (QueryPerformanceCounter on Windows), 240 frames per stream, first 20
  discarded as warm-up, so **220 samples** per row; p50/p95/max per frame; 1, 2 and 4 threads where the
  codec can be split. `codecbench tail` times the guest tail of the chain. `codecbench report` builds the table.
- Every decoded frame's output hash is checked against the host reference decode (`verify`, untimed pass
  before the timed one): **0 mismatches** in all guest and host runs (all SIMD variants are bit-identical
  to their scalar counterparts, NV12 included).
- Parallelism: a persistent spin-then-yield worker pool (tid 0 runs on the caller). Tile/slice split:
  8 independent tile-row groups (lossless, MJPEG strips `s8`), block rows (BC1/BC7), row pairs (NV12/raw).
  A single-JPEG-per-frame stream cannot be split, hence "-" for 2/4 threads on `mjpeg420`/`mjpeg444`.
- Timed section = decode only (compressed bytes already in RAM, as they would be after being read from the
  channel). The first frame of a 2-4 thread pass may include thread start-up; warm-up removes it.
  Source frames are ~0.25-2 GB per stream and are never reused within a pass, so caches do not flatter the numbers.
- Guest result per cell = mean of the two runs' p50 and p95, max of the two maxes. `run spread` = largest
  relative difference of p50 between run A and run B over 1/2/4 threads (noise indicator).

### Frame classes (240 distinct frames each, 1920x1080, all synthesised deterministically in `src/gen/frames.rs`)

| class | content | note |
|---|---|---|
| `ui` | guest screenshots from `/tmp/wbtest-shots` + `/tmp/g1.png`, nearest-neighbour tiled to 1080p; an 800x500 window scrolls 3 px/frame, a 64x64 tile changes every 4 frames | ~14 % of the 64x64 tiles change per frame (frame 0 counted in bytes) |
| `uiscroll` | same screenshots scrolled 5 px/frame over the whole screen | worst case for tile-diff: ~82 % tiles change |
| `mixed` | `ui` layout, 660x700 scrolling text window + 960x540 procedural "video" rectangle | ~48 % tiles change |
| `photo` | 4 wallpapers on disk (`adwaita-l.jpg`, `adwaita-d.jpg`, `vnc-d.png`, `symbolic-l.png` from `/usr/share/backgrounds/gnome`) Lanczos-scaled to 2400x1350, panned 16x9 px/frame, +-1 LSB grain on every pixel | every pixel differs every frame. **These wallpapers are smooth gradients/vector art, much easier than real photos** (PSNR and lossless ratios are flattered) |
| `game` | procedural: moving gradients, hard-edged circles/rectangles, pseudo-glyph HUD, +-4 noise on every pixel | every pixel differs every frame; noise is incompressible, so lossless ratios are pessimistic for real games |

### Candidates and encoders (host, CPU, single thread per frame)

| codec id | what it is | encoder |
|---|---|---|
| `raw-full` / `raw-diff` | BGRA memcpy of the frame / of changed 64x64 tiles (index list + packed tiles) | none (diff: tile compare) |
| `nv12` | 4:2:0 YUV, BT.709 limited range, 2x2 box chroma, nearest chroma upsampling in decode. Decoders: scalar, SSE2, AVX2 (`std::arch`, runtime detection) | scalar Rust |
| `bc1` | DXT1 4 bpp. Decoders: scalar, SSSE3 (pshufb palette lookup), `texture2ddecoder` crate | `texpresso` 2.0 ClusterFit, uniform RGB weights |
| `bc7` | 8 bpp. **Only mode 6 is produced** (1 subset, RGBA 7.7.7.7+shared p-bit, 4-bit indices, alpha forced to 255); decoder handles mode 6 only (scalar, SSE2, AVX2) plus `texture2ddecoder` (all modes, generic) as reference | own encoder (PCA axis, 2 least-squares refinements, index search); `intel_tex_2` (needs ISPC, not installed) was not used |
| `lz4-*` / `zstd1-*` / `zstd3-*` | lossless per tile-row group; `-full` = whole rows, `-diff` = only changed tiles vs previous frame (decoder keeps the previous surface) | `lz4_flex` block, `zstd` level 1 / 3 |
| `mjpeg444` / `mjpeg420` (`s8` = 8 independent strips) | JPEG quality 90, decoded straight to BGRA | `jpeg-encoder` 0.7; decode `zune-jpeg` 0.5 (pure Rust, SIMD inside) |

The diff variants are only generated for `ui`, `uiscroll` and `mixed` (in `photo`/`game` every tile changes,
so they would duplicate the full variants).

## VM and host

- Build VM (`Windows11Build`, run from a copy of its helper dir): QEMU/KVM guest, `-machine q35,accel=kvm
  -cpu host,hv_*` (Hyper-V enlightenments), 12 GB RAM. `Win32_ComputerSystem`: Manufacturer QEMU,
  Model "Standard PC (Q35 + ICH9, 2009)", HypervisorPresent True. `Win32_Processor`: **AMD Ryzen 9 5900X
  12-Core Processor, 4 cores / 4 logical (what the guest sees), 3700 MHz nominal**; Windows 11 IoT
  Enterprise LTSC build 26100; rustc/cargo 1.100.0, MSVC 14.52, build via `cargo build --release`
  inside the VM.
- `run.sh` defaults to `-smp $(nproc)/2` (= 12 here). I started it with `VM_THREADS=4`, i.e. **`-smp 4`,
  same as the real LTSC guest** (`Windows11LTSC-partition/run.sh`: `-smp 4 -m 8G`, same `-cpu host`).
  vCPUs are not pinned and the Linux host is loaded (the user's LTSC VM is running, load average ~3), so
  vCPU threads compete with other work: this is the main noise source.
- Host reference: same Ryzen 9 5900X, native Linux, 24 logical CPUs, loaded the same way.
- Data transfer: `./vm push` ran at ~32 MB/s, so the 6.2 GB stream set took ~11 min per VM boot. The
  two decode runs (A, B) were two full passes of all five classes in **one VM boot** (about 7 min each);
  the `ui` class was repeated in a **second session** (user session 1, see below). I did not do two VM boots
  for the whole set (transfer cost); the spread therefore does not include boot-to-boot differences.
  Stream data was deleted afterwards.

## Results

### Guest decode (Windows build VM, 4 vCPU), by frame class

Columns: bytes/frame = mean encoded size (diff variants: includes the full first frame, and the share of
changed tiles in steady state); MB/s @60 fps = bytes/frame x 60 (decimal MB); PSNR mean/min over 240 frames
on B,G,R (`lossless` = bit-exact); `px err>24` = % of pixels where any channel is off by more than 24
levels (a proxy for visible fringing on text/edges); host enc = CPU encoder p50 per frame, single thread
(**CPU only; the real encoder is a GPU compute shader, so only the relative cost and the format matter**);
decode cells are `p50/p95/max` ms.

| codec / decoder | class | bytes/frame | MB/s @60 fps | PSNR mean/min dB | px err>24 % | host enc ms p50 | guest 1T p50/p95/max ms | guest 2T | guest 4T | run spread |
|---|---|---:|---:|---|---:|---:|---|---|---|---|
| bc1 / bc1-scalar | ui | 1036800 | 62.2 | 35.7/35.2 | 0.99 | 287.3 | 1.22/1.32/1.74 | 0.64/0.72/0.98 | 0.36/0.43/0.55 | 2% |
| bc1 / bc1-ssse3 | ui | 1036800 | 62.2 | 35.7/35.2 | 0.99 | 287.3 | 0.65/0.75/1.01 | 0.32/0.41/0.58 | 0.17/0.26/0.32 | 6% |
| bc1 / bc1-t2d | ui | 1036800 | 62.2 | 35.7/35.2 | 0.99 | 287.3 | 1.69/1.82/2.43 | 0.88/1.02/2.23 | 0.48/0.62/5.61 | 6% |
| bc7 / bc7-avx2 | ui | 2073600 | 124.4 | 36.8/36.2 | 0.76 | 70.5 | 0.79/0.87/0.95 | 0.42/0.49/0.78 | 0.24/0.30/0.38 | 9% |
| bc7 / bc7-scalar | ui | 2073600 | 124.4 | 36.8/36.2 | 0.76 | 70.5 | 1.36/1.48/2.89 | 0.72/0.82/1.35 | 0.41/0.56/4.64 | 3% |
| bc7 / bc7-sse2 | ui | 2073600 | 124.4 | 36.8/36.2 | 0.76 | 70.5 | 1.22/1.30/1.58 | 0.65/0.96/1.73 | 0.37/0.43/0.81 | 3% |
| bc7 / bc7-t2d | ui | 2073600 | 124.4 | 36.8/36.2 | 0.76 | 70.5 | 49.16/51.16/55.87 | 25.40/26.38/31.69 | 13.01/13.81/17.41 | 2% |
| lz4-diff / lz4-diff | ui | 195328 (14% tiles) | 11.7 | lossless | 0.00 | 2.5 | 0.38/0.51/0.62 | 0.30/0.44/0.53 | 0.25/0.40/0.49 | 3% |
| lz4-full / lz4-full | ui | 1183949 | 71.0 | lossless | 0.00 | 4.2 | 1.56/1.77/1.99 | 0.92/1.08/1.85 | 0.51/0.60/0.79 | 3% |
| mjpeg420 / zune | ui | 218337 | 13.1 | 34.3/33.9 | 1.25 | 16.2 | 6.95/7.38/11.32 | - | - | 1% |
| mjpeg420s8 / zune | ui | 222768 | 13.4 | 34.3/33.9 | 1.25 | 16.3 | 6.88/7.22/7.83 | 3.78/4.37/8.36 | 2.13/2.59/4.36 | 2% |
| mjpeg444 / zune | ui | 295992 | 17.8 | 43.7/43.4 | 0.06 | 27.5 | 8.30/8.94/14.57 | - | - | 5% |
| mjpeg444s8 / zune | ui | 300472 | 18.0 | 43.7/43.4 | 0.06 | 27.6 | 8.38/8.87/9.87 | 4.60/4.96/6.41 | 2.64/2.85/4.68 | 5% |
| nv12 / nv12-avx2 | ui | 3110400 | 186.6 | 35.9/35.5 | 1.01 | 3.8 | 0.32/0.40/0.54 | 0.17/0.26/0.47 | 0.09/0.17/0.36 | 5% |
| nv12 / nv12-scalar | ui | 3110400 | 186.6 | 35.9/35.5 | 1.01 | 3.8 | 6.55/7.16/10.57 | 3.36/3.71/5.51 | 1.72/2.01/3.07 | 5% |
| nv12 / nv12-sse2 | ui | 3110400 | 186.6 | 35.9/35.5 | 1.01 | 3.8 | 0.53/0.61/0.69 | 0.28/0.36/0.45 | 0.15/0.23/0.48 | 12% |
| raw-diff / raw-diff | ui | 1160855 (14% tiles) | 69.7 | lossless | 0.00 | 2.5 | 0.04/0.07/0.16 | 0.04/0.06/0.15 | 0.04/0.08/0.14 | 13% |
| raw-full / raw-full | ui | 8294400 | 497.7 | lossless | 0.00 | 0.0 | 0.44/0.58/0.84 | 0.40/0.48/0.65 | 0.42/0.53/2.07 | 4% |
| zstd1-diff / zstd-diff | ui | 149617 (14% tiles) | 9.0 | lossless | 0.00 | 2.7 | 0.32/0.80/0.92 | 0.26/0.65/4.88 | 0.26/0.64/1.50 | 13% |
| zstd1-full / zstd-full | ui | 858215 | 51.5 | lossless | 0.00 | 6.0 | 2.45/2.94/3.50 | 1.39/1.63/1.91 | 0.95/1.02/4.32 | 6% |
| zstd3-diff / zstd-diff | ui | 141059 (14% tiles) | 8.5 | lossless | 0.00 | 3.3 | 0.33/0.91/1.11 | 0.28/0.79/2.51 | 0.28/0.77/0.91 | 16% |
| zstd3-full / zstd-full | ui | 823138 | 49.4 | lossless | 0.00 | 10.0 | 2.54/3.01/3.60 | 1.50/1.84/2.09 | 1.08/1.17/1.55 | 2% |
| bc1 / bc1-scalar | uiscroll | 1036800 | 62.2 | 35.8/35.6 | 0.96 | 249.8 | 1.22/1.43/1.93 | 0.63/0.71/0.95 | 0.38/0.48/0.71 | 8% |
| bc1 / bc1-ssse3 | uiscroll | 1036800 | 62.2 | 35.8/35.6 | 0.96 | 249.8 | 0.65/0.75/0.90 | 0.36/0.43/0.58 | 0.20/0.27/0.47 | 9% |
| bc1 / bc1-t2d | uiscroll | 1036800 | 62.2 | 35.8/35.6 | 0.96 | 249.8 | 1.66/1.75/5.68 | 0.89/0.98/1.29 | 0.50/0.75/4.58 | 5% |
| bc7 / bc7-avx2 | uiscroll | 2073600 | 124.4 | 36.9/36.6 | 0.74 | 66.6 | 0.83/0.93/1.14 | 0.43/0.52/0.67 | 0.24/0.30/1.36 | 3% |
| bc7 / bc7-scalar | uiscroll | 2073600 | 124.4 | 36.9/36.6 | 0.74 | 66.6 | 1.34/1.43/2.19 | 0.72/0.85/1.11 | 0.41/0.49/1.09 | 1% |
| bc7 / bc7-sse2 | uiscroll | 2073600 | 124.4 | 36.9/36.6 | 0.74 | 66.6 | 1.23/1.33/1.50 | 0.66/0.75/1.01 | 0.38/0.45/0.78 | 2% |
| bc7 / bc7-t2d | uiscroll | 2073600 | 124.4 | 36.9/36.6 | 0.74 | 66.6 | 49.52/51.16/57.00 | 25.45/26.63/30.22 | 13.07/15.01/19.47 | 2% |
| lz4-diff / lz4-diff | uiscroll | 997620 (82% tiles) | 59.9 | lossless | 0.00 | 5.4 | 1.94/2.17/2.49 | 1.13/1.32/1.57 | 0.66/0.87/1.10 | 3% |
| lz4-full / lz4-full | uiscroll | 942827 | 56.6 | lossless | 0.00 | 3.6 | 1.53/1.72/2.55 | 0.88/1.01/1.39 | 0.51/0.61/0.76 | 2% |
| mjpeg420 / zune | uiscroll | 209133 | 12.6 | 34.4/34.2 | 1.17 | 16.0 | 6.54/6.86/7.71 | - | - | 0% |
| mjpeg420s8 / zune | uiscroll | 213561 | 12.8 | 34.4/34.2 | 1.17 | 16.0 | 6.70/7.08/8.09 | 3.65/3.97/7.58 | 2.12/2.75/6.59 | 4% |
| mjpeg444 / zune | uiscroll | 279531 | 16.8 | 44.0/43.8 | 0.05 | 27.7 | 8.04/8.48/8.96 | - | - | 2% |
| mjpeg444s8 / zune | uiscroll | 283960 | 17.0 | 44.0/43.8 | 0.05 | 27.1 | 8.04/8.47/9.69 | 4.38/4.69/7.82 | 2.56/3.11/6.42 | 1% |
| nv12 / nv12-avx2 | uiscroll | 3110400 | 186.6 | 35.9/35.5 | 1.00 | 3.7 | 0.33/0.40/0.44 | 0.17/0.25/0.42 | 0.09/0.18/0.47 | 6% |
| nv12 / nv12-scalar | uiscroll | 3110400 | 186.6 | 35.9/35.5 | 1.00 | 3.7 | 6.48/6.80/10.15 | 3.36/3.69/8.50 | 1.74/2.12/4.68 | 3% |
| nv12 / nv12-sse2 | uiscroll | 3110400 | 186.6 | 35.9/35.5 | 1.00 | 3.7 | 0.55/0.63/0.89 | 0.29/0.37/0.87 | 0.15/0.24/0.55 | 9% |
| raw-diff / raw-diff | uiscroll | 6797833 (82% tiles) | 407.9 | lossless | 0.00 | 5.3 | 0.30/0.40/0.54 | 0.21/0.27/0.44 | 0.20/0.48/3.31 | 17% |
| raw-full / raw-full | uiscroll | 8294400 | 497.7 | lossless | 0.00 | 0.0 | 0.42/0.51/0.60 | 0.40/0.46/0.53 | 0.40/0.59/4.75 | 2% |
| zstd1-diff / zstd-diff | uiscroll | 676690 (82% tiles) | 40.6 | lossless | 0.00 | 6.7 | 2.58/2.85/3.35 | 1.57/2.02/3.08 | 1.06/1.26/3.25 | 2% |
| zstd1-full / zstd-full | uiscroll | 666900 | 40.0 | lossless | 0.00 | 5.1 | 2.12/2.28/2.90 | 1.30/1.67/3.18 | 0.92/1.22/3.34 | 4% |
| zstd3-diff / zstd-diff | uiscroll | 641436 (82% tiles) | 38.5 | lossless | 0.00 | 9.9 | 2.96/3.25/3.85 | 1.86/2.34/2.98 | 1.30/1.57/4.72 | 2% |
| zstd3-full / zstd-full | uiscroll | 640496 | 38.4 | lossless | 0.00 | 8.7 | 2.21/2.36/3.25 | 1.44/2.05/6.63 | 0.99/1.30/2.17 | 2% |
| bc1 / bc1-scalar | photo | 1036800 | 62.2 | 43.7/42.2 | 0.00 | 2508.9 | 1.33/1.41/1.60 | 0.70/0.80/1.27 | 0.40/0.47/0.58 | 1% |
| bc1 / bc1-ssse3 | photo | 1036800 | 62.2 | 43.7/42.2 | 0.00 | 2508.9 | 0.75/0.89/1.12 | 0.41/0.52/1.24 | 0.23/0.32/0.59 | 34% |
| bc1 / bc1-t2d | photo | 1036800 | 62.2 | 43.7/42.2 | 0.00 | 2508.9 | 1.83/2.07/9.20 | 0.96/1.17/2.12 | 0.52/0.65/1.10 | 2% |
| bc7 / bc7-avx2 | photo | 2073600 | 124.4 | 47.5/45.6 | 0.00 | 132.3 | 0.81/0.92/1.57 | 0.43/0.55/0.80 | 0.26/0.39/0.51 | 8% |
| bc7 / bc7-scalar | photo | 2073600 | 124.4 | 47.5/45.6 | 0.00 | 132.3 | 1.37/1.57/5.59 | 0.72/0.86/4.74 | 0.41/0.58/2.29 | 3% |
| bc7 / bc7-sse2 | photo | 2073600 | 124.4 | 47.5/45.6 | 0.00 | 132.3 | 1.30/1.44/2.26 | 0.67/0.75/1.16 | 0.38/0.54/1.58 | 8% |
| bc7 / bc7-t2d | photo | 2073600 | 124.4 | 47.5/45.6 | 0.00 | 132.3 | 49.64/51.66/55.80 | 25.41/26.98/31.17 | 13.14/14.05/17.86 | 3% |
| lz4-full / lz4-full | photo | 6101106 | 366.1 | lossless | 0.00 | 13.5 | 3.76/4.59/5.61 | 2.08/2.50/5.47 | 1.13/1.39/1.62 | 1% |
| mjpeg420 / zune | photo | 166410 | 10.0 | 43.0/40.7 | 0.00 | 16.4 | 7.44/7.98/12.18 | - | - | 0% |
| mjpeg420s8 / zune | photo | 170834 | 10.2 | 43.0/40.7 | 0.00 | 16.4 | 7.14/7.84/9.96 | 3.83/4.22/7.47 | 2.22/2.71/6.31 | 3% |
| mjpeg444 / zune | photo | 214676 | 12.9 | 43.7/42.2 | 0.00 | 27.5 | 8.47/9.75/12.03 | - | - | 1% |
| mjpeg444s8 / zune | photo | 219101 | 13.2 | 43.7/42.2 | 0.00 | 27.4 | 8.63/9.92/13.69 | 4.73/5.65/8.68 | 2.58/3.30/5.90 | 7% |
| nv12 / nv12-avx2 | photo | 3110400 | 186.6 | 43.8/41.8 | 0.00 | 3.5 | 0.33/0.41/0.55 | 0.17/0.24/0.27 | 0.09/0.17/0.41 | 6% |
| nv12 / nv12-scalar | photo | 3110400 | 186.6 | 43.8/41.8 | 0.00 | 3.5 | 6.46/6.86/9.21 | 3.29/3.52/7.58 | 1.75/2.49/4.54 | 2% |
| nv12 / nv12-sse2 | photo | 3110400 | 186.6 | 43.8/41.8 | 0.00 | 3.5 | 0.55/0.65/3.71 | 0.28/0.36/0.61 | 0.14/0.23/0.52 | 8% |
| raw-full / raw-full | photo | 8294400 | 497.7 | lossless | 0.00 | 0.0 | 0.45/0.54/0.60 | 0.44/0.52/0.82 | 0.41/0.49/2.08 | 18% |
| zstd1-full / zstd-full | photo | 4603639 | 276.2 | lossless | 0.00 | 21.9 | 7.82/8.75/10.68 | 4.24/4.76/7.47 | 2.41/3.22/6.79 | 1% |
| zstd3-full / zstd-full | photo | 3989498 | 239.4 | lossless | 0.00 | 51.2 | 8.75/10.32/14.89 | 4.77/5.85/9.92 | 2.83/3.36/5.45 | 5% |
| bc1 / bc1-scalar | game | 1036800 | 62.2 | 38.6/38.1 | 0.25 | 2775.5 | 1.29/1.39/5.80 | 0.69/0.74/1.03 | 0.39/0.46/1.22 | 3% |
| bc1 / bc1-ssse3 | game | 1036800 | 62.2 | 38.6/38.1 | 0.25 | 2775.5 | 0.69/0.77/0.81 | 0.37/0.44/0.53 | 0.23/0.32/0.38 | 36% |
| bc1 / bc1-t2d | game | 1036800 | 62.2 | 38.6/38.1 | 0.25 | 2775.5 | 1.68/1.76/5.72 | 0.88/0.96/1.68 | 0.50/0.72/4.74 | 3% |
| bc7 / bc7-avx2 | game | 2073600 | 124.4 | 39.7/39.0 | 0.23 | 134.0 | 0.81/0.92/1.18 | 0.43/0.50/0.68 | 0.28/0.35/0.40 | 8% |
| bc7 / bc7-scalar | game | 2073600 | 124.4 | 39.7/39.0 | 0.23 | 134.0 | 1.36/1.48/1.66 | 0.72/0.84/1.20 | 0.42/0.64/4.58 | 7% |
| bc7 / bc7-sse2 | game | 2073600 | 124.4 | 39.7/39.0 | 0.23 | 134.0 | 1.27/1.38/1.64 | 0.68/0.78/1.12 | 0.38/0.46/0.98 | 3% |
| bc7 / bc7-t2d | game | 2073600 | 124.4 | 39.7/39.0 | 0.23 | 134.0 | 50.27/54.70/73.93 | 25.65/30.64/46.65 | 13.82/17.77/42.42 | 14% |
| lz4-full / lz4-full | game | 8191480 | 491.5 | lossless | 0.00 | 8.0 | 0.67/0.81/1.03 | 0.44/0.60/4.38 | 0.30/0.60/2.59 | 26% |
| mjpeg420 / zune | game | 312933 | 18.8 | 36.4/36.0 | 0.58 | 18.2 | 8.38/8.83/12.10 | - | - | 0% |
| mjpeg420s8 / zune | game | 317345 | 19.0 | 36.4/36.0 | 0.58 | 18.1 | 8.39/8.80/18.16 | 4.49/4.85/5.93 | 2.55/2.80/7.08 | 2% |
| mjpeg444 / zune | game | 449204 | 26.9 | 39.7/39.6 | 0.03 | 30.2 | 10.96/11.56/13.36 | - | - | 2% |
| mjpeg444s8 / zune | game | 453614 | 27.2 | 39.7/39.6 | 0.03 | 30.1 | 11.12/11.56/12.27 | 5.95/6.30/11.18 | 3.28/3.50/7.89 | 1% |
| nv12 / nv12-avx2 | game | 3110400 | 186.6 | 38.0/37.6 | 0.50 | 3.4 | 0.34/0.41/0.69 | 0.17/0.25/0.32 | 0.09/0.17/0.41 | 4% |
| nv12 / nv12-scalar | game | 3110400 | 186.6 | 38.0/37.6 | 0.50 | 3.4 | 6.53/6.86/9.48 | 3.37/3.55/5.01 | 1.70/1.94/3.56 | 2% |
| nv12 / nv12-sse2 | game | 3110400 | 186.6 | 38.0/37.6 | 0.50 | 3.4 | 0.54/0.62/0.82 | 0.28/0.36/0.50 | 0.15/0.23/0.53 | 5% |
| raw-full / raw-full | game | 8294400 | 497.7 | lossless | 0.00 | 0.0 | 0.49/0.58/0.83 | 0.42/0.48/0.70 | 0.42/0.49/1.12 | 20% |
| zstd1-full / zstd-full | game | 6614274 | 396.9 | lossless | 0.00 | 11.0 | 6.38/6.96/9.97 | 3.44/3.73/4.95 | 1.92/2.20/8.84 | 5% |
| zstd3-full / zstd-full | game | 6318096 | 379.1 | lossless | 0.00 | 45.2 | 7.77/8.39/10.06 | 4.07/4.44/9.05 | 2.33/2.57/7.03 | 3% |
| bc1 / bc1-scalar | mixed | 1036800 | 62.2 | 36.3/35.7 | 0.77 | 945.8 | 1.28/1.36/1.77 | 0.67/0.78/2.66 | 0.38/0.55/1.02 | 3% |
| bc1 / bc1-ssse3 | mixed | 1036800 | 62.2 | 36.3/35.7 | 0.77 | 945.8 | 0.68/0.81/1.09 | 0.36/0.49/0.62 | 0.23/0.33/2.27 | 6% |
| bc1 / bc1-t2d | mixed | 1036800 | 62.2 | 36.3/35.7 | 0.77 | 945.8 | 1.70/1.82/3.00 | 0.88/0.93/1.31 | 0.49/0.59/2.24 | 2% |
| bc7 / bc7-avx2 | mixed | 2073600 | 124.4 | 37.4/36.7 | 0.60 | 86.6 | 0.80/0.86/1.00 | 0.44/0.53/1.03 | 0.27/0.38/2.63 | 5% |
| bc7 / bc7-scalar | mixed | 2073600 | 124.4 | 37.4/36.7 | 0.60 | 86.6 | 1.37/1.50/1.75 | 0.73/0.82/0.99 | 0.41/0.53/0.91 | 2% |
| bc7 / bc7-sse2 | mixed | 2073600 | 124.4 | 37.4/36.7 | 0.60 | 86.6 | 1.25/1.33/1.86 | 0.66/0.74/0.98 | 0.38/0.43/0.62 | 1% |
| bc7 / bc7-t2d | mixed | 2073600 | 124.4 | 37.4/36.7 | 0.60 | 86.6 | 50.19/51.75/56.19 | 25.48/26.76/33.46 | 13.06/14.54/20.25 | 2% |
| lz4-diff / lz4-diff | mixed | 2333671 (48% tiles) | 140.0 | lossless | 0.00 | 4.1 | 0.81/0.96/1.12 | 0.47/0.59/3.48 | 0.36/0.46/0.68 | 5% |
| lz4-full / lz4-full | mixed | 3068548 | 184.1 | lossless | 0.00 | 4.4 | 1.49/1.64/1.97 | 0.81/0.97/6.22 | 0.45/0.64/1.49 | 3% |
| mjpeg420 / zune | mixed | 250645 | 15.0 | 34.6/34.1 | 1.10 | 16.5 | 7.45/7.80/10.72 | - | - | 1% |
| mjpeg420s8 / zune | mixed | 255073 | 15.3 | 34.6/34.1 | 1.10 | 16.6 | 7.46/7.80/9.57 | 4.12/4.38/8.97 | 2.48/2.87/6.00 | 2% |
| mjpeg444 / zune | mixed | 345174 | 20.7 | 42.3/42.1 | 0.04 | 28.0 | 9.36/9.77/11.72 | - | - | 2% |
| mjpeg444s8 / zune | mixed | 349588 | 21.0 | 42.3/42.1 | 0.04 | 27.8 | 9.17/9.43/10.86 | 5.13/5.49/9.73 | 3.08/3.33/6.83 | 3% |
| nv12 / nv12-avx2 | mixed | 3110400 | 186.6 | 36.4/35.8 | 0.80 | 2.6 | 0.33/0.41/2.09 | 0.17/0.26/0.31 | 0.10/0.19/2.17 | 11% |
| nv12 / nv12-scalar | mixed | 3110400 | 186.6 | 36.4/35.8 | 0.80 | 2.6 | 6.39/6.75/8.02 | 3.38/3.96/6.02 | 1.74/2.37/6.04 | 3% |
| nv12 / nv12-sse2 | mixed | 3110400 | 186.6 | 36.4/35.8 | 0.80 | 2.6 | 0.53/0.60/0.62 | 0.28/0.38/4.53 | 0.17/0.27/0.46 | 36% |
| raw-diff / raw-diff | mixed | 4048819 (48% tiles) | 242.9 | lossless | 0.00 | 2.2 | 0.18/0.26/0.33 | 0.13/0.21/0.45 | 0.12/0.22/1.26 | 15% |
| raw-full / raw-full | mixed | 8294400 | 497.7 | lossless | 0.00 | 0.0 | 0.46/0.62/3.33 | 0.41/0.48/0.55 | 0.42/0.63/0.88 | 7% |
| zstd1-diff / zstd-diff | mixed | 1850187 (48% tiles) | 111.0 | lossless | 0.00 | 5.8 | 2.78/3.14/5.70 | 1.81/2.10/2.36 | 1.37/1.71/4.26 | 7% |
| zstd1-full / zstd-full | mixed | 2410503 | 144.6 | lossless | 0.00 | 8.6 | 3.82/4.16/5.71 | 2.09/2.54/3.23 | 1.31/1.60/5.00 | 2% |
| zstd3-diff / zstd-diff | mixed | 1768357 (48% tiles) | 106.1 | lossless | 0.00 | 9.9 | 3.09/3.36/4.22 | 1.98/2.43/2.81 | 1.47/1.88/4.16 | 2% |
| zstd3-full / zstd-full | mixed | 2322373 | 139.3 | lossless | 0.00 | 17.8 | 4.13/4.53/5.22 | 2.29/2.67/6.52 | 1.45/1.91/5.85 | 4% |

Host-channel budget for reference: the measured virtio-serial link is ~216-253 MiB/s = ~226-265 MB/s, i.e.
at 60 fps about 3.8-4.4 MB per 1080p frame (raw 1080p is 8.3 MB; raw 720p is 3.7 MB, 221 MB/s at 60 fps).

### Native Linux host reference (same decode code, upper bound; fastest variants only)

Single run, `Vec` output, 220 samples per cell, other load on the machine.

Native Linux host reference (same decode code):

| codec / decoder | class | host 1T p50/p95/max ms | 2T | 4T |
|---|---|---|---|---|
| bc1 / bc1-ssse3 | game | 0.69/0.88/1.09 | 0.36/0.48/0.60 | 0.20/0.28/0.38 |
| bc7 / bc7-avx2 | game | 0.80/0.85/1.21 | 0.40/0.47/0.59 | 0.34/0.35/0.39 |
| lz4-full / lz4-full | game | 0.71/0.94/1.49 | 0.48/0.68/1.00 | 0.30/0.39/0.60 |
| mjpeg420 / zune | game | 7.85/8.12/8.67 | - | - |
| mjpeg444 / zune | game | 10.24/10.93/12.59 | - | - |
| nv12 / nv12-avx2 | game | 0.34/0.39/0.66 | 0.19/0.24/0.29 | 0.11/0.13/0.23 |
| raw-full / raw-full | game | 0.39/0.58/0.81 | 0.34/0.52/0.88 | 0.31/0.49/2.43 |
| zstd1-full / zstd-full | game | 5.25/6.22/8.00 | 2.90/3.83/4.11 | 1.96/2.54/3.00 |
| bc1 / bc1-ssse3 | mixed | 0.69/1.00/1.12 | 0.34/0.47/0.53 | 0.26/0.26/0.29 |
| bc7 / bc7-avx2 | mixed | 0.83/0.87/1.02 | 0.41/0.54/0.69 | 0.22/0.32/0.43 |
| lz4-diff / lz4-diff | mixed | 0.78/0.98/1.44 | 0.46/0.60/0.62 | 0.38/0.45/0.51 |
| lz4-full / lz4-full | mixed | 1.40/1.48/1.56 | 0.74/0.87/1.00 | 0.41/0.51/0.55 |
| mjpeg420 / zune | mixed | 7.00/7.32/7.67 | - | - |
| mjpeg444 / zune | mixed | 8.72/9.08/11.27 | - | - |
| nv12 / nv12-avx2 | mixed | 0.34/0.36/1.12 | 0.17/0.19/0.34 | 0.09/0.16/1.00 |
| raw-diff / raw-diff | mixed | 0.15/0.20/0.31 | 0.12/0.16/0.25 | 0.11/0.14/0.19 |
| raw-full / raw-full | mixed | 0.30/0.43/0.68 | 0.20/0.25/0.41 | 0.19/0.23/0.31 |
| zstd1-diff / zstd-diff | mixed | 2.19/2.78/7.09 | 1.47/1.89/2.39 | 1.06/1.47/3.05 |
| zstd1-full / zstd-full | mixed | 3.23/3.54/5.01 | 1.74/1.93/3.11 | 1.12/1.36/2.59 |
| bc1 / bc1-ssse3 | photo | 0.69/0.75/0.89 | 0.36/0.46/0.67 | 0.19/0.25/0.27 |
| bc7 / bc7-avx2 | photo | 0.79/0.89/1.03 | 0.40/0.62/4.66 | 0.22/0.41/0.72 |
| lz4-full / lz4-full | photo | 3.43/3.96/6.33 | 1.85/2.00/2.03 | 1.13/1.34/1.62 |
| mjpeg420 / zune | photo | 6.74/7.30/7.59 | - | - |
| mjpeg444 / zune | photo | 8.18/9.47/11.80 | - | - |
| nv12 / nv12-avx2 | photo | 0.32/0.34/0.35 | 0.17/0.19/0.20 | 0.09/0.10/0.13 |
| raw-full / raw-full | photo | 0.22/0.26/0.28 | 0.18/0.24/0.43 | 0.18/0.21/0.38 |
| zstd1-full / zstd-full | photo | 6.41/7.73/10.17 | 3.38/3.86/6.59 | 1.93/2.79/2.91 |
| bc1 / bc1-ssse3 | ui | 0.62/0.65/0.72 | 0.33/0.36/0.50 | 0.18/0.22/0.26 |
| bc7 / bc7-avx2 | ui | 0.78/0.80/0.99 | 0.41/0.46/0.59 | 0.21/0.26/0.30 |
| lz4-diff / lz4-diff | ui | 0.36/0.47/0.54 | 0.27/0.40/0.43 | 0.24/0.39/0.42 |
| lz4-full / lz4-full | ui | 1.49/1.57/1.87 | 0.85/0.94/1.17 | 0.47/0.56/0.57 |
| mjpeg420 / zune | ui | 6.44/6.67/6.78 | - | - |
| mjpeg444 / zune | ui | 7.89/8.08/8.34 | - | - |
| nv12 / nv12-avx2 | ui | 0.33/0.35/0.43 | 0.17/0.19/0.20 | 0.10/0.11/0.14 |
| raw-diff / raw-diff | ui | 0.04/0.06/0.10 | 0.03/0.06/0.22 | 0.03/0.05/0.07 |
| raw-full / raw-full | ui | 0.31/0.33/0.36 | 0.20/0.23/0.25 | 0.20/0.25/0.34 |
| zstd1-diff / zstd-diff | ui | 0.26/0.64/0.69 | 0.19/0.50/0.67 | 0.21/0.52/0.69 |
| zstd1-full / zstd-full | ui | 1.98/2.37/2.64 | 1.10/1.28/1.32 | 0.81/0.85/0.89 |
| bc1 / bc1-ssse3 | uiscroll | 0.66/0.78/1.09 | 0.34/0.39/0.46 | 0.18/0.21/0.27 |
| bc7 / bc7-avx2 | uiscroll | 0.78/0.82/0.84 | 0.41/0.46/0.52 | 0.21/0.25/0.29 |
| lz4-diff / lz4-diff | uiscroll | 1.81/2.02/2.38 | 1.08/1.19/1.36 | 0.64/0.78/0.88 |
| lz4-full / lz4-full | uiscroll | 1.45/1.56/1.72 | 0.82/0.93/1.37 | 0.48/0.56/0.66 |
| mjpeg420 / zune | uiscroll | 6.63/7.14/7.85 | - | - |
| mjpeg444 / zune | uiscroll | 7.68/7.95/8.43 | - | - |
| nv12 / nv12-avx2 | uiscroll | 0.32/0.33/0.33 | 0.16/0.23/0.44 | 0.10/0.16/1.22 |
| raw-diff / raw-diff | uiscroll | 0.26/0.29/0.31 | 0.18/0.30/0.37 | 0.16/0.18/0.23 |
| raw-full / raw-full | uiscroll | 0.29/0.33/0.60 | 0.19/0.22/0.24 | 0.18/0.21/0.24 |
| zstd1-diff / zstd-diff | uiscroll | 2.17/2.26/2.37 | 1.35/1.66/2.35 | 0.89/1.03/1.14 |
| zstd1-full / zstd-full | uiscroll | 1.84/1.91/2.85 | 1.11/1.37/1.54 | 0.74/0.85/1.00 |

### Chain tail inside the guest (`codecbench tail`, 300 samples after 20 warm-up, ms p50/p95/max)

| step | session 0 (SYSTEM, `vm exec`, no desktop) | session 1 (console `builder`, interactive 1280x720) |
|---|---|---|
| (i) memcpy 8 MiB BGRA (16 rotating 8 MiB sources) into the DIB section | 0.40 / 0.53 / 1.57 | 0.42 / 0.56 / 2.73 |
| (ii) BitBlt DIB -> memory DC (second 32 bpp DIB), + `GdiFlush` | 0.16 / 0.25 / 0.44 | 0.17 / 0.28 / 1.20 |
| (ii') BitBlt DIB -> device-compatible bitmap in a memory DC | 0.17 / 0.24 / 0.26 | 0.16 / 0.30 / 1.26 |
| (iii) BitBlt DIB -> visible popup window DC | not possible (session 0 has no usable desktop DC; GDI rejects the blit) | 0.24 / 0.33 / 1.25 (see caveat) |

What I could do: (i), (ii), (ii') in both sessions, and (iii) after finding that the build VM has an
**interactive desktop** (`query user`: `builder`, `console`, session 1, Active; I did not touch auto-logon).
`vm exec` runs in session 0, so (iii) was run in session 1 through a one-shot Scheduled Task registered for
`builder` (interactive logon type) and unregistered afterwards. The decode benchmark itself was also repeated
for the `ui` class in session 1: raw-full 0.42 ms, nv12-avx2 0.34 ms, bc7-avx2 0.86 ms, bc1-ssse3 0.67 ms,
lz4-full 1.57 ms, zstd1-full 2.59 ms p50 at 1 thread, i.e. the same as session 0 within run noise.

## Observations

- **Raw copy is not the problem on the CPU, the link is.** memcpy of 8.3 MB into a DIB takes ~0.4-0.45 ms
  (~19 GB/s); with the tail BitBlt it is ~0.6-0.7 ms per frame, but raw 1080p60 is 498 MB/s, 2x the channel.
  Raw `diff` is excellent for `ui` (0.04 ms, 70 MB/s average) but its worst case equals `raw-full` plus
  index overhead (`uiscroll`/`game`: 408-498 MB/s).
- **NV12 with AVX2 costs 0.33 ms p50 at 1 thread** (0.09 ms at 4 threads), 187 MB/s at 1080p60, but it is
  lossy on text (PSNR 35.9 dB on `ui`, 1 % of pixels off by >24 levels: coloured fringes on thin text).
  Scalar is 6.6 ms (20x slower) and SSE2 0.53 ms, so SIMD is mandatory; the guest has AVX2.
- **BC1 / BC7 decode is nearly as cheap**: BC1 SSSE3 0.65 ms, BC7(mode 6) AVX2 0.8 ms at 1 thread, ~0.2-0.3 ms
  at 4. BC1 is 62 MB/s, BC7 124 MB/s, constant size. Generic `texture2ddecoder` BC7 is 49 ms/frame (60x slower)
  because it handles all 8 modes per block: a fixed-mode, SIMD decoder is required, not a generic crate.
  Quality: BC1 35.7 dB and BC7-mode-6 36.8 dB on `ui` (text edges get BC block fringing, similar to NV12);
  43.7-47.5 dB on the smooth `photo` set; 38.6 / 39.7 dB on noisy `game`. My BC7 encoder is minimal (one
  subset only); a real multi-mode BC7 encoder on the GPU would be better on text. **Unverified.**
- **LZ4 decode** is 1.5 ms (`ui`, ~1 MB/frame, 71 MB/s) and 0.4 ms for the diff variant (14 % tiles, 12 MB/s);
  on noise (`game`) it degrades to ~8.2 MB/frame (no compression, 0.67 ms but 492 MB/s on the wire).
  **zstd** compresses better (`ui` 0.82-0.86 MB, `photo` 4.6 MB, `game` 6.6 MB) but costs 2.5 ms (`ui`) to
  7.8 ms (`photo`) / 6.4 ms (`game`) per frame at 1 thread; level 3 decodes no faster than level 1 and
  saves ~4 %. Lossless is the only way to get bit-exact text, and for `ui`/`mixed` the diff variants
  make it very cheap (0.3-0.8 ms and 12-140 MB/s) but for the gaming/video-like classes it exceeds the channel.
- **MJPEG (zune-jpeg)** is 7-8.4 ms at 1 thread for 4:2:0 / 4:4:4 on `ui` (8.4-11 ms on `game`); 2.1-2.6 ms at 4 threads
  with 8 strips. It has the smallest streams (10-27 MB/s) and 4:4:4 q90 is good on text (43.7 dB, 0.06 % bad
  pixels) but 4:2:0 smears text like NV12 (34.3 dB). A single-frame JPEG cannot use more than one vCPU.
- **Noise**: sub-millisecond p50 values vary between runs A/B by 0-36 % (`run spread`; the largest spreads are
  on the BC/memcpy rows whose absolute values are 0.2-0.8 ms) while the p95/max columns show occasional
  1-5 ms hiccups (vCPU preemption, host load). Memory-bound work (raw memcpy) does not scale with threads
  (0.40-0.44 ms at 1, 2 and 4 threads), compute-bound work (NV12, BC) scales well.

## Recommendation (4 vCPU guest)

Budgets: 1080p60 = 16.7 ms/frame, 1080p30 = 33 ms, 720p60 = 16.7 ms for 0.44x the pixels (decode costs
scale about linearly with pixel count, so 720p costs ~0.44x of the 1080p numbers; same for bandwidth).
The channel floor of 226 MB/s allows 3.8 MB/frame at 1080p60 and 7.5 MB/frame at 1080p30 (720p60: 3.8 MB/frame
of 0.44x the pixels, so raw 720p at 3.7 MB just fits).
Decode CPU = ms x fps / 1000 of one vCPU; add ~0.6 ms for memcpy + BitBlt (tail) per frame.

| class | 1080p60 | 1080p30 | 720p60 |
|---|---|---|---|
| `ui` / `uiscroll` / `mixed` (desktop-like) | **diff tiles + LZ4/zstd1 (lossless)** for `ui`/`mixed`: 0.4-0.8 ms, 12-140 MB/s; for full-screen scroll (`uiscroll`) fall back to **zstd1/lz4 full 1.5-2.5 ms, 40-57 MB/s**, still < 15 % of one vCPU. If bit-exact is not required, BC7 (0.8 ms, 124 MB/s) or NV12 are alternatives | same, zstd level 1 plenty | same, 2x headroom |
| `photo` (smooth, wallpapers/video-like) | **BC7 or BC1** (0.7-0.8 ms, 62-124 MB/s, 43-47 dB) or **NV12 AVX2** (0.33 ms, 187 MB/s, 43.8 dB); lossless does not fit (LZ4 6.1 MB, zstd 4.6 MB/frame) | same; lossless zstd1 (7.8 ms, 138 MB/s) would just fit | NV12/BC fine; zstd1 fits (3.5 ms, 125 MB/s) |
| `game` (noisy, hard edges) | **BC7** (0.8 ms, 124 MB/s, 39.7 dB), NV12 (0.34 ms, 187 MB/s, 38.0 dB) or BC1 (0.7 ms, 62 MB/s, 38.6 dB); lossless needs 400-490 MB/s: no | BC7/NV12; zstd1 6.4 ms x 30 = 19 % of one vCPU but 200 MB/s of the channel | BC7/NV12; zstd1 lossless (~2.8 ms, 176 MB/s) fits |

Short list: (1) **tile-diff + LZ4 (or zstd 1)** as the default lossless mode for desktop content,
(2) **BC7** fixed-mode (SIMD decoder shipped here: AVX2 0.8 ms, SSE2 1.2 ms, scalar 1.3 ms) as the lossy
mode for photo/game, chosen per frame or per tile by the host; BC1 if the bandwidth, not the quality,
limits; (3) NV12 if the host shader can only write one cheap format, but expect text fringing; (4) MJPEG only
as the 4:4:4 low-bandwidth fallback with 4-8 strips (2-3 ms at 4 threads) when the link is the bottleneck;
(5) raw tiles only for tiny updates. None of the candidates needs more than ~5 % of one vCPU except
zstd on high-entropy frames and MJPEG (~40-50 % of one vCPU at 60 fps on a single thread).

## Stretch: H.264 through Media Foundation (item 7)

**Skipped.** `ffmpeg`, `x264` and `gst-launch-1.0` are not installed on the host (`which`/`ffmpeg -encoders`
find nothing), no system packages were installed and no Annex-B stream could be produced. Not measured, no
estimate given.

## Caveats and unverified

- All host encode times are CPU, single thread per frame (texpresso BC1 cluster-fit is 250-2800 ms per
  frame; the BC7 mode-6 encoder 70-135 ms; MJPEG 16-30 ms; NV12 3-4 ms; LZ4 4-13 ms; zstd 6-22 ms). The GPU
  encoder is not simulated; quality numbers are those of these CPU encoders (a GPU BC7/BC1 encoder will
  differ; BC7 uses one mode only).
- PSNR is per-frame on B,G,R of the synthetic frames; I did not look at the pictures, so "visible artefacts on
  text" is inferred from PSNR on `ui` and the share of pixels with >24 levels error. `photo` is smoother than
  real photos and `game` noise is synthetic: lossless ratios and PSNR there are not representative.
- Timings are 1 VM boot, 2 passes for all classes + 1 extra `ui` pass in another session, ~220 samples; p95
  and max are affected by vCPU scheduling on a loaded host (the host also runs the user's LTSC VM). Absolute
  numbers on the user's LTSC guest may be higher when its own desktop work competes for the 4 vCPUs.
- BitBlt to DIB/DDB memory DCs at 0.16 ms (8 MB in 0.16 ms = ~50 GB/s) is far faster than the 0.4 ms
  memcpy from a rotating 128 MiB set; both source and destination fit the 5900X's L3 there, so this is the
  warm-cache cost. The visible-window BitBlt went to a 1280x720 console screen, so it only copied 0.44x of
  the pixels (clipped) and to a VGA display whose real scan-out/present cost (QEMU display, DWM
  composition, host side) is **not** included; GdiFlush only waits for GDI. Treat (iii) as a lower bound.
- The decode `Vec`-vs-DIB question: the guest used a real DIB section; the Linux reference used a `Vec`.
- The pool spins up to ~20k iterations before yielding; on a 4 vCPU guest with other work running, spinning
  waiters can steal time from the application: real usage should be measured with the actual guest agent
  running.
- 2-thread/4-thread numbers use all 4 vCPUs of the VM at once; with the guest desktop also running, real
  speed-ups will be lower.
- The decoders assume AVX2 for the fastest path (runtime detected; the VM exposes sse2/ssse3/sse4.1/avx/avx2,
  no AVX-512). Other CPUs/hypervisors (e.g. without `-cpu host`) may fall back to the SSE2/scalar rows.

## Reproduce

```
# host: 240 frames per class (6.2 GB of streams; use --frames 60 and --classes for a smaller run)
cargo build --release --manifest-path experiments/codecbench/Cargo.toml
codecbench gen --out DATA --shots s1.png,s2.png,... --walls w1.jpg,w2.png,... --frames 240
codecbench decode --data DATA --out host.jsonl --label host        # native reference
# guest (Windows VM): cargo build --release --no-default-features   (decode/tail/report only)
codecbench.exe decode --data C:\work\data --classes ui --out res.jsonl --label guest
codecbench.exe tail --out tail.jsonl
codecbench report --gen DATA\gen.jsonl --guest resA.jsonl,resB.jsonl --host host.jsonl
```

Used inputs: 5 PNG screenshots (`s1-explorer`, `s4-notepad`, `s11-downloads`, `s3-explorer-deep`, `/tmp/g1.png`; the first is
the static background, the others form the scrolled document) and the 4 wallpapers `adwaita-l.jpg`,
`vnc-d.png`, `symbolic-l.png`, `adwaita-d.jpg` (the `photo` class uses these four).

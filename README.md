# teapress

[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue)](LICENSE) [![Rust: 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange)](Cargo.toml)

[中文版](README.zh.md) | [English](README.md)

**A simple, fast, local image compression CLI for PNG, JPEG and WebP, built for developers and AI agents.**

## Install

```sh
curl -fsSL https://github.com/teash1rt/teapress/releases/latest/download/teapress-installer.sh | sh

# Update
teapress update
```

## Usage

**By default, teapress replaces the original files in place, without a backup.** Names and formats are preserved.

```sh
# It will compress photo.png in place
teapress photo.png

# It will compress three images in place, one at a time
teapress photo.jpg banner.png icon.webp

# It will save a compressed copy to output/ and keep the original
teapress photo.png -o output/
```

| Argument / option | Behavior |
| --- | --- |
| `<INPUTS>...` | One or more image paths |
| `update` | Check for and install the latest stable release |
| `-o, --outdir <DIR>` | Save copies instead; never overwrite existing files |
| `-l, --lossless` | Preserve decoded pixels; metadata and file bytes may change |
| `-h, --help` | Show all options and output conventions |
| `-v, --version` | Show version |

Static `.png`, `.jpg`, `.jpeg` and `.webp` files only, up to 25 MiB and 20 million pixels each. Files are processed sequentially; directories are not traversed.

Results go to stdout as one JSON array after the batch completes. Example:

```json
[
  {
    "input": "icon.png",
    "output": "/images/icon.png",
    "before": 253,
    "after": 96,
    "dssim": 0.0,
    "dssim_peak": 0.0,
    "dssim_mode": "exact",
    "detail": "PNG lossless",
    "ms": 1
  }
]
```

| Field | Meaning |
| --- | --- |
| `before`, `after` | Original and compressed sizes in bytes |
| `ms` | Compression time in milliseconds, excluding file I/O |
| `dssim`, `dssim_peak` | Average and peak distortion; lower is better |
| `error` | `{code, message}` on failure, alongside `input`; the batch continues |

| Exit code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Processing or output failure |
| `2` | Invalid usage |

## Benchmark

Compared with actual [TinyPNG](https://tinypng.com/) website outputs on **236 opaque PNGs**: 150 from CLIC and 86 from Wikimedia Commons. Same original files, default compression, no format conversion.

| Metric | teapress | TinyPNG |
| --- | ---: | ---: |
| Total output, from 599.02 MiB | 206.73 MiB | 202.82 MiB |
| Size reduction | 65.49% | 66.14% |
| Mean image DSSIM ↓ | 0.000686 | 0.001021 |
| Mean worst-tile DSSIM ↓ | 0.001124 | 0.001798 |

**1.9% larger overall, with lower average measured distortion.** Both DSSIM scores were no worse than TinyPNG on 235 of 236 images.

Quality was scored independently after color conversion to sRGB, using full-resolution 512×512 tiles for images above 1 MP. These are historical measurements, not a guarantee of visual equivalence or results on other formats and transparent images.

[Per-image results](validation/benchmark.json) · [Methodology and environment](validation/benchmark.meta.json)

## License

[AGPL-3.0-only](LICENSE) Referenced benchmark images retain their original licenses

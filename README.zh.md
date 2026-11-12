# teapress

[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue)](LICENSE) [![Rust: 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange)](Cargo.toml)

[中文版](README.zh.md) | [English](README.md)

**简洁、快速、本地运行的图片压缩 CLI，支持 PNG、JPEG 和 WebP，为开发者和 AI Agent 而构建。**

## 安装

TODO: Homebrew 安装

## 用法

**默认原地替换原图，不创建备份。** 保留文件名和格式。

```sh
# 压缩并原地替换 photo.png
teapress photo.png

# 按顺序压缩并原地替换三张图片
teapress photo.jpg banner.png icon.webp

# 将压缩副本保存到 output/ 保留原图
teapress photo.png -o output/
```

| 参数 | 行为 |
| --- | --- |
| `<INPUTS>...` | 一个或多个图片路径 |
| `-o, --outdir <DIR>` | 另存副本，不覆盖已有文件 |
| `-l, --lossless` | 保持解码后的像素不变，元数据和文件字节可能变化 |
| `-h, --help` | 查看全部参数和输出约定 |
| `-v, --version` | 查看版本 |

仅支持静态 `.png`、`.jpg`、`.jpeg` 和 `.webp` 图片，每张不超过 25 MiB、2000 万像素。多张图片串行处理，不遍历目录。

整批处理完成后，向标准输出返回一个 JSON 数组。示例：

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

| 字段 | 含义 |
| --- | --- |
| `before`、`after` | 原图和压缩后的体积，单位为字节 |
| `ms` | 压缩耗时，单位为毫秒，不含文件读写 |
| `dssim`、`dssim_peak` | 平均和峰值失真，越低越好 |
| `error` | 失败时与 `input` 一起返回 `{code, message}`，其余图片继续处理 |

| 退出码 | 含义 |
| --- | --- |
| `0` | 成功 |
| `1` | 处理或输出失败 |
| `2` | 参数错误 |

## Benchmark

使用 **236 张不透明 PNG** 与 [TinyPNG](https://tinypng.com/) 网站实际输出对比：150 张来自 CLIC，86 张来自 Wikimedia Commons。使用相同原图、默认压缩策略，不转换格式。

| 指标 | teapress | TinyPNG |
| --- | ---: | ---: |
| 输出总体积，原图共 599.02 MiB | 206.73 MiB | 202.82 MiB |
| 体积减少 | 65.49% | 66.14% |
| 逐图 DSSIM 均值 ↓ | 0.000686 | 0.001021 |
| 逐图最差分块 DSSIM 均值 ↓ | 0.001124 | 0.001798 |

**总体积大 1.9%，平均测量失真更低。** 236 张图片中有 235 张的两项 DSSIM 指标均不高于 TinyPNG。

画质评分在转为 sRGB 后独立计算，超过 100 万像素的图片按原始分辨率划分为 512×512 分块。这些历史数据不保证视觉无差异，也不代表其他格式和透明图片的表现。

[逐图结果](validation/benchmark.json) · [测试方法与环境](validation/benchmark.meta.json)

## 许可证

[AGPL-3.0-only](LICENSE) 引用的 benchmark 图片保留各自的原始许可证

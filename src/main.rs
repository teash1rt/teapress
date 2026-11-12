mod codec;
mod compress;
mod png_strategy;
mod quality;

use crate::compress::{MAX_BYTES, compress};
use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::json;
use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    version,
    disable_version_flag = true,
    arg(clap::Arg::new("version").short('v').long("version").action(clap::ArgAction::Version).help("Print version")),
    about = "Compress PNG, JPEG and WebP locally",
    after_help = "Balanced lossy compression; replaces inputs in place, no backup. Use -o to save copies.\nStatic images only; max 25 MiB and 20 million pixels per image. No directory traversal.\nRepeated canonical input paths return DUPLICATE_INPUT, including symlink aliases.\nStdout: one compact JSON array after the batch completes.\nSuccess: input/output paths, before/after bytes, ms (compression only),\ndssim/dssim_peak (lower is better), dssim_mode, detail.\nFailure: input, error {code, message}. Batches continue after errors.\nUsage errors: JSON array without input. Help/version: plain text.\nExit: 0 success, 1 failure, 2 usage."
)]
struct Cli {
    // 按顺序逐张处理图片
    #[arg(required = true, help = "Input images, processed sequentially")]
    inputs: Vec<PathBuf>,
    // 默认原地替换 指定目录时另存且不覆盖已有文件
    #[arg(
        short = 'o',
        long,
        help = "Save to a directory without overwriting existing files; default: replace inputs"
    )]
    outdir: Option<PathBuf>,
    // 无损只保证解码像素不变 文件字节和元数据可能变化
    #[arg(
        short = 'l',
        long,
        help = "Preserve decoded pixels; file bytes and metadata may change"
    )]
    lossless: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            // 标准输出不可写时 改用标准错误报告
            eprintln!(
                "{}",
                json!([{"error":{"code":"STDOUT_WRITE_FAILED","message":error.to_string()}}])
            );
            ExitCode::FAILURE
        }
    }
}

fn emit(writer: &mut impl Write, value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn run() -> Result<u8> {
    let mut stdout = io::stdout().lock();
    let Cli {
        inputs,
        outdir,
        lossless,
    } = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => match error.kind() {
            clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                error.exit()
            }
            _ => {
                emit(
                    &mut stdout,
                    &json!([{"error":{"code":"CLI_PARSE_ERROR","message":error.to_string()}}]),
                )?;
                return Ok(2);
            }
        },
    };
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let mut failed = false;
    for input in inputs {
        let mut code = "INPUT_READ_FAILED";
        let result = (|| -> Result<serde_json::Value> {
            let metadata = fs::metadata(&input)?;
            code = "INVALID_INPUT";
            if !metadata.is_file() {
                bail!("Input must be a regular image file; directories are not supported");
            }
            let name = input
                .file_name()
                .and_then(|x| x.to_str())
                .context("Filename is not valid UTF-8")?;
            code = "INPUT_READ_FAILED";
            let canonical = fs::canonicalize(&input)?;
            // 同一路径及符号链接别名只处理一次 避免重复有损压缩
            if !seen.insert(canonical.clone()) {
                code = "DUPLICATE_INPUT";
                bail!("Input resolves to a file already attempted in this batch");
            }
            let file = fs::File::open(&input)?;
            let permissions = file.metadata()?.permissions();
            let mut bytes = Vec::new();
            file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
            // 替换符号链接指向的文件 保留链接本身
            let destination = match &outdir {
                Some(dir) => {
                    code = "OUTPUT_WRITE_FAILED";
                    fs::create_dir_all(dir)?;
                    dir.join(name)
                }
                None => canonical,
            };
            if outdir.is_some() && destination.exists() {
                code = "OUTPUT_EXISTS";
                bail!(
                    "Output already exists; refusing to overwrite: {}",
                    destination.display()
                );
            }
            code = "COMPRESSION_FAILED";
            let output = compress(&bytes, name, lossless)?;
            // 原样保留就不写文件
            // 需要替换时先在同一文件系统写完临时文件 再原子替换
            if outdir.is_some() || !output.retained {
                code = "OUTPUT_WRITE_FAILED";
                let mut target = tempfile::NamedTempFile::new_in(
                    destination
                        .parent()
                        .context("Output path has no parent directory")?,
                )?;
                target.write_all(&output.bytes)?;
                target.as_file().set_permissions(permissions)?;
                target.as_file().sync_all()?;
                if outdir.is_some() {
                    target
                        .persist_noclobber(&destination)
                        .inspect_err(|error| {
                            if error.error.kind() == io::ErrorKind::AlreadyExists {
                                code = "OUTPUT_EXISTS";
                            }
                        })?;
                } else {
                    target.persist(&destination)?;
                }
            }
            let mut row = serde_json::to_value(&output)?;
            row["input"] = json!(input);
            row["output"] = json!(destination);
            Ok(row)
        })();
        let row = match result {
            Ok(row) => row,
            Err(error) => {
                failed = true;
                if code == "INPUT_READ_FAILED"
                    && error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
                {
                    code = "INPUT_NOT_FOUND";
                }
                json!({"input":input,"error":{"code":code,"message":error.to_string()}})
            }
        };
        rows.push(row);
    }
    emit(&mut stdout, &rows)?;
    Ok(u8::from(failed))
}

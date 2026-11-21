mod codec;
mod compress;
mod png_strategy;
mod quality;

use crate::compress::{MAX_BYTES, compress, supported_format};
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

#[derive(Parser)]
#[command(
    version,
    disable_version_flag = true,
    arg_required_else_help = true,
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true,
    arg(clap::Arg::new("version").short('v').long("version").action(clap::ArgAction::Version).help("Print version")),
    about = "Compress PNG, JPEG and WebP locally",
    after_help = "Balanced lossy compression; replaces images in place, no backup.\nUse -o to save copies and preserve relative paths for a directory input.\nDirectory inputs scan the top level by default. Use -r to include subdirectories.\nStatic images only; max 25 MiB and 20 million pixels per image.\nRepeated canonical input paths return DUPLICATE_INPUT, including symlink aliases.\nStdout: one compact JSON array after the batch completes.\nSuccess: input/output paths, before/after bytes, ms (compression only),\ndssim/dssim_peak (lower is better), dssim_mode, detail.\nFailure: input, error {code, message}. Batches continue after errors.\nUsage errors: JSON array without input. Help/version: plain text.\nExit: 0 success, 1 failure, 2 usage."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
    // 按顺序逐张处理图片
    #[arg(required = true, help = "One or more image files, or one directory")]
    inputs: Vec<PathBuf>,
    // 默认原地替换 指定目录时另存且不覆盖已有文件
    #[arg(
        short = 'o',
        long,
        help = "Save copies without overwriting existing files"
    )]
    outdir: Option<PathBuf>,
    #[arg(short, long, help = "Include subdirectories of a directory input")]
    recursive: bool,
    // 无损只保证解码像素不变 文件字节和元数据可能变化
    #[arg(
        short = 'l',
        long,
        help = "Preserve decoded pixels; file bytes and metadata may change"
    )]
    lossless: bool,
}

#[derive(Subcommand)]
enum Action {
    #[command(about = "Install the latest stable release in place")]
    Update,
}

fn update(stdout: &mut impl Write) -> Result<u8> {
    let result = (|| -> Result<()> {
        #[cfg(not(windows))]
        let executable = std::env::current_exe()?;
        #[cfg(not(windows))]
        let directory = executable
            .parent()
            .context("Executable has no parent directory")?;
        let temporary = tempfile::tempdir()?;
        #[cfg(not(windows))]
        let (script_name, curl) = ("teapress-installer.sh", "curl");
        #[cfg(windows)]
        let (script_name, curl) = ("teapress-installer.ps1", "curl.exe");
        let script = temporary.path().join(script_name);
        let url =
            format!("https://github.com/teash1rt/teapress/releases/latest/download/{script_name}");
        // 下载完整后再执行 避免网络中断时运行残缺脚本
        let status = Command::new(curl)
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--connect-timeout",
                "10",
                "--max-time",
                "120",
                "--output",
            ])
            .arg(&script)
            .arg(url)
            .stdout(Stdio::from(io::stderr()))
            .status()
            .context("Failed to run curl to download the installer")?;
        if !status.success() {
            bail!("Installer download failed with {status}");
        }
        // Windows 先安装到临时目录 避免安装器覆盖正在运行的 exe
        #[cfg(windows)]
        let directory = temporary.path();
        #[cfg(windows)]
        let mut installer = {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]);
            command
        };
        #[cfg(not(windows))]
        let mut installer = Command::new("/bin/sh");
        // 复用官方安装器 不改 PATH 或安装记录
        let status = installer
            .arg(&script)
            .env_remove("TEAPRESS_INSTALL_DIR")
            .env_remove("CARGO_DIST_FORCE_INSTALL_DIR")
            .env("TEAPRESS_UNMANAGED_INSTALL", directory)
            .stdout(Stdio::from(io::stderr()))
            .status()
            .context("Failed to run the installer")?;
        if !status.success() {
            bail!("Installer exited with {status}");
        }
        #[cfg(windows)]
        {
            let replacement = directory.join("teapress.exe");
            // 替换前确认下载的程序存在且可执行 失败时保留原程序
            let status = Command::new(&replacement)
                .arg("--version")
                .stdout(Stdio::from(io::stderr()))
                .status()
                .context("Failed to validate the downloaded executable")?;
            if !status.success() {
                bail!("Downloaded executable exited with {status}");
            }
            self_replace::self_replace(&replacement)
                .context("Failed to replace the running executable")?;
        }
        Ok(())
    })();
    let (code, row) = match result {
        Ok(()) => (0, json!({"detail":"Update completed"})),
        Err(error) => (
            1,
            json!({"error":{"code":"UPDATE_FAILED","message":error_message(error)}}),
        ),
    };
    emit(stdout, &json!([row]))?;
    Ok(code)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            // 标准输出不可写时 改用标准错误报告
            eprintln!(
                "{}",
                json!([{"error":{"code":"STDOUT_WRITE_FAILED","message":error_message(error)}}])
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

fn error_message(error: impl std::fmt::Display) -> String {
    error
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn input_error(input: &Path, code: &str, error: impl std::fmt::Display) -> serde_json::Value {
    json!({"input":input.to_string_lossy(),"error":{"code":code,"message":error_message(error)}})
}

fn scan_directory(
    source: &Path,
    excluded: Option<&Path>,
    recursive: bool,
) -> Vec<(PathBuf, io::Result<()>)> {
    let mut directories = vec![PathBuf::new()];
    let mut paths = Vec::new();
    while let Some(directory) = directories.pop() {
        let entries = match fs::read_dir(source.join(&directory)) {
            Ok(entries) => entries,
            Err(error) => {
                paths.push((directory, Err(error)));
                continue;
            }
        };
        for entry in entries {
            let (path, file_type) = match entry {
                Ok(entry) => (directory.join(entry.file_name()), entry.file_type()),
                Err(error) => (directory.clone(), Err(error)),
            };
            match file_type {
                Ok(kind) if recursive && kind.is_dir() && Some(path.as_path()) != excluded => {
                    directories.push(path);
                }
                Ok(kind) if kind.is_file() && supported_format(&path).is_some() => {
                    paths.push((path, Ok(())));
                }
                Err(error) => paths.push((path, Err(error))),
                _ => {}
            }
        }
    }
    paths.sort_by(|a, b| a.0.cmp(&b.0));
    paths
}

fn run() -> Result<u8> {
    let mut stdout = io::stdout().lock();
    let Cli {
        command,
        inputs,
        outdir,
        recursive,
        lossless,
    } = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => match error.kind() {
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                write!(stdout, "{error}")?;
                return Ok(0);
            }
            clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                error.exit()
            }
            _ => {
                emit(
                    &mut stdout,
                    &json!([{"error":{"code":"CLI_PARSE_ERROR","message":error_message(error)}}]),
                )?;
                return Ok(2);
            }
        },
    };
    if let Some(Action::Update) = command {
        return update(&mut stdout);
    }
    let (directory, paths) = if let [input] = inputs.as_slice()
        && fs::metadata(input).is_ok_and(|metadata| metadata.is_dir())
    {
        let mut code = "INPUT_READ_FAILED";
        let result = (|| -> Result<_> {
            let source = fs::canonicalize(input)?;
            let output = if let Some(outdir) = &outdir {
                code = "OUTPUT_WRITE_FAILED";
                fs::create_dir_all(outdir)?;
                let output = fs::canonicalize(outdir)?;
                code = "INVALID_INPUT";
                ensure!(source != output, "Input and output directories must differ");
                Some(output)
            } else {
                None
            };
            let excluded = output
                .as_deref()
                .and_then(|dir| dir.strip_prefix(&source).ok());
            Ok(scan_directory(&source, excluded, recursive))
        })();
        match result {
            Ok(paths) => (Some(input.as_path()), paths),
            Err(error) => {
                emit(&mut stdout, &[input_error(input, code, error)])?;
                return Ok(1);
            }
        }
    } else {
        (
            None,
            inputs.iter().cloned().map(|path| (path, Ok(()))).collect(),
        )
    };
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let mut failed = false;
    for (path, scan_result) in paths {
        let input = match directory {
            Some(dir) if path.as_os_str().is_empty() => dir.to_path_buf(),
            Some(dir) => dir.join(&path),
            None => path.clone(),
        };
        let mut code = "INPUT_READ_FAILED";
        let result = (|| -> Result<serde_json::Value> {
            scan_result?;
            let metadata = fs::metadata(&input)?;
            code = "INVALID_INPUT";
            if metadata.is_dir() {
                bail!("Input directories must be passed alone");
            }
            ensure!(metadata.is_file(), "Input must be a regular image file");
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
                Some(dir) if directory.is_some() => dir.join(&path),
                Some(dir) => dir.join(name),
                None => canonical,
            };
            code = "OUTPUT_WRITE_FAILED";
            let parent = destination
                .parent()
                .context("Output path has no parent directory")?;
            if outdir.is_some() {
                fs::create_dir_all(parent)?;
                if destination.exists() {
                    code = "OUTPUT_EXISTS";
                    bail!(
                        "Output already exists; refusing to overwrite: {}",
                        destination.display()
                    );
                }
            }
            code = "COMPRESSION_FAILED";
            let output = compress(&bytes, name, lossless)?;
            // 原样保留就不写文件
            // 需要替换时先在同一文件系统写完临时文件 再原子替换
            if outdir.is_some() || !output.retained {
                code = "OUTPUT_WRITE_FAILED";
                let mut target = tempfile::NamedTempFile::new_in(parent)?;
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
            row["input"] = json!(input.to_string_lossy());
            row["output"] = json!(destination.to_string_lossy());
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
                input_error(&input, code, error)
            }
        };
        rows.push(row);
    }
    emit(&mut stdout, &rows)?;
    Ok(u8::from(failed))
}

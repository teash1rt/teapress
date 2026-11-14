use image::{ImageFormat, Rgb, RgbImage};
use std::{io::Cursor, process::Command};

fn image_bytes(format: ImageFormat) -> Vec<u8> {
    let image = RgbImage::from_fn(32, 24, |x, y| Rgb([x as u8 * 4, y as u8 * 6, 80]));
    let mut bytes = Cursor::new(Vec::new());
    if format == ImageFormat::Png {
        // 不预先压缩 确保测试会走到原地替换
        let mut encoder = png::Encoder::new(&mut bytes, image.width(), image.height());
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::NoCompression);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(image.as_raw())
            .unwrap();
    } else {
        image.write_to(&mut bytes, format).unwrap();
    }
    bytes.into_inner()
}

#[test]
fn batch_continues_after_bad_image_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("中文.JPEG");
    let bad = dir.path().join("坏图.png");
    let out = dir.path().join("out");
    std::fs::write(&good, image_bytes(ImageFormat::Jpeg)).unwrap();
    std::fs::write(&bad, b"broken").unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_teapress"))
        .arg("-o")
        .arg(&out)
        .arg(&bad)
        .arg(&good)
        .output()
        .unwrap();
    assert!(!run.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert!(rows[0]["error"]["message"].is_string());
    assert_eq!(rows[1]["input"], good.to_str().unwrap());
    assert_eq!(
        rows[1]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "after",
            "before",
            "detail",
            "dssim",
            "dssim_mode",
            "dssim_peak",
            "input",
            "ms",
            "output"
        ]
    );
    assert!(rows[1]["detail"].as_str().unwrap().is_ascii());
    assert_eq!(String::from_utf8_lossy(&run.stdout).lines().count(), 1);
    let dst = out.join("中文.JPEG");
    assert!(dst.exists());
    std::fs::write(&dst, b"must not overwrite").unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_teapress"))
        .arg("-o")
        .arg(&out)
        .arg(&good)
        .output()
        .unwrap();
    assert!(!run.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(rows[0]["error"]["code"], "OUTPUT_EXISTS");
    assert_eq!(std::fs::read(dst).unwrap(), b"must not overwrite");
}

#[test]
fn default_replaces_inputs_losslessly_and_leaves_failed_inputs_intact() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = [
        ("按钮 空格🙂.PNG", image_bytes(ImageFormat::Png)),
        ("拍照.JPEG", image_bytes(ImageFormat::Jpeg)),
        ("方向.webp", image_bytes(ImageFormat::WebP)),
    ];
    for (name, bytes) in &inputs {
        std::fs::write(dir.path().join(name), bytes).unwrap();
    }
    std::fs::write(dir.path().join("bad.png"), b"broken").unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_teapress"))
        .current_dir(dir.path())
        .arg("-l")
        .arg("bad.png")
        .args(inputs.iter().map(|(name, _)| name))
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));
    let rows: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 4);
    assert!(rows[0]["error"]["message"].is_string());
    assert_eq!(
        std::fs::read(dir.path().join("bad.png")).unwrap(),
        b"broken"
    );
    assert!(!dir.path().join("output").exists());
    for (i, (name, original)) in inputs.into_iter().enumerate() {
        let path = dir.path().join(name);
        let output = std::fs::read(&path).unwrap();
        assert_eq!(rows[i + 1]["input"], name);
        assert!(rows[i + 1]["detail"].as_str().unwrap().is_ascii());
        assert_eq!(
            rows[i + 1]["output"],
            std::fs::canonicalize(path).unwrap().to_str().unwrap()
        );
        assert_eq!(rows[i + 1]["dssim_mode"], "exact");
        assert!(output.len() <= original.len());
        if name.ends_with(".PNG") {
            assert!(output.len() < original.len(), "must exercise replacement");
        }
        assert_eq!(
            image::load_from_memory(&output).unwrap().to_rgba8(),
            image::load_from_memory(&original).unwrap().to_rgba8()
        );
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 4);
}

#[cfg(unix)]
#[test]
fn in_place_replacement_preserves_symlink_and_file_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let original = image_bytes(ImageFormat::Png);
    let input = dir.path().join("source.png");
    let link = dir.path().join("link.png");
    std::fs::write(&input, &original).unwrap();
    std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o640)).unwrap();
    symlink(&input, &link).unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_teapress"))
        .arg(&link)
        .arg("--lossless")
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(link.is_symlink());
    assert!(std::fs::metadata(&input).unwrap().len() < original.len() as u64);
    assert_eq!(
        std::fs::metadata(&input).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn invalid_output_directory_returns_json_without_changing_input() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("photo.jpeg");
    let out = dir.path().join("not-a-directory");
    let original = image_bytes(ImageFormat::Jpeg);
    std::fs::write(&input, &original).unwrap();
    std::fs::write(&out, b"existing file").unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_teapress"))
        .arg(&input)
        .arg("-o")
        .arg(&out)
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));
    assert!(run.stderr.is_empty());
    let rows: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert!(rows[0]["error"]["message"].is_string());
    assert_eq!(std::fs::read(&input).unwrap(), original);
    assert_eq!(rows[0]["error"]["code"], "OUTPUT_WRITE_FAILED");
    assert_eq!(std::fs::read(&out).unwrap(), b"existing file");
}

#[test]
fn usage_errors_are_json_and_help_stays_text() {
    for args in [
        vec!["--unknown"],
        vec!["--jsonl"],
        vec!["-o"],
        vec!["--lossless"],
        vec!["-o", "output"],
        vec!["update", "photo.png"],
        vec!["update", "--lossless"],
    ] {
        let run = Command::new(env!("CARGO_BIN_EXE_teapress"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(run.status.code(), Some(2));
        assert!(run.stderr.is_empty());
        let rows: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
        assert_eq!(rows[0]["error"]["code"], "CLI_PARSE_ERROR");
        let message = rows[0]["error"]["message"].as_str().unwrap();
        assert!(!message.contains(['\n', '\r', '\t']));
        assert!(!message.contains("  "));
        assert!(rows[0].get("input").is_none());
    }
    for args in [vec![], vec!["--help"], vec!["--version"], vec!["-v"]] {
        let run = Command::new(env!("CARGO_BIN_EXE_teapress"))
            .args(&args)
            .output()
            .unwrap();
        assert!(run.status.success());
        assert!(run.stderr.is_empty());
        let text = String::from_utf8(run.stdout).unwrap();
        assert!(text.contains("teapress"));
        if args.is_empty() || args == ["--help"] {
            assert!(text.contains("25 MiB"));
            assert!(text.contains("Usage:"));
            assert!(text.contains("Commands:"));
            assert!(text.contains("Options:"));
            assert!(!text.contains("CLI_PARSE_ERROR"));
            assert!(!text.contains("--jsonl"));
            assert!(text.contains("-v, --version"));
        } else {
            assert_eq!(text.trim(), concat!("teapress ", env!("CARGO_PKG_VERSION")));
        }
    }
}

#[cfg(unix)]
#[test]
fn update_installs_in_place_and_keeps_stdout_json() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let install = dir.path().join("install with spaces");
    std::fs::create_dir(&install).unwrap();
    let binary = install.join("teapress");
    std::fs::copy(env!("CARGO_BIN_EXE_teapress"), &binary).unwrap();
    let help = Command::new(&binary)
        .args(["update", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("latest stable release"));

    let tools = dir.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    let missing = Command::new(&binary)
        .arg("update")
        .env("PATH", &tools)
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    let rows: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(rows[0]["error"]["code"], "UPDATE_FAILED");

    // 模拟下载 不访问网络也不替换真实安装
    let curl = tools.join("curl");
    std::fs::write(&curl, r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output) shift; output="$1" ;;
        https://*) test "$1" = 'https://github.com/teash1rt/teapress/releases/latest/download/teapress-installer.sh' || exit 99 ;;
    esac
    shift
done
/bin/cat "$MOCK_INSTALLER" > "$output"
echo downloading >&2
exit "$MOCK_DOWNLOAD_EXIT"
"#).unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let installer = dir.path().join("installer.sh");
    std::fs::write(
        &installer,
        r#"test -z "${TEAPRESS_INSTALL_DIR:-}${CARGO_DIST_FORCE_INSTALL_DIR:-}" || exit 99
printf installed > "$TEAPRESS_UNMANAGED_INSTALL/installed"
echo installing
echo progress >&2
exit "$MOCK_INSTALL_EXIT"
"#,
    )
    .unwrap();
    for (download_exit, install_exit) in [(7, 0), (0, 7), (0, 0)] {
        let run = Command::new(&binary)
            .arg("update")
            .env("PATH", &tools)
            .env("MOCK_INSTALLER", &installer)
            .env("MOCK_DOWNLOAD_EXIT", download_exit.to_string())
            .env("MOCK_INSTALL_EXIT", install_exit.to_string())
            .env("TEAPRESS_INSTALL_DIR", dir.path().join("wrong"))
            .env("CARGO_DIST_FORCE_INSTALL_DIR", dir.path().join("wrong"))
            .output()
            .unwrap();
        assert_eq!(
            run.status.success(),
            download_exit == 0 && install_exit == 0
        );
        let rows: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        let progress = String::from_utf8(run.stderr).unwrap();
        assert!(progress.contains("downloading"));
        if download_exit == 0 {
            assert!(progress.contains("installing") && progress.contains("progress"));
            assert_eq!(
                std::fs::read(install.join("installed")).unwrap(),
                b"installed"
            );
        } else {
            assert!(!install.join("installed").exists());
        }
        if run.status.success() {
            assert_eq!(rows[0]["detail"], "Update completed");
        } else {
            assert_eq!(rows[0]["error"]["code"], "UPDATE_FAILED");
        }
    }
}

#[test]
fn json_array_preserves_order_and_continues_after_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.png"), b"broken").unwrap();
    std::fs::write(dir.path().join("-中文.png"), image_bytes(ImageFormat::Png)).unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_teapress"))
        .current_dir(dir.path())
        .args([
            "--lossless",
            "--",
            "missing.png",
            ".",
            "bad.png",
            "-中文.png",
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));
    assert!(run.stderr.is_empty());
    let text = String::from_utf8(run.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(rows.len(), 4);
    for (row, (input, code)) in rows.iter().zip([
        ("missing.png", "INPUT_NOT_FOUND"),
        (".", "INVALID_INPUT"),
        ("bad.png", "COMPRESSION_FAILED"),
    ]) {
        assert_eq!(row["input"], input);
        assert_eq!(row["error"]["code"], code);
    }
    assert!(
        rows[1]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("directories")
    );
    assert_eq!(rows[3]["input"], "-中文.png");
    assert!(rows[3]["after"].as_u64().unwrap() < rows[3]["before"].as_u64().unwrap());
}

#[test]
fn duplicate_paths_are_rejected_without_recompressing() {
    for copies in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("a.png");
        let original = image_bytes(ImageFormat::Png);
        std::fs::write(&input, &original).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_teapress"));
        command.current_dir(dir.path()).arg("--lossless");
        if copies {
            command.args(["-o", "out"]);
        }
        command.arg("a.png").arg("./a.png").arg(&input);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&input, dir.path().join("alias.png")).unwrap();
            command.arg("alias.png");
        }
        let run = command.output().unwrap();
        assert_eq!(run.status.code(), Some(1));
        assert!(run.stderr.is_empty());
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&run.stdout).unwrap();
        assert_eq!(rows.len(), if cfg!(unix) { 4 } else { 3 });
        assert!(rows[0]["after"].as_u64().unwrap() < original.len() as u64);
        for row in &rows[1..] {
            assert_eq!(row["error"]["code"], "DUPLICATE_INPUT");
            assert!(row.get("after").is_none());
        }
        let destination = if copies {
            dir.path().join("out/a.png")
        } else {
            input.clone()
        };
        let output = std::fs::read(destination).unwrap();
        assert_eq!(output.len() as u64, rows[0]["after"].as_u64().unwrap());
        if copies {
            assert_eq!(std::fs::read(&input).unwrap(), original);
            assert_eq!(
                std::fs::read_dir(dir.path().join("out")).unwrap().count(),
                1
            );
        }
    }
}

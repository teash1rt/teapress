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
        vec![],
        vec!["--unknown"],
        vec!["--jsonl"],
        vec!["-o"],
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
        assert!(rows[0]["error"]["message"].is_string());
        assert!(rows[0].get("input").is_none());
    }
    for flag in ["--help", "--version", "-v"] {
        let run = Command::new(env!("CARGO_BIN_EXE_teapress"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(run.status.success());
        assert!(run.stderr.is_empty());
        let text = String::from_utf8(run.stdout).unwrap();
        assert!(text.contains("teapress"));
        if flag == "--help" {
            assert!(text.contains("25 MiB"));
            assert!(!text.contains("--jsonl"));
            assert!(text.contains("-v, --version"));
        } else {
            assert_eq!(text.trim(), concat!("teapress ", env!("CARGO_PKG_VERSION")));
        }
    }
}

#[cfg(unix)]
#[test]
fn update_uses_sibling_updater_and_keeps_stdout_json() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("teapress");
    let updater = dir.path().join("teapress-update");
    std::fs::copy(env!("CARGO_BIN_EXE_teapress"), &binary).unwrap();
    let help = Command::new(&binary)
        .args(["update", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("latest stable release"));

    // PATH 中的同名程序不能代替随包安装的更新器
    let other = dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let decoy = other.join("teapress-update");
    std::fs::write(&decoy, "#!/bin/sh\necho unexpected\n").unwrap();
    std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o755)).unwrap();
    let missing = Command::new(&binary)
        .arg("update")
        .env("PATH", &other)
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    let rows: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(rows[0]["error"]["code"], "UPDATE_UNAVAILABLE");

    for exit in [0, 7] {
        std::fs::write(&updater, format!("#!/bin/sh\ntest \"$#\" -eq 0 || exit 99\necho checking\necho progress >&2\nexit {exit}\n")).unwrap();
        std::fs::set_permissions(&updater, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = Command::new(&binary).arg("update").output().unwrap();
        assert_eq!(run.status.success(), exit == 0);
        let rows: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        let progress = String::from_utf8(run.stderr).unwrap();
        assert!(progress.contains("checking") && progress.contains("progress"));
        if exit == 0 {
            assert_eq!(rows[0]["detail"], "Update check completed");
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

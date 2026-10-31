mod common;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::process::Command;

#[test]
fn content_only_keeps_files_readable_and_symlinks_as_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let mut bytes = common::image();
    common::u16_at(&mut bytes, common::inode_at(3), 0o100000);
    let image = temp.path().join("test.img");
    fs::write(&image, bytes).unwrap();
    let dest = temp.path().join("contents");
    let result = Command::new(env!("CARGO_BIN_EXE_ufsreader"))
        .args(["--content-only", "extract"])
        .arg(&image)
        .arg(&dest)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(dest.join("file")).unwrap(), b"Hello, UFS1!\n");
    assert!(!dest.join("link").exists());
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .contains("symlink_target")
    );
}

#[test]
fn extracts_regular_files_hardlinks_and_symlinks_but_not_devices() {
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("test.img");
    fs::write(&image, common::image()).unwrap();
    let dest = temp.path().join("extracted");
    let result = Command::new(env!("CARGO_BIN_EXE_ufsreader"))
        .arg("extract")
        .arg(&image)
        .arg(&dest)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(dest.join("file")).unwrap(), b"Hello, UFS1!\n");
    assert_eq!(
        fs::metadata(dest.join("file")).unwrap().ino(),
        fs::metadata(dest.join("hard")).unwrap().ino()
    );
    assert_eq!(
        fs::read_link(dest.join("link")).unwrap().to_str(),
        Some("/file")
    );
    assert!(!dest.join("device").exists());
    assert_eq!(fs::metadata(dest.join("file")).unwrap().mtime(), 900000000);
    assert_eq!(fs::metadata(&dest).unwrap().mtime(), 900000000);
    let rows: Vec<serde_json::Value> = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 6);
    assert!(rows.iter().any(|r| r["action"] == "special_metadata_only"));
    let retry = Command::new(env!("CARGO_BIN_EXE_ufsreader"))
        .arg("extract")
        .arg(&image)
        .arg(&dest)
        .output()
        .unwrap();
    assert!(!retry.status.success());
}

#[test]
fn bad_images_fail_cleanly_and_byte_budget_is_enforced() {
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("bad.img");
    fs::write(&image, []).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ufsreader"))
        .arg("list")
        .arg(&image)
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&result.stderr).contains("panicked"));
    fs::write(&image, common::image()).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ufsreader"))
        .args(["--max-bytes", "1", "cat"])
        .arg(&image)
        .arg("/file")
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
}

#[test]
fn preserves_non_utf8_names_and_never_follows_extracted_links() {
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir().unwrap();
    let mut b = common::image();
    common::directory(&mut b, 24, &[(3, b"bad\xff"), (4, b"escape")]);
    common::u64_at(&mut b, common::inode_at(4) + 8, 13);
    b[29 * 1024..29 * 1024 + 13].copy_from_slice(b"../../outside");
    let image = temp.path().join("test.img");
    fs::write(&image, b).unwrap();
    let dest = temp.path().join("extract");
    let result = Command::new(env!("CARGO_BIN_EXE_ufsreader"))
        .arg("extract")
        .arg(&image)
        .arg(&dest)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(dest.join(std::ffi::OsStr::from_bytes(b"bad\xff"))).unwrap(),
        b"Hello, UFS1!\n"
    );
    assert_eq!(
        fs::read_link(dest.join("escape")).unwrap().to_str(),
        Some("../../outside")
    );
    assert!(!temp.path().join("outside").exists());
    let rows: Vec<serde_json::Value> = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(rows.iter().any(|r| r["path_hex"] == "2f626164ff"));
}

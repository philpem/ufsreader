mod common;
use common::*;
use std::io::Cursor;
use ufsreader::{FileSystem, Format};

fn open(b: Vec<u8>) -> std::io::Result<FileSystem<Cursor<Vec<u8>>>> {
    FileSystem::open(Cursor::new(b), 0, None, Format::Riscix)
}

#[test]
fn reads_old_ownership_paths_and_links() {
    let fs = open(image()).unwrap();
    let inode = fs.lookup(b"/link").unwrap();
    assert_eq!((inode.uid, inode.gid, inode.number), (3, 4, 3));
    let mut out = Vec::new();
    fs.copy(&inode, &mut out).unwrap();
    assert_eq!(out, b"Hello, UFS1!\n");
    assert!(fs.lookup(b"/loop").is_err());
    assert!(fs.lookup(b"/absent").is_err());
    let mut count = 0;
    fs.walk(20, |_, _| {
        count += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(count, 6);
    assert!(fs.walk(2, |_, _| Ok(())).is_err());
}

#[test]
fn handles_holes_and_all_indirection_levels() {
    let mut b = image();
    // First direct block is a hole, second contains data.
    inode(&mut b, 7, 0o100644, 4100, 0);
    u32_at(&mut b, inode_at(7) + 44, 32);
    b[32 * 1024..32 * 1024 + 4].copy_from_slice(b"DATA");
    // First single, double and triple indirect pointers each lead to DATA.
    let n = 1024u64;
    inode(&mut b, 8, 0o100644, (12 + n + n * n + 1) * 4096, 0);
    for (offset, fragment) in [(88, 40), (92, 44), (96, 52)] {
        u32_at(&mut b, inode_at(8) + offset, fragment);
    }
    for (fragment, next) in [(40, 32), (44, 48), (48, 32), (52, 56), (56, 60), (60, 32)] {
        u32_at(&mut b, fragment * 1024, next);
    }
    let fs = open(b).unwrap();
    let mut direct = vec![1; 4100];
    fs.read(&fs.inode(7).unwrap(), 0, &mut direct).unwrap();
    assert!(direct[..4096].iter().all(|b| *b == 0));
    assert_eq!(&direct[4096..], b"DATA");
    let inode = fs.inode(8).unwrap();
    for lbn in [12, 12 + n, 12 + n + n * n] {
        let mut data = [0; 4];
        fs.read(&inode, lbn * 4096, &mut data).unwrap();
        assert_eq!(&data, b"DATA");
    }
    let mut hole = [1; 4];
    fs.read(&inode, 13 * 4096, &mut hole).unwrap();
    assert_eq!(hole, [0; 4]);
}

#[test]
fn rejects_truncation_geometry_and_bad_pointers() {
    let original = image();
    for length in [0, 9000, 16384, original.len() - 1] {
        assert!(open(original[..length].to_vec()).is_err());
    }
    for (at, value) in [
        (1372, 0),
        (184, 0),
        (52, 0),
        (48, 0xffffffff),
        (16, 0x7fffffff),
        (44, 0xffffffff),
        (120, 1),
    ] {
        let mut b = original.clone();
        u32_at(&mut b, 8192 + at, value);
        assert!(open(b).is_err(), "field {at}");
    }
    let mut b = original;
    u32_at(&mut b, inode_at(3) + 40, 0xffffffff);
    let fs = open(b).unwrap();
    assert!(fs.copy(&fs.inode(3).unwrap(), &mut Vec::new()).is_err());
    assert!(fs.inode(u32::MAX).is_err());
}

#[test]
fn rejects_bad_directory_records_names_and_cycles() {
    for length in [0, 4, 9, 516] {
        let mut b = image();
        u16_at(&mut b, 24 * 1024 + 4, length);
        assert!(open(b).unwrap().walk(20, |_, _| Ok(())).is_err());
    }
    for name in [b"../escape".as_slice(), b"a/b", b"a\0b"] {
        let mut b = image();
        directory(&mut b, 24, &[(3, name)]);
        assert!(open(b).unwrap().walk(20, |_, _| Ok(())).is_err());
    }
    let mut b = image();
    directory(&mut b, 24, &[(2, b"cycle")]);
    assert!(open(b).unwrap().walk(20, |_, _| Ok(())).is_err());
    let mut b = image();
    directory(&mut b, 24, &[(3, b"same"), (3, b"same")]);
    assert!(open(b).unwrap().walk(20, |_, _| Ok(())).is_err());
}

#[test]
fn slice_bounds_are_enforced() {
    let mut b = vec![0; 512];
    b.extend(image());
    let fs = FileSystem::open(
        Cursor::new(b.clone()),
        512,
        Some(128 * 1024),
        Format::Riscix,
    )
    .unwrap();
    assert_eq!(fs.lookup(b"/file").unwrap().size, 13);
    assert!(FileSystem::open(Cursor::new(b), 512, Some(128 * 1024 - 1), Format::Riscix).is_err());
}

#[test]
fn mutated_inputs_never_panic_or_hide_errors() {
    // Deterministic mutation smoke test covers arithmetic and directory extents.
    for (region, length) in [(8192, 1376), (inode_at(2), 128), (24 * 1024, 512)] {
        for index in (0..length).step_by(7) {
            let mut b = image();
            b[region + index] ^= 0xff;
            if let Ok(fs) = open(b) {
                let _ = fs.walk(20, |_, inode| {
                    if inode.is_file() {
                        fs.copy(inode, &mut std::io::sink())?;
                    }
                    Ok(())
                });
            }
        }
    }
}

#[test]
fn reads_big_endian_old_ufs_without_native_struct_casts() {
    let mut b = image();
    for offset in [16, 36, 44, 48, 52, 56, 116, 120, 184, 188, 1372] {
        b[8192 + offset..8192 + offset + 4].reverse();
    }
    for number in 2..=6 {
        let at = inode_at(number);
        for offset in [0, 2, 4, 6] {
            b[at + offset..at + offset + 2].reverse();
        }
        b[at + 8..at + 16].reverse();
        for offset in (16..128).step_by(4) {
            b[at + offset..at + offset + 4].reverse();
        }
    }
    let mut at = 24 * 1024;
    while at < 24 * 1024 + 512 {
        let length = u16::from_le_bytes([b[at + 4], b[at + 5]]) as usize;
        b[at..at + 4].reverse();
        b[at + 4..at + 6].reverse();
        b[at + 6..at + 8].reverse();
        at += length;
    }
    let fs = open(b).unwrap();
    let inode = fs.lookup(b"/link").unwrap();
    assert_eq!((inode.uid, inode.gid), (3, 4));
    let mut out = Vec::new();
    fs.copy(&inode, &mut out).unwrap();
    assert_eq!(out, b"Hello, UFS1!\n");
}

#[test]
fn rejects_oversized_directories_before_scanning_them() {
    let mut b = image();
    u64_at(&mut b, inode_at(2) + 8, 32 * 1024 * 1024);
    assert!(open(b).is_err());
    assert!(open(image()).unwrap().lookup(b"/file/..").is_err());
}

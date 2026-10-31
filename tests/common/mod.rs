#![allow(dead_code)]
pub fn u16_at(b: &mut [u8], at: usize, value: u16) {
    b[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
pub fn u32_at(b: &mut [u8], at: usize, value: u32) {
    b[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
pub fn u64_at(b: &mut [u8], at: usize, value: u64) {
    b[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
pub fn inode_at(number: usize) -> usize {
    16384 + number * 128
}
pub fn inode(b: &mut [u8], number: usize, mode: u16, size: u64, ptr: u32) {
    let at = inode_at(number);
    u16_at(b, at, mode);
    u16_at(b, at + 2, 1);
    u16_at(b, at + 4, 3);
    u16_at(b, at + 6, 4);
    u64_at(b, at + 8, size);
    for offset in [16, 24, 32] {
        u32_at(b, at + offset, 900000000);
    }
    u32_at(b, at + 40, ptr);
    u32_at(b, at + 104, size.div_ceil(512) as u32);
}
pub fn directory(b: &mut [u8], fragment: usize, entries: &[(u32, &[u8])]) {
    let mut at = fragment * 1024;
    let end = at + 512;
    b[at..end].fill(0);
    for (index, (ino, name)) in entries.iter().enumerate() {
        let length = if index + 1 == entries.len() {
            end - at
        } else {
            (8 + name.len() + 1).next_multiple_of(4)
        };
        u32_at(b, at, *ino);
        u16_at(b, at + 4, length as u16);
        u16_at(b, at + 6, name.len() as u16);
        b[at + 8..at + 8 + name.len()].copy_from_slice(name);
        at += length;
    }
}
pub fn image() -> Vec<u8> {
    let mut b = vec![0; 128 * 1024];
    for (at, value) in [
        (16, 16),
        (36, 128),
        (44, 1),
        (48, 4096),
        (52, 1024),
        (56, 4),
        (116, 1024),
        (120, 32),
        (184, 32),
        (188, 128),
        (1372, 0x11954),
    ] {
        u32_at(&mut b, 8192 + at, value);
    }
    inode(&mut b, 2, 0o40755, 512, 24);
    inode(&mut b, 3, 0o100755, 13, 28);
    u16_at(&mut b, inode_at(3) + 2, 2);
    b[28 * 1024..28 * 1024 + 13].copy_from_slice(b"Hello, UFS1!\n");
    inode(&mut b, 4, 0o120777, 5, 29);
    b[29 * 1024..29 * 1024 + 5].copy_from_slice(b"/file");
    inode(&mut b, 5, 0o120777, 4, 30);
    b[30 * 1024..30 * 1024 + 4].copy_from_slice(b"loop");
    inode(&mut b, 6, 0o20600, 0, 0x1234);
    directory(
        &mut b,
        24,
        &[
            (2, b"."),
            (2, b".."),
            (3, b"file"),
            (3, b"hard"),
            (4, b"link"),
            (5, b"loop"),
            (6, b"device"),
        ],
    );
    b
}

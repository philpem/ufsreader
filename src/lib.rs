// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Checked, read-only UFS1 access. Derived from dancrossnyc/ufsreader's
//! inode, cylinder-group and indirect-block mapping implementation.
#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::io::{self, Read, Seek, SeekFrom, Write};

pub type Result<T> = io::Result<T>;
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or_else(|| invalid("offset overflow"))
}
fn mul(a: u64, b: u64) -> Result<u64> {
    a.checked_mul(b).ok_or_else(|| invalid("offset overflow"))
}

/// Directory and ownership interpretation; these cannot be inferred from magic.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Format {
    /// RISC iX / old BSD: 16-bit directory name length and UID/GID.
    Riscix,
    /// Illumos: old directory entries, 32-bit extended UID/GID.
    Illumos,
    /// 4.4BSD: directory type byte and 8-bit name length, extended UID/GID.
    Bsd44,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Endian {
    Little,
    Big,
}
impl Endian {
    fn u16(self, b: &[u8], at: usize) -> u16 {
        let b = [b[at], b[at + 1]];
        match self {
            Self::Little => u16::from_le_bytes(b),
            Self::Big => u16::from_be_bytes(b),
        }
    }
    fn u32(self, b: &[u8], at: usize) -> u32 {
        let b = [b[at], b[at + 1], b[at + 2], b[at + 3]];
        match self {
            Self::Little => u32::from_le_bytes(b),
            Self::Big => u32::from_be_bytes(b),
        }
    }
    fn u64(self, b: &[u8], at: usize) -> u64 {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&b[at..at + 8]);
        match self {
            Self::Little => u64::from_le_bytes(bytes),
            Self::Big => u64::from_be_bytes(bytes),
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct SuperBlock {
    pub block_size: u32,
    pub fragment_size: u32,
    pub fragments: u32,
    pub cylinder_groups: u32,
    pub inodes_per_group: u32,
    pub fragments_per_group: u32,
    inode_block: u32,
    cg_offset: u32,
    cg_mask: u32,
    endian: Endian,
    max_symlink: u32,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Inode {
    pub number: u32,
    pub mode: u16,
    pub nlink: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub blocks: u32,
    /// Raw first device word for special inodes; encoding is format-dependent.
    pub device_word: u32,
    #[serde(skip)]
    direct: [u32; 12],
    #[serde(skip)]
    indirect: [u32; 3],
    #[serde(skip)]
    inline: Vec<u8>,
}
impl Inode {
    pub fn kind(&self) -> u16 {
        self.mode & 0xf000
    }
    pub fn is_dir(&self) -> bool {
        self.kind() == 0x4000
    }
    pub fn is_file(&self) -> bool {
        self.kind() == 0x8000
    }
    pub fn is_symlink(&self) -> bool {
        self.kind() == 0xa000
    }
}

#[derive(Debug)]
pub struct DirEntry {
    pub inode: u32,
    pub name: Vec<u8>,
}

pub struct FileSystem<R> {
    image: RefCell<R>,
    start: u64,
    length: u64,
    pub superblock: SuperBlock,
    format: Format,
}

impl<R: Read + Seek> FileSystem<R> {
    /// Open a filesystem slice without loading the image. `length=None` uses
    /// the remaining image bytes. All filesystem pointers stay inside the slice.
    pub fn open(mut image: R, start: u64, length: Option<u64>, format: Format) -> Result<Self> {
        let image_len = image.seek(SeekFrom::End(0))?;
        let available = image_len
            .checked_sub(start)
            .ok_or_else(|| invalid("slice starts after image"))?;
        let length = length.unwrap_or(available);
        if length > available || length < 16384 {
            return Err(invalid("truncated filesystem slice"));
        }
        let mut b = [0; 8192];
        image.seek(SeekFrom::Start(add(start, 8192)?))?;
        image.read_exact(&mut b)?;
        let endian = if u32::from_le_bytes(b[1372..1376].try_into().unwrap()) == 0x11954 {
            Endian::Little
        } else if u32::from_be_bytes(b[1372..1376].try_into().unwrap()) == 0x11954 {
            Endian::Big
        } else {
            return Err(invalid("UFS1 magic missing at superblock +1372"));
        };
        let u = |at| endian.u32(&b, at);
        let sb = SuperBlock {
            block_size: u(48),
            fragment_size: u(52),
            fragments: u(36),
            cylinder_groups: u(44),
            inodes_per_group: u(184),
            fragments_per_group: u(188),
            inode_block: u(16),
            cg_offset: u(24),
            cg_mask: u(28),
            endian,
            max_symlink: u(1320),
        };
        let bs = sb.block_size;
        let frag = sb.fragment_size;
        if !(4096..=65536).contains(&bs)
            || !bs.is_power_of_two()
            || !(512..=bs).contains(&frag)
            || !frag.is_power_of_two()
            || bs / frag > 8
            || u(56) != bs / frag
            || u(116) != bs / 4
            || u(120) != bs / 128
            || sb.inodes_per_group == 0
            || !sb.inodes_per_group.is_multiple_of(bs / 128)
            || sb.fragments_per_group == 0
            || sb.cylinder_groups == 0
            || sb.inode_block >= sb.fragments_per_group
            || sb.fragments == 0
        {
            return Err(invalid("invalid UFS geometry"));
        }
        let total = mul(u64::from(sb.fragments), u64::from(frag))?;
        if total > length || total < 16384 {
            return Err(invalid("filesystem extent exceeds slice"));
        }
        let groups = u64::from(sb.fragments).div_ceil(u64::from(sb.fragments_per_group));
        if groups != u64::from(sb.cylinder_groups)
            || mul(
                u64::from(sb.cylinder_groups),
                u64::from(sb.inodes_per_group),
            )? > u64::from(u32::MAX)
        {
            return Err(invalid("invalid cylinder-group count"));
        }
        let fs = Self {
            image: RefCell::new(image),
            start,
            length: total,
            superblock: sb,
            format,
        };
        if !fs.inode(2)?.is_dir() {
            return Err(invalid("root inode is not a directory"));
        }
        Ok(fs)
    }

    fn read_at(&self, at: u64, buf: &mut [u8]) -> Result<()> {
        if add(at, buf.len() as u64)? > self.length {
            return Err(invalid("read outside filesystem extent"));
        }
        let mut image = self.image.borrow_mut();
        image.seek(SeekFrom::Start(add(self.start, at)?))?;
        image.read_exact(buf)
    }

    fn inode_offset(&self, ino: u32) -> Result<u64> {
        let sb = &self.superblock;
        let cg = ino / sb.inodes_per_group;
        if cg >= sb.cylinder_groups {
            return Err(invalid("inode number outside filesystem"));
        }
        let base = mul(u64::from(cg), u64::from(sb.fragments_per_group))?;
        let skew = mul(u64::from(sb.cg_offset), u64::from(cg & !sb.cg_mask))?;
        let table = add(add(base, skew)?, u64::from(sb.inode_block))?;
        let table_end = add(
            mul(table, u64::from(sb.fragment_size))?,
            mul(u64::from(sb.inodes_per_group), 128)?,
        )?;
        let group_end = mul(
            add(base, u64::from(sb.fragments_per_group))?,
            u64::from(sb.fragment_size),
        )?
        .min(self.length);
        if table_end > group_end {
            return Err(invalid("inode table exceeds cylinder group"));
        }
        add(
            mul(table, u64::from(sb.fragment_size))?,
            mul(u64::from(ino % sb.inodes_per_group), 128)?,
        )
    }

    pub fn inode(&self, number: u32) -> Result<Inode> {
        let mut b = [0; 128];
        self.read_at(self.inode_offset(number)?, &mut b)?;
        let e = self.superblock.endian;
        let mut direct = [0; 12];
        let mut indirect = [0; 3];
        for (i, ptr) in direct.iter_mut().enumerate() {
            *ptr = e.u32(&b, 40 + i * 4);
        }
        for (i, ptr) in indirect.iter_mut().enumerate() {
            *ptr = e.u32(&b, 88 + i * 4);
        }
        let legacy = matches!(self.format, Format::Riscix);
        let inode = Inode {
            number,
            mode: e.u16(&b, 0),
            nlink: e.u16(&b, 2),
            uid: if legacy {
                u32::from(e.u16(&b, 4))
            } else {
                e.u32(&b, 116)
            },
            gid: if legacy {
                u32::from(e.u16(&b, 6))
            } else {
                e.u32(&b, 120)
            },
            size: e.u64(&b, 8),
            atime: i64::from(e.u32(&b, 16) as i32),
            mtime: i64::from(e.u32(&b, 24) as i32),
            ctime: i64::from(e.u32(&b, 32) as i32),
            blocks: e.u32(&b, 104),
            device_word: direct[0],
            direct,
            indirect,
            inline: b[40..100].to_vec(),
        };
        if inode.is_dir()
            && (inode.size == 0 || inode.size > 16 * 1024 * 1024 || !inode.size.is_multiple_of(512))
        {
            return Err(invalid("invalid directory size"));
        }
        let n = u64::from(self.superblock.block_size / 4);
        let max_blocks = 12 + n + n * n + n * n * n;
        if inode.size > mul(max_blocks, u64::from(self.superblock.block_size))? {
            return Err(invalid("inode exceeds addressable file size"));
        }
        Ok(inode)
    }

    fn fragment(&self, inode: &Inode, off: u64) -> Result<Option<u64>> {
        let sb = &self.superblock;
        let bs = u64::from(sb.block_size);
        let fs = u64::from(sb.fragment_size);
        let lbn = off / bs;
        let mut ptr;
        if lbn < 12 {
            ptr = inode.direct[lbn as usize];
        } else {
            let mut lbn = lbn - 12;
            let n = bs / 4;
            let mut span = n;
            let mut depth = 0;
            while depth < 3 && lbn >= span {
                lbn -= span;
                span *= n;
                depth += 1;
            }
            if depth == 3 {
                return Err(invalid("indirect address exceeds file limit"));
            }
            ptr = inode.indirect[depth];
            for _ in 0..=depth {
                if ptr == 0 {
                    return Ok(None);
                }
                span /= n;
                let index = (lbn / span) % n;
                let at = add(mul(u64::from(ptr), fs)?, index * 4)?;
                // An indirect block must fit in full, even if only one pointer is read.
                if add(mul(u64::from(ptr), fs)?, bs)? > self.length {
                    return Err(invalid("indirect block outside filesystem"));
                }
                let mut b = [0; 4];
                self.read_at(at, &mut b)?;
                ptr = sb.endian.u32(&b, 0);
            }
        }
        if ptr == 0 {
            return Ok(None);
        }
        let at = add(mul(u64::from(ptr), fs)?, (off % bs) / fs * fs)?;
        if add(at, fs)? > self.length {
            return Err(invalid("data fragment outside filesystem"));
        }
        Ok(Some(at))
    }

    /// Read a bounded caller-provided buffer, including zero-filled holes.
    pub fn read(&self, inode: &Inode, off: u64, buf: &mut [u8]) -> Result<usize> {
        if off >= inode.size {
            return Ok(0);
        }
        let count = (inode.size - off).min(buf.len() as u64) as usize;
        let fs = u64::from(self.superblock.fragment_size);
        let mut done = 0;
        while done < count {
            let offset = add(off, done as u64)?;
            let within = offset % fs;
            let n = (count - done).min((fs - within) as usize);
            match self.fragment(inode, offset)? {
                Some(at) => self.read_at(add(at, within)?, &mut buf[done..done + n])?,
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(done)
    }

    pub fn copy<W: Write>(&self, inode: &Inode, out: &mut W) -> Result<()> {
        if !inode.is_file() {
            return Err(invalid("not a regular file"));
        }
        let mut buf = [0; 65536];
        let mut off = 0;
        while off < inode.size {
            let n = self.read(inode, off, &mut buf)?;
            if n == 0 {
                return Err(invalid("short file read"));
            }
            out.write_all(&buf[..n])?;
            off += n as u64;
        }
        Ok(())
    }

    pub fn read_link(&self, inode: &Inode) -> Result<Vec<u8>> {
        if !inode.is_symlink() || inode.size == 0 || inode.size > 4096 {
            return Err(invalid("invalid symbolic link"));
        }
        let mut b = vec![0; inode.size as usize];
        // RISC iX has no inline symlinks. Later variants declare their threshold.
        if !matches!(self.format, Format::Riscix)
            && self.superblock.max_symlink > inode.size as u32
            && inode.size <= 60
        {
            b.copy_from_slice(&inode.inline[..inode.size as usize]);
        } else {
            self.read(inode, 0, &mut b)?;
        }
        if b.contains(&0) {
            return Err(invalid("NUL in symbolic link target"));
        }
        Ok(b)
    }

    /// Scan directories in 512-byte records; never buffer an entire directory.
    pub fn directory(
        &self,
        inode: &Inode,
        mut visit: impl FnMut(DirEntry) -> Result<()>,
    ) -> Result<()> {
        if !inode.is_dir() {
            return Err(invalid("not a directory"));
        }
        let mut b = [0; 512];
        let e = self.superblock.endian;
        for offset in (0..inode.size).step_by(512) {
            if self.read(inode, offset, &mut b)? != 512 {
                return Err(invalid("short directory read"));
            }
            let mut at = 0;
            while at < b.len() {
                if b.len() - at < 8 {
                    return Err(invalid("truncated directory record"));
                }
                let ino = e.u32(&b, at);
                let reclen = usize::from(e.u16(&b, at + 4));
                if reclen < 8 || reclen % 4 != 0 || reclen > b.len() - at {
                    return Err(invalid("invalid directory record length"));
                }
                let namelen = if matches!(self.format, Format::Bsd44) {
                    usize::from(b[at + 7])
                } else {
                    usize::from(e.u16(&b, at + 6))
                };
                if ino != 0 {
                    if namelen == 0 || namelen > 255 || namelen + 8 >= reclen {
                        return Err(invalid("invalid directory name length"));
                    }
                    let name = b[at + 8..at + 8 + namelen].to_vec();
                    if name.contains(&0) || name.contains(&b'/') || b[at + 8 + namelen] != 0 {
                        return Err(invalid("invalid directory name"));
                    }
                    visit(DirEntry { inode: ino, name })?;
                }
                at += reclen;
            }
        }
        Ok(())
    }

    /// Resolve a path inside the image, following at most 40 symbolic links.
    pub fn lookup(&self, path: &[u8]) -> Result<Inode> {
        if path.len() > 4096 || path.contains(&0) {
            return Err(invalid("invalid path"));
        }
        let mut todo: VecDeque<Vec<u8>> = path
            .split(|b| *b == b'/')
            .filter(|s| !s.is_empty())
            .map(Vec::from)
            .collect();
        let mut stack = vec![2];
        let mut links = 0;
        while let Some(part) = todo.pop_front() {
            if !self.inode(*stack.last().unwrap())?.is_dir() {
                return Err(invalid("path component is not a directory"));
            }
            if part == b"." {
                continue;
            }
            if part == b".." {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            let parent = self.inode(*stack.last().unwrap())?;
            let mut found = None;
            self.directory(&parent, |entry| {
                if entry.name == part {
                    if found.is_some() {
                        return Err(invalid("duplicate directory name"));
                    }
                    found = Some(entry.inode);
                }
                Ok(())
            })?;
            let child = self.inode(found.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "path not found in image")
            })?)?;
            if child.is_symlink() {
                links += 1;
                if links > 40 {
                    return Err(invalid("symbolic link loop"));
                }
                let target = self.read_link(&child)?;
                if target.starts_with(b"/") {
                    stack.truncate(1);
                }
                for piece in target.split(|b| *b == b'/').filter(|s| !s.is_empty()).rev() {
                    todo.push_front(piece.to_vec());
                }
                if todo.iter().map(|s| s.len() + 1).sum::<usize>() > 4096 {
                    return Err(invalid("expanded path too long"));
                }
            } else {
                stack.push(child.number);
            }
        }
        self.inode(*stack.last().unwrap())
    }

    /// Depth-first traversal with bounded path/depth and a cycle/alias guard.
    pub fn walk(
        &self,
        max_entries: usize,
        mut visit: impl FnMut(&[u8], &Inode) -> Result<()>,
    ) -> Result<()> {
        struct WalkState {
            seen: HashSet<u32>,
            left: usize,
            directory_bytes: u64,
        }
        fn descend<R: Read + Seek>(
            fs: &FileSystem<R>,
            path: &mut Vec<u8>,
            inode: Inode,
            depth: usize,
            state: &mut WalkState,
            visit: &mut impl FnMut(&[u8], &Inode) -> Result<()>,
        ) -> Result<()> {
            if state.left == 0 {
                return Err(invalid("entry limit exceeded"));
            }
            state.left -= 1;
            visit(path, &inode)?;
            if !inode.is_dir() {
                return Ok(());
            }
            state.directory_bytes = state
                .directory_bytes
                .checked_sub(inode.size)
                .ok_or_else(|| invalid("directory traversal byte limit exceeded"))?;
            if depth > 256 || !state.seen.insert(inode.number) {
                return Err(invalid("directory cycle, alias or depth limit"));
            }
            let mut names = HashSet::new();
            fs.directory(&inode, |entry| {
                if entry.name == b"." || entry.name == b".." {
                    return Ok(());
                }
                if !names.insert(entry.name.clone()) {
                    return Err(invalid("duplicate directory name"));
                }
                let saved = path.len();
                if saved > 1 {
                    path.push(b'/');
                }
                path.extend_from_slice(&entry.name);
                if path.len() > 4096 {
                    return Err(invalid("path length limit exceeded"));
                }
                descend(fs, path, fs.inode(entry.inode)?, depth + 1, state, visit)?;
                path.truncate(saved);
                Ok(())
            })
        }
        let mut state = WalkState {
            seen: HashSet::new(),
            left: max_entries,
            directory_bytes: 256 * 1024 * 1024,
        };
        descend(
            self,
            &mut b"/".to_vec(),
            self.inode(2)?,
            0,
            &mut state,
            &mut visit,
        )
    }
}

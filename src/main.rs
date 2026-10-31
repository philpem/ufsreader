// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use cap_std::fs::{Dir, OpenOptions};
use clap::{Parser, Subcommand};
use filetime::FileTime;
use serde_json::json;
use sha2::{Digest, Sha256};
use ufsreader::{FileSystem, Format, Inode, Result};

#[derive(Parser)]
#[command(
    version,
    about = "Read-only UFS1 filesystem inspection and extraction (Unix hosts)"
)]
struct Args {
    #[arg(long, default_value = "riscix", global = true)]
    format: Format,
    /// Byte offset of the filesystem inside the image.
    #[arg(long, default_value_t = 0, global = true)]
    offset: u64,
    /// Byte length of the filesystem slice.
    #[arg(long, global = true)]
    length: Option<u64>,
    #[arg(long, default_value_t = 100_000, global = true)]
    max_entries: usize,
    /// Maximum total regular-file bytes processed, counting hard-link paths.
    #[arg(long, default_value_t = 68_719_476_736, global = true)]
    max_bytes: u64,
    /// Extract only regular files/directories, keeping owner access and recording
    /// symlinks and original permissions in metadata (for catalogue ingestion).
    #[arg(long, global = true)]
    content_only: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Validate and print filesystem geometry as JSON.
    Info { image: PathBuf },
    /// Recursive JSON Lines listing, optionally hashing file contents.
    List {
        image: PathBuf,
        #[arg(long)]
        hashes: bool,
    },
    /// Stream a regular file to stdout; symbolic links resolve within the image.
    Cat { image: PathBuf, path: String },
    /// Extract into a NEW directory, emitting JSON Lines metadata to stdout.
    Extract {
        image: PathBuf,
        destination: PathBuf,
    },
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
struct HashWriter<W> {
    out: W,
    hash: Sha256,
}
impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> Result<usize> {
        let n = self.out.write(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> Result<()> {
        self.out.flush()
    }
}
fn record(
    out: &mut impl Write,
    path: &[u8],
    inode: &Inode,
    hash: Option<String>,
    target: Option<&[u8]>,
    action: &str,
) -> Result<()> {
    let value = json!({
        "schema_version": 1,
        "path": String::from_utf8_lossy(path), "path_hex": hex(path),
        "inode": inode, "sha256": hash,
        "symlink_target": target.map(String::from_utf8_lossy),
        "symlink_target_hex": target.map(hex), "action": action,
    });
    serde_json::to_writer(&mut *out, &value)?;
    out.write_all(b"\n")
}
fn budget(remaining: &mut u64, inode: &Inode) -> Result<()> {
    if inode.is_file() {
        *remaining = remaining
            .checked_sub(inode.size)
            .ok_or_else(|| io::Error::other("regular-file byte limit exceeded"))?;
    }
    Ok(())
}
fn set_metadata(file: &File, inode: &Inode) -> Result<()> {
    filetime::set_file_handle_times(
        file,
        Some(FileTime::from_unix_time(inode.atime, 0)),
        Some(FileTime::from_unix_time(inode.mtime, 0)),
    )?;
    // Keep ordinary rwx bits. Ownership, set-ID and special bits are recorded
    // rather than applied to host files.
    file.set_permissions(fs::Permissions::from_mode(u32::from(inode.mode & 0o777)))
}

fn run(args: Args) -> Result<()> {
    let image = match &args.command {
        Command::Info { image }
        | Command::List { image, .. }
        | Command::Cat { image, .. }
        | Command::Extract { image, .. } => image,
    };
    let fs = FileSystem::open(File::open(image)?, args.offset, args.length, args.format)?;
    let mut out = BufWriter::new(io::stdout().lock());
    let mut remaining = args.max_bytes;
    match args.command {
        Command::Info { .. } => {
            serde_json::to_writer(&mut out, &fs.superblock)?;
            writeln!(out)?;
        }
        Command::Cat { path, .. } => {
            let inode = fs.lookup(path.as_bytes())?;
            budget(&mut remaining, &inode)?;
            fs.copy(&inode, &mut out)?;
        }
        Command::List { hashes, .. } => {
            fs.walk(args.max_entries, |path, inode| {
                let hash = if hashes && inode.is_file() {
                    budget(&mut remaining, inode)?;
                    let mut writer = HashWriter {
                        out: io::sink(),
                        hash: Sha256::new(),
                    };
                    fs.copy(inode, &mut writer)?;
                    Some(hex(&writer.hash.finalize()))
                } else {
                    None
                };
                let target = if inode.is_symlink() {
                    Some(fs.read_link(inode)?)
                } else {
                    None
                };
                record(&mut out, path, inode, hash, target.as_deref(), "listed")
            })?;
        }
        Command::Extract { destination, .. } => {
            // A fresh directory plus capability-relative operations prevents
            // existing symlinks from redirecting writes outside the destination.
            fs::create_dir(&destination)?;
            let root = Dir::open_ambient_dir(&destination, cap_std::ambient_authority())?;
            let mut links = Vec::new();
            let mut directories = Vec::new();
            let mut files: HashMap<u32, (PathBuf, String)> = HashMap::new();
            fs.walk(args.max_entries, |path, inode| {
                let relative = if path == b"/" {
                    Path::new(".")
                } else {
                    Path::new(std::ffi::OsStr::from_bytes(&path[1..]))
                };
                let mut hash = None;
                let mut target = None;
                let action;
                if inode.is_dir() {
                    if path != b"/" {
                        root.create_dir(relative)?;
                    }
                    directories.push((relative.to_path_buf(), inode.clone()));
                    action = "directory";
                } else if inode.is_file() {
                    budget(&mut remaining, inode)?;
                    if let Some((first, digest)) = files.get(&inode.number) {
                        root.hard_link(first, &root, relative)?;
                        hash = Some(digest.clone());
                        action = "hard_link";
                    } else {
                        let mut options = OpenOptions::new();
                        options.write(true).create_new(true);
                        let file = root.open_with(relative, &options)?.into_std();
                        let mut writer = HashWriter {
                            out: BufWriter::new(file),
                            hash: Sha256::new(),
                        };
                        fs.copy(inode, &mut writer)?;
                        writer.flush()?;
                        if !args.content_only {
                            set_metadata(writer.out.get_ref(), inode)?;
                        }
                        let digest = hex(&writer.hash.finalize());
                        if inode.nlink > 1 {
                            files.insert(inode.number, (relative.to_path_buf(), digest.clone()));
                        }
                        hash = Some(digest);
                        action = "file";
                    }
                } else if inode.is_symlink() {
                    let value = fs.read_link(inode)?;
                    links.push((relative.to_path_buf(), value.clone()));
                    target = Some(value);
                    action = "symlink";
                } else {
                    // Device nodes, FIFOs and sockets stay as metadata only.
                    action = "special_metadata_only";
                }
                record(&mut out, path, inode, hash, target.as_deref(), action)
            })?;
            // Symlinks are created last, and never followed by the extractor.
            if !args.content_only {
                for (path, target) in links {
                    root.symlink_contents(std::ffi::OsStr::from_bytes(&target), path)?;
                }
            }
            // All recorded paths here are actual directories, not symlink
            // entries. Finish children first so restrictive parent permissions
            // cannot prevent access, and keep only one directory handle open.
            if !args.content_only {
                for (path, inode) in directories.into_iter().rev() {
                    set_metadata(&root.open(path)?.into_std(), &inode)?;
                }
            }
        }
    }
    out.flush()
}
fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("ufsreader: {error}");
        std::process::exit(1);
    }
}

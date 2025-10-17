//! Streaming FUSE filesystem for netcat transfers
//!
//! This module provides a minimal FUSE filesystem that presents a network stream
//! (TcpStream) as a regular file that qemu-img can read from.
//!
//! The problem this solves:
//! - qemu-img requires regular files (doesn't work with pipes/FIFOs/stdin for conversion)
//! - We want to stream data over network without writing to a temporary file first
//! - FUSE lets us present a network stream as a "regular file" to qemu-img
//!
//! Architecture:
//! 1. Mount FUSE filesystem at /tmp/netcat-stream-XXXXX/
//! 2. Present single file "disk.raw" with known size
//! 3. As qemu-img reads the file, FUSE pulls data from TcpStream on-demand
//! 4. Sequential reads only (no seeking) - optimized for streaming
//! 5. Unmount when transfer completes

use std::io::{self, Read};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use anyhow::{Context, Error, Result};

use proxmox_fuse::requests::{self, FuseRequest};
use proxmox_fuse::{Request, ROOT_ID};

const TIMEOUT: f64 = 600.0;
const FILE_INODE: u64 = 2;

/// Streaming filesystem that presents a TcpStream as a regular file
pub struct StreamingFs {
    /// The network stream to read from
    stream: Arc<Mutex<TcpStream>>,

    /// Size of the file (must be known in advance from ESXi metadata)
    file_size: u64,

    /// Current read position in the stream
    position: Arc<Mutex<u64>>,

    /// Buffer for the stream data (reserved for future use)
    #[allow(dead_code)]
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl StreamingFs {
    /// Create a new streaming filesystem
    pub fn new(stream: TcpStream, file_size: u64) -> Result<Arc<Self>> {
        // Force blocking mode - prevents spurious Ok(0) returns from read()
        // In blocking mode, read() will wait for data instead of returning immediately
        // when the TCP buffer is temporarily empty
        stream.set_nonblocking(false)
            .context("Failed to set TcpStream to blocking mode")?;

        Ok(Arc::new(Self {
            stream: Arc::new(Mutex::new(stream)),
            file_size,
            position: Arc::new(Mutex::new(0)),
            buffer: Arc::new(Mutex::new(Vec::new())),
        }))
    }

    /// Handle a FUSE request
    pub async fn handle_request(self: Arc<Self>, request: Request) {
        log::debug!("FUSE REQUEST: {request:?}");

        let res = match request {
            Request::Getattr(r) => self.handle_getattr(r),
            Request::Lookup(r) => self.handle_lookup(r),
            Request::Open(r) => self.handle_open(r),
            Request::Read(r) => self.handle_read(r),
            Request::Release(r) => self.handle_release(r),
            Request::ReaddirPlus(r) => self.handle_readdir(r),
            Request::Forget(r) => {
                r.reply();
                Ok(())
            }
            _ => {
                log::debug!("unhandled request: {request:?}");
                return;
            }
        };

        match res {
            Ok(()) => (),
            Err(err) => log::error!("error handling request: {err:?}"),
        }
    }

    fn handle_getattr(&self, getattr: requests::Getattr) -> Result<(), Error> {
        let stat = if getattr.inode == ROOT_ID {
            self.root_stat()
        } else if getattr.inode == FILE_INODE {
            self.file_stat()
        } else {
            return Ok(getattr.fail(libc::ENOENT)?);
        };

        Ok(getattr.reply(&stat, TIMEOUT)?)
    }

    fn handle_lookup(&self, lookup: requests::Lookup) -> Result<(), Error> {
        if lookup.parent != ROOT_ID {
            return Ok(lookup.fail(libc::ENOENT)?);
        }

        let file_name = lookup.file_name.to_str().unwrap_or("");

        if file_name != "disk.raw" {
            return Ok(lookup.fail(libc::ENOENT)?);
        }

        let entry = proxmox_fuse::EntryParam {
            inode: FILE_INODE,
            generation: 1,
            attr: self.file_stat(),
            attr_timeout: TIMEOUT,
            entry_timeout: TIMEOUT,
        };

        Ok(lookup.reply(&entry)?)
    }

    fn handle_open(&self, open: requests::Open) -> Result<(), Error> {
        if open.inode != FILE_INODE {
            return Ok(open.fail(libc::ENOENT)?);
        }

        Ok(open.reply(0)?)
    }

    fn handle_read(&self, read: requests::Read) -> Result<(), Error> {
        if read.inode == ROOT_ID {
            return Ok(read.fail(libc::EISDIR)?);
        }

        if read.inode != FILE_INODE {
            return Ok(read.fail(libc::ENOENT)?);
        }

        let offset = read.offset;
        let size = read.size as usize;

        log::debug!("Read request: offset={}, size={}", offset, size);

        // Get current position
        let current_pos = *self.position.lock().unwrap();

        // We only support sequential reads (no seeking backwards)
        // This is a streaming filesystem!
        if offset < current_pos {
            log::error!("Attempted to seek backwards from {} to {} - not supported in streaming mode", current_pos, offset);
            return Ok(read.fail(libc::EINVAL)?);
        }

        // If seeking forward, we need to skip data
        if offset > current_pos {
            let skip_bytes = (offset - current_pos) as usize;
            log::debug!("Skipping {} bytes to reach offset {}", skip_bytes, offset);

            let mut stream = self.stream.lock().unwrap();
            let mut discard = vec![0u8; skip_bytes.min(1024 * 1024)]; // 1MB at a time
            let mut remaining = skip_bytes;

            while remaining > 0 {
                let to_read = remaining.min(discard.len());
                match stream.read(&mut discard[..to_read]) {
                    Ok(0) => break, // EOF
                    Ok(n) => remaining -= n,
                    Err(e) => {
                        log::error!("Error skipping bytes: {}", e);
                        return Ok(read.fail(libc::EIO)?);
                    }
                }
            }

            *self.position.lock().unwrap() = offset;
        }

        // Read the requested data - KEEP READING UNTIL BUFFER FILLED OR TRUE EOF
        // This loop is critical to prevent premature EOF when TCP buffers are temporarily empty
        let mut buffer = vec![0u8; size];
        let mut stream = self.stream.lock().unwrap();
        let mut total_read = 0;

        // Keep reading until we fill the buffer or reach true EOF
        while total_read < size {
            match stream.read(&mut buffer[total_read..]) {
                Ok(0) => {
                    // Got zero bytes - this means EOF on the stream
                    // Only accept this as valid EOF if we're at or past the expected file size
                    if offset + total_read as u64 >= self.file_size {
                        log::debug!(
                            "Reached end of file at offset {} (file size: {})",
                            offset + total_read as u64,
                            self.file_size
                        );
                        break;
                    } else {
                        // Unexpected EOF - we haven't read all the data we should have
                        log::error!(
                            "Unexpected EOF at offset {} (expected {} more bytes until file size {})",
                            offset + total_read as u64,
                            self.file_size - (offset + total_read as u64),
                            self.file_size
                        );
                        return Ok(read.fail(libc::EIO)?);
                    }
                }
                Ok(n) => {
                    total_read += n;
                    log::debug!(
                        "Read {} bytes from stream (total this request: {}/{})",
                        n,
                        total_read,
                        size
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Should not happen in blocking mode, but handle it anyway
                    log::warn!("Got WouldBlock in blocking mode, retrying...");
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    // Signal interrupted the read, retry
                    log::debug!("Read interrupted by signal, retrying...");
                    continue;
                }
                Err(e) => {
                    log::error!("Error reading from stream: {}", e);
                    return Ok(read.fail(libc::EIO)?);
                }
            }
        }

        log::debug!(
            "Completed read request: offset={}, requested={}, actual={}",
            offset,
            size,
            total_read
        );

        // Update position
        *self.position.lock().unwrap() = offset + total_read as u64;

        // Reply with data
        Ok(read.reply(&buffer[..total_read])?)
    }

    fn handle_release(&self, release: requests::Release) -> Result<(), Error> {
        Ok(release.reply()?)
    }

    fn handle_readdir(&self, mut readdir: requests::ReaddirPlus) -> Result<(), Error> {
        if readdir.inode != ROOT_ID {
            return Ok(readdir.fail(libc::ENOTDIR)?);
        }

        if readdir.offset == 0 {
            let _ = readdir.add_entry(
                "disk.raw".as_ref(),
                &self.file_stat(),
                1,
                1,
                TIMEOUT,
                TIMEOUT,
            )?;
        }

        Ok(readdir.reply()?)
    }

    fn root_stat(&self) -> libc::stat {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        stat.st_ino = ROOT_ID;
        stat.st_nlink = 2;
        stat.st_mode = 0o555 | libc::S_IFDIR;
        stat
    }

    fn file_stat(&self) -> libc::stat {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        stat.st_ino = FILE_INODE;
        stat.st_nlink = 1;
        stat.st_mode = 0o444 | libc::S_IFREG;
        stat.st_size = self.file_size as i64;
        stat
    }
}

/// Mount a streaming FUSE filesystem and return mount path, filesystem handle, and FUSE session
pub async fn mount_streaming_fs(
    stream: TcpStream,
    file_size: u64
) -> Result<(String, Arc<StreamingFs>, proxmox_fuse::Fuse)> {
    use std::path::Path;

    // Create temporary mount directory
    let temp_dir = std::env::temp_dir().join(format!("netcat-stream-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)
        .context("Failed to create temporary mount directory")?;

    let mount_path = temp_dir.to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid mount path"))?
        .to_string();

    log::info!("Mounting streaming FUSE at {}", mount_path);

    // Create the streaming filesystem
    let fs = StreamingFs::new(stream, file_size)?;

    // Build and mount FUSE filesystem
    let fuse_session = proxmox_fuse::Fuse::builder("netcat-stream-fs")
        .context("Failed to create FUSE builder")?
        .enable_open()
        .enable_read()
        .enable_readdirplus()
        .build()
        .context("Failed to build FUSE session")?
        .mount(Path::new(&mount_path))
        .context("Failed to mount FUSE filesystem")?;

    log::info!("Streaming FUSE mounted successfully at {}", mount_path);

    Ok((mount_path, fs, fuse_session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_stat() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let stream = std::net::TcpStream::connect(addr).unwrap();
        let fs = StreamingFs::new(stream, 1024 * 1024).unwrap();

        let stat = fs.file_stat();
        assert_eq!(stat.st_size, 1024 * 1024);
        assert_eq!(stat.st_ino, FILE_INODE);
    }
}

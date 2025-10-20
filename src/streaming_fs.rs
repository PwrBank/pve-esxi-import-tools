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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::collections::VecDeque;
use anyhow::{Context, Error, Result};

use proxmox_fuse::requests::{self, FuseRequest};
use proxmox_fuse::{Request, ROOT_ID};

const TIMEOUT: f64 = 600.0;
const FILE_INODE: u64 = 2;

// Buffer size for seekable streaming - allows qcow2 to seek backwards within this window
// Research showed qcow2 only seeks backward by exactly 128 KB (one cluster)
// We use 2 MB to provide a 16x safety margin while being memory efficient
const BUFFER_WINDOW_SIZE: usize = 2 * 1024 * 1024; // 2 MB (128x the observed seek distance)

/// Circular buffer that maintains a sliding window of stream data in RAM
/// Allows limited backward seeks within the buffered range
struct CircularBuffer {
    /// The actual data buffer - stores raw bytes from the stream
    data: VecDeque<u8>,

    /// Offset in the file where the first byte of the buffer starts
    start_offset: u64,

    /// Offset in the file where the last byte of the buffer ends (exclusive)
    end_offset: u64,

    /// Maximum capacity of the buffer
    capacity: usize,
}

impl CircularBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity),
            start_offset: 0,
            end_offset: 0,
            capacity,
        }
    }

    /// Check if we can seek to a given offset (is it within our buffered range?)
    fn can_seek_to(&self, offset: u64, size: usize) -> bool {
        offset >= self.start_offset && offset + size as u64 <= self.end_offset
    }

    /// Read data at a specific offset within the buffer
    /// Returns None if the offset is not within the buffered range
    fn read_at(&self, offset: u64, size: usize) -> Option<Vec<u8>> {
        if !self.can_seek_to(offset, size) {
            return None;
        }

        let buffer_offset = (offset - self.start_offset) as usize;
        let mut result = vec![0u8; size];

        for (i, byte_offset) in (buffer_offset..buffer_offset + size).enumerate() {
            result[i] = *self.data.get(byte_offset)?;
        }

        Some(result)
    }

    /// Append new data from the stream to the buffer
    /// Evicts old data if buffer exceeds capacity
    fn append(&mut self, data: &[u8]) {
        // Add new data to the end
        for &byte in data {
            self.data.push_back(byte);
        }

        self.end_offset += data.len() as u64;

        // Evict old data if we exceed capacity
        while self.data.len() > self.capacity {
            self.data.pop_front();
            self.start_offset += 1;
        }
    }

    /// Get the current buffer statistics for logging
    fn stats(&self) -> (u64, u64, usize) {
        (self.start_offset, self.end_offset, self.data.len())
    }
}

/// Streaming filesystem that presents a TcpStream as a regular file
/// Now with seekable circular buffer support for qcow2 conversion
pub struct StreamingFs {
    /// The network stream to read from
    stream: Arc<Mutex<TcpStream>>,

    /// Size of the file (must be known in advance from ESXi metadata)
    file_size: u64,

    /// Circular buffer that maintains a sliding window of recent data
    /// Allows limited backward seeks for qcow2 format conversion
    buffer: Arc<Mutex<CircularBuffer>>,

    /// Current position in the stream (how many bytes we've read from network)
    /// This is atomically updated as we read more data
    stream_position: Arc<AtomicU64>,

    /// Track the last read offset for seek distance calculation
    last_read_offset: Arc<Mutex<u64>>,

    /// Statistics: count of backward seeks
    backward_seek_count: Arc<AtomicUsize>,

    /// Statistics: maximum backward seek distance observed
    max_backward_seek_distance: Arc<AtomicU64>,

    /// Statistics: count of buffer hits (served from buffer)
    buffer_hit_count: Arc<AtomicUsize>,
}

impl StreamingFs {
    /// Create a new streaming filesystem with seekable buffer
    pub fn new(stream: TcpStream, file_size: u64) -> Result<Arc<Self>> {
        // Force blocking mode - prevents spurious Ok(0) returns from read()
        // In blocking mode, read() will wait for data instead of returning immediately
        // when the TCP buffer is temporarily empty
        stream.set_nonblocking(false)
            .context("Failed to set TcpStream to blocking mode")?;

        log::info!(
            "Creating streaming filesystem with {} MB seekable buffer for {} byte file",
            BUFFER_WINDOW_SIZE / (1024 * 1024),
            file_size
        );

        Ok(Arc::new(Self {
            stream: Arc::new(Mutex::new(stream)),
            file_size,
            buffer: Arc::new(Mutex::new(CircularBuffer::new(BUFFER_WINDOW_SIZE))),
            stream_position: Arc::new(AtomicU64::new(0)),
            last_read_offset: Arc::new(Mutex::new(0)),
            backward_seek_count: Arc::new(AtomicUsize::new(0)),
            max_backward_seek_distance: Arc::new(AtomicU64::new(0)),
            buffer_hit_count: Arc::new(AtomicUsize::new(0)),
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

        // Track seek behavior for statistics
        let last_offset = {
            let mut last = self.last_read_offset.lock().unwrap();
            let prev = *last;
            *last = offset;
            prev
        };

        // Check if this is a backward seek
        if offset < last_offset {
            let seek_distance = last_offset - offset;
            self.backward_seek_count.fetch_add(1, Ordering::Relaxed);

            // Update max backward seek distance
            let mut max_dist = self.max_backward_seek_distance.load(Ordering::Relaxed);
            while seek_distance > max_dist {
                match self.max_backward_seek_distance.compare_exchange(
                    max_dist,
                    seek_distance,
                    Ordering::Relaxed,
                    Ordering::Relaxed
                ) {
                    Ok(_) => break,
                    Err(x) => max_dist = x,
                }
            }

            eprintln!(
                "[SEEK] Backward seek: from {} to {} (distance: {} bytes = {} MB)",
                last_offset,
                offset,
                seek_distance,
                seek_distance / (1024 * 1024)
            );
        }

        log::debug!("Read request: offset={}, size={}", offset, size);

        // Get current stream position (how far we've read from the network)
        let stream_pos = self.stream_position.load(Ordering::Relaxed);

        // Try to serve from buffer first (handles backward seeks within buffer window)
        {
            let buffer = self.buffer.lock().unwrap();
            if let Some(data) = buffer.read_at(offset, size) {
                self.buffer_hit_count.fetch_add(1, Ordering::Relaxed);
                let (start, end, len) = buffer.stats();
                log::debug!(
                    "✓ Served from buffer: offset={}, size={} (buffer: {}-{}, {} bytes)",
                    offset,
                    size,
                    start,
                    end,
                    len
                );
                return Ok(read.reply(&data)?);
            }
        }

        // Check if this is a backward seek beyond our buffer
        if offset < stream_pos {
            let buffer = self.buffer.lock().unwrap();
            let (start, end, len) = buffer.stats();
            log::error!(
                "Backward seek beyond buffer window: offset={}, stream_pos={}, buffer: {}-{} ({} bytes)",
                offset,
                stream_pos,
                start,
                end,
                len
            );
            return Ok(read.fail(libc::EINVAL)?);
        }

        // Need to read forward from stream until we have the requested data
        log::debug!(
            "Need to read from stream: offset={}, size={}, stream_pos={}",
            offset,
            size,
            stream_pos
        );

        // Calculate how much to read: need to get to offset + size
        let target_pos = offset + size as u64;

        // Read in 16MB chunks (matches block size recommendation from guide)
        const CHUNK_SIZE: usize = 16 * 1024 * 1024;

        while self.stream_position.load(Ordering::Relaxed) < target_pos {
            let mut chunk = vec![0u8; CHUNK_SIZE];
            let mut stream = self.stream.lock().unwrap();
            let mut total_read = 0;

            // Keep reading until we fill the chunk or reach EOF
            while total_read < CHUNK_SIZE {
                match stream.read(&mut chunk[total_read..]) {
                    Ok(0) => {
                        // EOF - check if it's expected
                        let current_stream_pos = self.stream_position.load(Ordering::Relaxed);
                        if current_stream_pos + total_read as u64 >= self.file_size {
                            log::debug!(
                                "Reached end of file at stream position {} (file size: {})",
                                current_stream_pos + total_read as u64,
                                self.file_size
                            );
                            break;
                        } else {
                            log::error!(
                                "Unexpected EOF at stream position {} (expected {} more bytes until file size {})",
                                current_stream_pos + total_read as u64,
                                self.file_size - (current_stream_pos + total_read as u64),
                                self.file_size
                            );
                            return Ok(read.fail(libc::EIO)?);
                        }
                    }
                    Ok(n) => {
                        total_read += n;
                        log::trace!(
                            "Read {} bytes from stream (chunk progress: {}/{})",
                            n,
                            total_read,
                            CHUNK_SIZE
                        );
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        log::warn!("Got WouldBlock in blocking mode, retrying...");
                        continue;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        log::debug!("Read interrupted by signal, retrying...");
                        continue;
                    }
                    Err(e) => {
                        log::error!("Error reading from stream: {}", e);
                        return Ok(read.fail(libc::EIO)?);
                    }
                }
            }

            // Append the chunk to our circular buffer
            if total_read > 0 {
                let mut buffer = self.buffer.lock().unwrap();
                buffer.append(&chunk[..total_read]);
                self.stream_position.fetch_add(total_read as u64, Ordering::Relaxed);

                let (start, end, len) = buffer.stats();
                log::debug!(
                    "Appended {} bytes to buffer (now: {}-{}, {} bytes, stream_pos={})",
                    total_read,
                    start,
                    end,
                    len,
                    self.stream_position.load(Ordering::Relaxed)
                );
            }

            // Break if we got less than a full chunk (likely EOF)
            if total_read < CHUNK_SIZE {
                break;
            }
        }

        // Now try to serve the request from the buffer again
        let buffer = self.buffer.lock().unwrap();
        if let Some(data) = buffer.read_at(offset, size) {
            log::debug!(
                "✓ Completed read after filling buffer: offset={}, size={}",
                offset,
                size
            );
            return Ok(read.reply(&data)?);
        }

        // If we still can't serve it, something went wrong
        let (start, end, len) = buffer.stats();
        log::error!(
            "Failed to serve read even after filling buffer: offset={}, size={}, buffer: {}-{} ({} bytes)",
            offset,
            size,
            start,
            end,
            len
        );
        Ok(read.fail(libc::EIO)?)
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

    /// Print seek statistics
    pub fn print_seek_statistics(&self) {
        let backward_seeks = self.backward_seek_count.load(Ordering::Relaxed);
        let max_distance = self.max_backward_seek_distance.load(Ordering::Relaxed);
        let buffer_hits = self.buffer_hit_count.load(Ordering::Relaxed);

        eprintln!();
        eprintln!("=== FUSE Seek Statistics ===");
        eprintln!("Total backward seeks: {}", backward_seeks);
        eprintln!("Buffer hits (reads served from buffer): {}", buffer_hits);
        eprintln!("Maximum backward seek distance: {} bytes ({} MB)",
                  max_distance,
                  max_distance / (1024 * 1024));
        eprintln!("Buffer size: {} MB", BUFFER_WINDOW_SIZE / (1024 * 1024));

        if max_distance > BUFFER_WINDOW_SIZE as u64 {
            eprintln!("⚠ WARNING: Maximum seek distance ({} MB) exceeds buffer size ({} MB)!",
                      max_distance / (1024 * 1024),
                      BUFFER_WINDOW_SIZE / (1024 * 1024));
            eprintln!("Consider increasing BUFFER_WINDOW_SIZE");
        } else {
            eprintln!("✓ Buffer size is adequate for observed seek pattern");
            let overhead_mb = (BUFFER_WINDOW_SIZE as u64 - max_distance) / (1024 * 1024);
            eprintln!("  Headroom: {} MB ({:.1}% of buffer unused)",
                      overhead_mb,
                      (overhead_mb as f64 / (BUFFER_WINDOW_SIZE as f64 / (1024.0 * 1024.0))) * 100.0);
        }
        eprintln!("============================");
        eprintln!();
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

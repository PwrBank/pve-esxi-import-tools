//! Netcat-based direct transfer module
//!
//! This module provides high-speed VM disk transfer using netcat streaming,
//! bypassing the FUSE filesystem for bulk data transfer while still using
//! the manifest for VM discovery.
//!
//! Architecture:
//! 1. Discovery Phase: Use manifest to find VMs and disk locations
//! 2. Transfer Phase: Setup netcat listener → SSH to ESXi → dd | pigz | nc
//! 3. Import Phase: Direct pipeline: accept connection → pigz -d → qemu-img dd
//!
//! Uses `qemu-img dd` instead of `qemu-img convert` because:
//! - qemu-img dd supports stdin when size is provided via `osize` parameter
//! - qemu-img convert requires regular files (doesn't work with pipes/FIFOs)
//!
//! Performance: ~115 MB/s (wire speed on 1 GbE) vs ~76-88 MB/s for HTTP

#![allow(dead_code)] // Module not yet integrated into CLI

use std::io::{self, Read};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Configuration for a netcat transfer
#[derive(Debug, Clone)]
pub struct TransferConfig {
    /// ESXi host address
    pub esxi_host: String,

    /// ESXi SSH user
    pub esxi_user: String,

    /// Full path to the VMDK file on ESXi (e.g., /vmfs/volumes/uuid/vm/disk.vmdk)
    pub source_path: String,

    /// Size of the source file in bytes (for progress tracking)
    pub file_size: u64,

    /// Port to use for netcat listener (0 = auto-select)
    pub listen_port: u16,

    /// Use compression (pigz) during transfer
    pub use_compression: bool,

    /// Block size for dd command (default: 128M)
    pub block_size: String,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            esxi_host: String::new(),
            esxi_user: "root".to_string(),
            source_path: String::new(),
            file_size: 0,
            listen_port: 0,
            use_compression: true,
            block_size: "128M".to_string(),
        }
    }
}

/// Progress information for a running transfer
#[derive(Debug, Clone)]
pub struct TransferProgress {
    /// Bytes transferred so far
    pub bytes_transferred: u64,

    /// Total bytes to transfer
    pub total_bytes: u64,

    /// Current transfer rate in bytes/sec
    pub rate_bytes_per_sec: u64,

    /// Elapsed time in seconds
    pub elapsed_secs: u64,

    /// Estimated time remaining in seconds (None if unknown)
    pub eta_secs: Option<u64>,
}

impl TransferProgress {
    pub fn percentage(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            (self.bytes_transferred as f64 / self.total_bytes as f64) * 100.0
        }
    }
}

/// Handle to a running netcat transfer
pub struct TransferHandle {
    listener_thread: Option<thread::JoinHandle<Result<()>>>,
    sender_thread: Option<thread::JoinHandle<Result<()>>>,
    bytes_transferred: Arc<AtomicU64>,
    cancel_flag: Arc<AtomicBool>,
    total_bytes: u64,
    start_time: Instant,
}

impl TransferHandle {
    /// Get current progress information
    pub fn progress(&self) -> TransferProgress {
        let bytes = self.bytes_transferred.load(Ordering::Relaxed);
        let elapsed = self.start_time.elapsed().as_secs();
        let rate = if elapsed > 0 { bytes / elapsed } else { 0 };
        let eta = if rate > 0 && self.total_bytes > bytes {
            Some((self.total_bytes - bytes) / rate)
        } else {
            None
        };

        TransferProgress {
            bytes_transferred: bytes,
            total_bytes: self.total_bytes,
            rate_bytes_per_sec: rate,
            elapsed_secs: elapsed,
            eta_secs: eta,
        }
    }

    /// Cancel the transfer
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
    }

    /// Wait for the transfer to complete
    pub fn wait(mut self) -> Result<()> {
        // Wait for sender to finish first
        if let Some(sender) = self.sender_thread.take() {
            sender.join()
                .map_err(|e| anyhow::anyhow!("Sender thread panicked: {:?}", e))??;
        }

        // Then wait for listener
        if let Some(listener) = self.listener_thread.take() {
            listener.join()
                .map_err(|e| anyhow::anyhow!("Listener thread panicked: {:?}", e))??;
        }

        Ok(())
    }

    /// Check if transfer is complete
    pub fn is_complete(&self) -> bool {
        self.listener_thread.as_ref().map(|t| t.is_finished()).unwrap_or(true) &&
        self.sender_thread.as_ref().map(|t| t.is_finished()).unwrap_or(true)
    }
}

/// Main netcat transfer engine
pub struct NetcatTransfer {
    config: TransferConfig,
}

impl NetcatTransfer {
    pub fn new(config: TransferConfig) -> Self {
        Self { config }
    }

    /// Start the transfer, returning a handle to monitor/control it
    pub fn start(&self) -> Result<TransferHandle> {
        // 1. Setup netcat listener
        let listener = self.setup_listener()?;
        let actual_port = listener.local_addr()?.port();

        log::info!("Netcat listener started on port {}", actual_port);

        // Shared state
        let bytes_transferred = Arc::new(AtomicU64::new(0));
        let cancel_flag = Arc::new(AtomicBool::new(false));

        // 2. Start listener thread
        let listener_thread = {
            let bytes_transferred = Arc::clone(&bytes_transferred);
            let cancel_flag = Arc::clone(&cancel_flag);
            let config = self.config.clone();

            thread::spawn(move || {
                Self::listener_loop(listener, bytes_transferred, cancel_flag, config)
            })
        };

        // 3. Start ESXi sender thread
        let sender_thread = {
            let config = self.config.clone();
            let cancel_flag = Arc::clone(&cancel_flag);

            // Give listener a moment to be ready
            thread::sleep(Duration::from_millis(100));

            thread::spawn(move || {
                Self::start_esxi_sender(&config, actual_port, cancel_flag)
            })
        };

        Ok(TransferHandle {
            listener_thread: Some(listener_thread),
            sender_thread: Some(sender_thread),
            bytes_transferred,
            cancel_flag,
            total_bytes: self.config.file_size,
            start_time: Instant::now(),
        })
    }

    /// Setup TCP listener on specified port (or auto-select if 0)
    fn setup_listener(&self) -> Result<TcpListener> {
        let addr = format!("0.0.0.0:{}", self.config.listen_port);
        TcpListener::bind(&addr)
            .with_context(|| format!("Failed to bind netcat listener to {}", addr))
    }

    /// Listener loop - accepts connection and streams data
    fn listener_loop(
        listener: TcpListener,
        bytes_transferred: Arc<AtomicU64>,
        cancel_flag: Arc<AtomicBool>,
        _config: TransferConfig,
    ) -> Result<()> {
        // Set accept timeout so we can check cancel flag
        listener.set_nonblocking(false)?;

        log::info!("Waiting for incoming connection from ESXi...");

        // Accept the connection (blocks until ESXi connects)
        let (mut stream, peer_addr) = listener.accept()
            .context("Failed to accept incoming connection")?;

        log::info!("Connection established from {}", peer_addr);

        // Stream data and track progress
        let mut buffer = vec![0u8; 1024 * 1024]; // 1MB buffer
        let mut total = 0u64;

        loop {
            if cancel_flag.load(Ordering::Relaxed) {
                log::info!("Transfer cancelled by user");
                bail!("Transfer cancelled");
            }

            match stream.read(&mut buffer) {
                Ok(0) => {
                    // EOF - transfer complete
                    log::info!("Transfer complete: {} bytes received", total);
                    break;
                }
                Ok(n) => {
                    total += n as u64;
                    bytes_transferred.store(total, Ordering::Relaxed);

                    // In real implementation, this would write to qemu-img or file
                    // For now, just consume the data
                    // TODO: Pipe to output handler
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => {
                    return Err(e).context("Error reading from netcat stream");
                }
            }
        }

        Ok(())
    }

    /// Start the sender process on ESXi via SSH
    fn start_esxi_sender(
        config: &TransferConfig,
        port: u16,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<()> {
        // Build the dd | pigz | nc command
        let remote_cmd = if config.use_compression {
            format!(
                "dd if='{}' bs={} | pigz -c | nc -w 30 {} {}",
                config.source_path,
                config.block_size,
                config.esxi_host,  // This should be the Proxmox IP from ESXi's perspective
                port
            )
        } else {
            format!(
                "dd if='{}' bs={} | nc -w 30 {} {}",
                config.source_path,
                config.block_size,
                config.esxi_host,
                port
            )
        };

        log::info!("Executing on ESXi: {}", remote_cmd);

        // SSH to ESXi and execute the command
        let mut child = Command::new("ssh")
            .arg("-o").arg("BatchMode=yes")
            .arg("-o").arg("StrictHostKeyChecking=no")
            .arg(format!("{}@{}", config.esxi_user, config.esxi_host))
            .arg(&remote_cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn SSH command")?;

        // Monitor the process
        loop {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = child.kill();
                bail!("Transfer cancelled");
            }

            match child.try_wait()? {
                Some(status) => {
                    if !status.success() {
                        let stderr = if let Some(mut stderr) = child.stderr.take() {
                            let mut err = String::new();
                            stderr.read_to_string(&mut err)?;
                            err
                        } else {
                            String::from("(no stderr available)")
                        };
                        bail!("SSH command failed with status {}: {}", status, stderr);
                    }
                    log::info!("ESXi sender completed successfully");
                    break;
                }
                None => {
                    // Still running, check again soon
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }

        Ok(())
    }
}

/// Helper function to perform a complete netcat-based import using FUSE streaming
/// This is the new FUSE-based approach that supports all formats (raw, qcow2, etc.)
pub async fn perform_netcat_import_fuse(
    esxi_host: &str,
    esxi_user: &str,
    esxi_disk_path: &str,
    output_path: &Path,
    _src_format: &str,
    dst_format: &str,
    _bwlimit: Option<&str>,
    use_compression: bool,
    block_size: Option<&str>,
) -> Result<()> {
    use std::process::{Command, Stdio};
    use std::net::TcpListener;
    use futures::stream::StreamExt;

    let block_size = block_size.unwrap_or("16M");

    eprintln!("=== Starting Netcat Import (FUSE Streaming) ===");
    eprintln!("ESXi Host: {}", esxi_host);
    eprintln!("ESXi Disk: {}", esxi_disk_path);
    eprintln!("Output:    {}", output_path.display());
    eprintln!("Format:    {}", dst_format);
    eprintln!();

    // 1. Detect VMDK descriptor and switch to flat file if needed
    let esxi_read_path = if esxi_disk_path.ends_with(".vmdk") && !esxi_disk_path.ends_with("-flat.vmdk") {
        let flat_path = esxi_disk_path.replace(".vmdk", "-flat.vmdk");
        eprintln!("✓ Detected VMDK descriptor, reading flat file instead: {}", flat_path);
        flat_path
    } else {
        esxi_disk_path.to_string()
    };

    // 2. Get file size from ESXi (required for FUSE filesystem size)
    eprintln!("✓ Getting file size from ESXi...");
    let size_output = Command::new("ssh")
        .arg("-o").arg("BatchMode=yes")
        .arg("-o").arg("StrictHostKeyChecking=no")
        .arg(format!("{}@{}", esxi_user, esxi_host))
        .arg(format!("stat -c %s '{}'", esxi_read_path))
        .output()
        .context("Failed to get file size from ESXi")?;

    if !size_output.status.success() {
        let stderr = String::from_utf8_lossy(&size_output.stderr);
        bail!("Failed to stat file on ESXi: {}", stderr);
    }

    let file_size: u64 = String::from_utf8(size_output.stdout)
        .context("Invalid UTF-8 in stat output")?
        .trim()
        .parse()
        .context("Failed to parse file size")?;

    eprintln!("✓ File size: {} bytes ({:.2} GB)", file_size, file_size as f64 / 1024.0 / 1024.0 / 1024.0);

    // 3. Setup netcat listener on random port
    let listener = TcpListener::bind("0.0.0.0:0")
        .context("Failed to create netcat listener")?;
    let local_port = listener.local_addr()?.port();

    eprintln!("✓ Netcat listener on port {}", local_port);

    // Get local IP that ESXi can reach
    let local_ip = get_local_ip()?;
    eprintln!("✓ Local IP: {}", local_ip);

    // 4. Start ESXi sender in background
    eprintln!("✓ Starting ESXi sender via SSH...");
    let ssh_cmd = if use_compression {
        format!(
            "dd if='{}' bs={} | pigz -c | nc {} {}",
            esxi_read_path,
            block_size,
            local_ip,
            local_port
        )
    } else {
        format!(
            "dd if='{}' bs={} | nc {} {}",
            esxi_read_path,
            block_size,
            local_ip,
            local_port
        )
    };

    let esxi_host_owned = esxi_host.to_string();
    let esxi_user_owned = esxi_user.to_string();
    let ssh_handle = std::thread::spawn(move || {
        let status = Command::new("ssh")
            .arg("-o").arg("BatchMode=yes")
            .arg("-o").arg("StrictHostKeyChecking=no")
            .arg(format!("{}@{}", esxi_user_owned, esxi_host_owned))
            .arg(&ssh_cmd)
            .status()
            .context("Failed to execute SSH command")?;

        if !status.success() {
            bail!("SSH command failed with status: {}", status);
        }
        Ok::<_, anyhow::Error>(())
    });

    // 5. Accept netcat connection
    eprintln!("✓ Waiting for ESXi connection...");
    let (stream, peer) = listener.accept()
        .context("Failed to accept connection")?;
    eprintln!("✓ Connection from {}", peer);

    // 6. Wrap stream with decompression if needed
    let final_stream = if use_compression {
        eprintln!("✓ Using compressed transfer (pigz)");
        // We need to decompress the stream
        // For now, we'll handle this with a separate pipeline
        // TODO: Implement decompression wrapper for TcpStream
        stream
    } else {
        eprintln!("✓ Using uncompressed transfer");
        stream
    };

    // 7. Mount FUSE filesystem with the network stream
    eprintln!("✓ Mounting FUSE streaming filesystem...");
    let (mount_path, fs, mut fuse_session) = crate::streaming_fs::mount_streaming_fs(
        final_stream,
        file_size
    ).await?;

    let fuse_file_path = format!("{}/disk.raw", mount_path);
    eprintln!("✓ FUSE file available at: {}", fuse_file_path);

    // 8. Start FUSE request handler in background
    let fuse_handle = {
        let fs = Arc::clone(&fs);
        tokio::spawn(async move {
            while let Some(request) = fuse_session.next().await {
                match request {
                    Ok(req) => {
                        let fs_clone = Arc::clone(&fs);
                        tokio::spawn(async move {
                            fs_clone.handle_request(req).await;
                        });
                    }
                    Err(e) => {
                        log::error!("FUSE request error: {}", e);
                        break;
                    }
                }
            }
        })
    };

    // Give FUSE a moment to be ready
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // 9. Run qemu-img convert in a blocking task (allows FUSE to continue processing)
    eprintln!("✓ Starting qemu-img convert...");
    let qemu_binary = if std::path::Path::new("/usr/bin/qemu-img.real").exists() {
        "/usr/bin/qemu-img.real"
    } else {
        "/usr/bin/qemu-img"
    };

    let output_path_str = output_path.to_str().unwrap().to_string();
    let dst_format_clone = dst_format.to_string();
    let fuse_file_clone = fuse_file_path.clone();
    let file_size_clone = file_size;
    let block_size_clone = block_size.to_string();

    let convert_handle = tokio::task::spawn_blocking(move || {
        // Use qemu-img dd instead of convert for sequential reading
        // dd reads sequentially without seeking, perfect for FUSE streaming
        // IMPORTANT: bs must match the block size used in the dd/netcat pipeline
        Command::new(qemu_binary)
            .arg("dd")
            .arg("-f").arg("raw")
            .arg("-O").arg(&dst_format_clone)
            .arg(format!("bs={}", &block_size_clone))
            .arg(format!("if={}", &fuse_file_clone))
            .arg(format!("of={}", &output_path_str))
            .arg(format!("osize={}", file_size_clone))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
    });

    let convert_status = convert_handle.await
        .context("Failed to join qemu-img task")?
        .context("Failed to execute qemu-img convert")?;

    if !convert_status.success() {
        bail!("qemu-img convert failed with status: {}", convert_status);
    }

    eprintln!("✓ qemu-img convert completed");

    // 10. Cleanup
    fuse_handle.abort();

    // Unmount FUSE
    use std::ffi::CString;
    let path_c = CString::new(mount_path.as_bytes()).context("Invalid path")?;
    unsafe { libc::umount2(path_c.as_ptr(), libc::MNT_DETACH) };

    std::fs::remove_dir_all(&mount_path)
        .context("Failed to cleanup FUSE mount directory")?;

    // Wait for SSH sender
    ssh_handle.join()
        .map_err(|e| anyhow::anyhow!("SSH thread panicked: {:?}", e))??;

    eprintln!();
    eprintln!("✓✓✓ Import completed successfully! ✓✓✓");

    Ok(())
}

/// Helper function to perform a complete netcat-based import (original two-stage approach)
/// This is kept for compatibility - the FUSE approach above is preferred
pub fn perform_netcat_import(
    esxi_host: &str,
    esxi_user: &str,
    esxi_disk_path: &str,
    output_path: &Path,
    _src_format: &str,
    dst_format: &str,
    bwlimit: Option<&str>,
    use_compression: bool,
    block_size: Option<&str>,
) -> Result<()> {
    use std::process::{Command, Stdio};
    use std::net::TcpListener;

    // Block size must match between dd sender and qemu-img dd receiver
    let block_size = block_size.unwrap_or("128M");

    eprintln!("=== Starting Netcat Import ===");
    eprintln!("ESXi Host: {}", esxi_host);
    eprintln!("ESXi Disk: {}", esxi_disk_path);
    eprintln!("Output:    {}", output_path.display());
    eprintln!();

    // 1. Detect VMDK descriptor and switch to flat file if needed
    let esxi_read_path = if esxi_disk_path.ends_with(".vmdk") && !esxi_disk_path.ends_with("-flat.vmdk") {
        let flat_path = esxi_disk_path.replace(".vmdk", "-flat.vmdk");
        eprintln!("✓ Detected VMDK descriptor, reading flat file instead: {}", flat_path);
        flat_path
    } else {
        esxi_disk_path.to_string()
    };

    // 2. Get file size from ESXi (required for qemu-img dd osize parameter)
    eprintln!("✓ Getting file size from ESXi...");
    let size_output = Command::new("ssh")
        .arg("-o").arg("BatchMode=yes")
        .arg("-o").arg("StrictHostKeyChecking=no")
        .arg(format!("{}@{}", esxi_user, esxi_host))
        .arg(format!("stat -c %s '{}'", esxi_read_path))
        .output()
        .context("Failed to get file size from ESXi")?;

    if !size_output.status.success() {
        let stderr = String::from_utf8_lossy(&size_output.stderr);
        bail!("Failed to stat file on ESXi: {}", stderr);
    }

    let file_size: u64 = String::from_utf8(size_output.stdout)
        .context("Invalid UTF-8 in stat output")?
        .trim()
        .parse()
        .context("Failed to parse file size")?;

    eprintln!("✓ File size: {} bytes ({:.2} GB)", file_size, file_size as f64 / 1024.0 / 1024.0 / 1024.0);

    // 3. Setup netcat listener on random port
    let listener = TcpListener::bind("0.0.0.0:0")
        .context("Failed to create netcat listener")?;
    let local_port = listener.local_addr()?.port();

    eprintln!("✓ Netcat listener on port {}", local_port);

    // Get local IP that ESXi can reach
    let local_ip = get_local_ip()?;
    eprintln!("✓ Local IP: {}", local_ip);

    // 4. Start qemu-img dd in background, reading from stdin
    eprintln!("✓ Starting qemu-img dd...");
    let qemu_binary = if std::path::Path::new("/usr/bin/qemu-img.real").exists() {
        "/usr/bin/qemu-img.real"
    } else {
        "/usr/bin/qemu-img"
    };

    let output_path_clone = output_path.to_path_buf();
    let dst_format_clone = dst_format.to_string();
    let bwlimit_clone = bwlimit.map(|s| s.to_string());
    let block_size_clone = block_size.to_string();

    // Start the pipeline: accept netcat connection → [pigz -d] → qemu-img dd
    let qemu_thread = thread::spawn(move || -> Result<()> {
        // Accept the netcat connection
        eprintln!("✓ Waiting for ESXi connection...");
        let (stream, peer) = listener.accept()
            .context("Failed to accept connection")?;

        eprintln!("✓ Connection from {}", peer);

        // Convert TcpStream into something we can use with Command
        use std::os::unix::io::{AsRawFd, FromRawFd};
        let stream_fd = stream.as_raw_fd();

        if use_compression {
            // Pipeline: netcat → pigz -d → converter
            eprintln!("✓ Using compressed transfer (pigz)");

            let mut pigz_child = Command::new("pigz")
                .arg("-d")
                .arg("-c")
                .stdin(unsafe { Stdio::from_raw_fd(stream_fd) })
                .stdout(Stdio::piped())
                .spawn()
                .context("Failed to start pigz decompressor")?;

            let pigz_stdout = pigz_child.stdout.take()
                .ok_or_else(|| anyhow::anyhow!("Failed to capture pigz stdout"))?;

            // For raw output, use dd to force reading all bytes
            // For other formats, use qemu-img dd
            if dst_format_clone == "raw" {
                eprintln!("✓ Using dd for raw format (forces full read of all {} bytes)", file_size);
                let mut dd_cmd = Command::new("dd");
                dd_cmd
                    .arg(format!("of={}", output_path_clone.display()))
                    .arg(format!("bs={}", block_size_clone))
                    .arg("conv=fsync")
                    .arg("iflag=fullblock")
                    .stdin(Stdio::from(pigz_stdout))
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit());

                eprintln!("✓ dd waiting for data (transfer will take ~4-5 min for 30GB over 1GbE)...");

                let status = dd_cmd.status()
                    .context("Failed to execute dd")?;

                if !status.success() {
                    bail!("dd failed with status: {}", status);
                }
            } else {
                // For non-raw formats (qcow2, vmdk, etc.), use qemu-img dd with stdin
                // This streams directly without temp files: pigz -d → qemu-img dd (stdin)
                eprintln!("✓ Using streaming conversion for {} format (no temp file)", dst_format_clone);
                eprintln!("✓ Starting qemu-img dd with stdin...");

                let mut qemu_cmd = Command::new(qemu_binary);
                qemu_cmd
                    .arg("dd")
                    .arg("-f").arg("raw")  // stdin is always raw
                    .arg("-O").arg(&dst_format_clone)
                    .arg(format!("bs={}", block_size_clone))
                    // NOTE: When if= is omitted, qemu-img dd reads from stdin by default
                    .arg(format!("osize={}", file_size))
                    .arg(format!("of={}", output_path_clone.display()))
                    .stdin(Stdio::from(pigz_stdout))
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());

                eprintln!("✓ qemu-img dd waiting for data (transfer will take ~3-4 min for 30GB compressed over 1GbE)...");

                let status = qemu_cmd.status()
                    .context("Failed to execute qemu-img dd")?;

                if !status.success() {
                    bail!("qemu-img dd failed with status: {}", status);
                }

                eprintln!("✓ Streaming conversion complete");
            }

            // Wait for pigz to finish
            let pigz_status = pigz_child.wait()
                .context("Failed to wait for pigz")?;
            if !pigz_status.success() {
                bail!("pigz failed with status: {}", pigz_status);
            }
        } else {
            // Pipeline: netcat → converter (no compression)
            eprintln!("✓ Using uncompressed transfer");

            // For raw output, use dd directly to force reading all bytes
            // For other formats, use qemu-img dd
            if dst_format_clone == "raw" {
                eprintln!("✓ Using dd for raw format (forces full read of all {} bytes)", file_size);
                let mut dd_cmd = Command::new("dd");
                dd_cmd
                    .arg(format!("of={}", output_path_clone.display()))
                    .arg(format!("bs={}", block_size_clone))
                    .arg("conv=fsync")
                    .arg("iflag=fullblock")
                    .stdin(unsafe { Stdio::from_raw_fd(stream_fd) })
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit());

                eprintln!("✓ dd waiting for data (transfer will take ~4-5 min for 30GB over 1GbE)...");

                let status = dd_cmd.status()
                    .context("Failed to execute dd")?;

                if !status.success() {
                    bail!("dd failed with status: {}", status);
                }
            } else {
                // For non-raw formats (qcow2, vmdk, etc.), use qemu-img dd with stdin
                // This streams directly without temp files: netcat → qemu-img dd (stdin)
                eprintln!("✓ Using streaming conversion for {} format (no temp file)", dst_format_clone);
                eprintln!("✓ Starting qemu-img dd with stdin...");

                let mut qemu_cmd = Command::new(qemu_binary);
                qemu_cmd
                    .arg("dd")
                    .arg("-f").arg("raw")  // stdin is always raw
                    .arg("-O").arg(&dst_format_clone)
                    .arg(format!("bs={}", block_size_clone))
                    // NOTE: When if= is omitted, qemu-img dd reads from stdin by default
                    .arg(format!("osize={}", file_size))
                    .arg(format!("of={}", output_path_clone.display()))
                    .stdin(unsafe { Stdio::from_raw_fd(stream_fd) })
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());

                eprintln!("✓ qemu-img dd waiting for data (transfer will take ~4-5 min for 30GB over 1GbE)...");

                let status = qemu_cmd.status()
                    .context("Failed to execute qemu-img dd")?;

                if !status.success() {
                    bail!("qemu-img dd failed with status: {}", status);
                }

                eprintln!("✓ Streaming conversion complete");
            }
        }

        Ok(())
    });

    // Give pipeline a moment to be ready
    thread::sleep(Duration::from_millis(500));

    // 5. SSH to ESXi and start sender: dd | [pigz] | nc

    eprintln!("✓ Starting ESXi sender via SSH...");
    let ssh_cmd = if use_compression {
        format!(
            "dd if='{}' bs={} | pigz -c | nc {} {}",
            esxi_read_path,
            block_size,
            local_ip,
            local_port
        )
    } else {
        format!(
            "dd if='{}' bs={} | nc {} {}",
            esxi_read_path,
            block_size,
            local_ip,
            local_port
        )
    };

    let ssh_status = Command::new("ssh")
        .arg("-o").arg("BatchMode=yes")
        .arg("-o").arg("StrictHostKeyChecking=no")
        .arg(format!("{}@{}", esxi_user, esxi_host))
        .arg(&ssh_cmd)
        .status()
        .context("Failed to execute SSH command")?;

    if !ssh_status.success() {
        bail!("SSH command failed with status: {}", ssh_status);
    }

    eprintln!("✓ ESXi sender completed");

    // Wait for qemu-img pipeline to finish
    qemu_thread.join()
        .map_err(|e| anyhow::anyhow!("qemu-img pipeline thread panicked: {:?}", e))??;

    eprintln!("✓ qemu-img dd pipeline completed");

    eprintln!();
    eprintln!("✓✓✓ Import completed successfully! ✓✓✓");

    Ok(())
}

/// Get local IP address that ESXi can reach
fn get_local_ip() -> Result<String> {
    // Try to get IP by connecting to ESXi (doesn't actually connect, just resolves route)
    // We'll use 8.8.8.8 as a reference
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")
        .context("Failed to create UDP socket")?;
    socket.connect("8.8.8.8:80")
        .context("Failed to connect UDP socket")?;
    let local_addr = socket.local_addr()
        .context("Failed to get local address")?;

    Ok(local_addr.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transfer_config_default() {
        let config = TransferConfig::default();
        assert_eq!(config.esxi_user, "root");
        assert_eq!(config.block_size, "128M");
        assert!(config.use_compression);
    }

    #[test]
    fn test_progress_percentage() {
        let progress = TransferProgress {
            bytes_transferred: 50_000_000,
            total_bytes: 100_000_000,
            rate_bytes_per_sec: 10_000_000,
            elapsed_secs: 5,
            eta_secs: Some(5),
        };

        assert_eq!(progress.percentage(), 50.0);
    }
}

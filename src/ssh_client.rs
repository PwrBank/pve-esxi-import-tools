use std::fmt;
use std::ops::Range;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, format_err, Context as _, Error};
use hyper::body::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Semaphore};

use crate::esxi::{EofReached, IsDirectory, NotFound};

/// A persistent SSH session that can execute multiple commands
struct PersistentSshSession {
    child: Child,
}

impl PersistentSshSession {
    /// Create a new persistent SSH session
    async fn new(host: &str, user: &str) -> Result<Self, Error> {
        let ssh_host = format!("{}@{}", user, host);

        let child = Command::new("ssh")
            .arg("-o").arg("BatchMode=yes")
            .arg("-o").arg("StrictHostKeyChecking=no")
            .arg("-o").arg("ServerAliveInterval=60")
            .arg("-o").arg("ServerAliveCountMax=3")
            .arg(&ssh_host)
            .arg("sh") // Start a shell session
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn persistent SSH session")?;

        log::debug!("Created persistent SSH session to {}", ssh_host);

        Ok(Self { child })
    }

    /// Execute a dd command and read the output
    async fn read_range(&mut self, path: &str, skip_blocks: u64, read_blocks: u64, block_size: u64) -> Result<Vec<u8>, Error> {
        let stdin = self.child.stdin.as_mut()
            .ok_or_else(|| format_err!("stdin not available"))?;
        let stdout = self.child.stdout.as_mut()
            .ok_or_else(|| format_err!("stdout not available"))?;

        // Send the dd command
        let cmd = format!(
            "dd if='{}' bs={} skip={} count={} 2>/dev/null; echo DONE_$?\n",
            path, block_size, skip_blocks, read_blocks
        );

        stdin.write_all(cmd.as_bytes()).await
            .context("failed to write command to SSH session")?;
        stdin.flush().await
            .context("failed to flush SSH stdin")?;

        // Read the dd output
        let expected_size = (read_blocks * block_size) as usize;
        let mut buffer = Vec::with_capacity(expected_size);

        // Read data until we see "DONE_" marker
        let mut marker_buf = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            stdout.read_exact(&mut byte).await
                .context("failed to read from SSH session")?;

            if byte[0] == b'D' || !marker_buf.is_empty() {
                marker_buf.push(byte[0]);
                if marker_buf.starts_with(b"DONE_") {
                    // Read the exit code and newline
                    let mut exit_code = Vec::new();
                    loop {
                        let mut byte = [0u8; 1];
                        stdout.read_exact(&mut byte).await?;
                        if byte[0] == b'\n' {
                            break;
                        }
                        exit_code.push(byte[0]);
                    }

                    // Check if dd succeeded
                    let code = String::from_utf8_lossy(&exit_code);
                    if code.trim() != "0" {
                        bail!("dd command failed with exit code: {}", code);
                    }

                    break;
                }

                // False alarm, wasn't the marker
                if marker_buf.len() >= 5 && !b"DONE_".starts_with(&marker_buf) {
                    buffer.extend_from_slice(&marker_buf);
                    marker_buf.clear();
                }
            } else {
                buffer.push(byte[0]);
            }
        }

        Ok(buffer)
    }

    /// Check if the session is still alive
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for PersistentSshSession {
    fn drop(&mut self) {
        // Try to kill the SSH process gracefully
        let _ = self.child.start_kill();
    }
}

/// Pool of persistent SSH sessions
struct SshSessionPool {
    host: String,
    user: String,
    sessions: Vec<Mutex<Option<PersistentSshSession>>>,
}

impl SshSessionPool {
    fn new(host: String, user: String, pool_size: usize) -> Self {
        let sessions = (0..pool_size)
            .map(|_| Mutex::new(None))
            .collect();

        Self {
            host,
            user,
            sessions,
        }
    }

    /// Get a session from the pool, creating a new one if needed
    async fn get_session(&self, index: usize) -> Result<tokio::sync::MutexGuard<'_, Option<PersistentSshSession>>, Error> {
        let mut guard = self.sessions[index].lock().await;

        // Check if we need to create or recreate the session
        let needs_new = match guard.as_mut() {
            None => true,
            Some(session) => !session.is_alive(),
        };

        if needs_new {
            log::debug!("Creating new persistent SSH session #{}", index);
            let new_session = PersistentSshSession::new(&self.host, &self.user).await?;
            *guard = Some(new_session);
        }

        Ok(guard)
    }
}

/// SSH-based client for streaming data directly from ESXi datastores
/// Uses dd + SSH with persistent connections to bypass HTTP API throttling
pub struct SshClient {
    host: String,
    user: String,
    /// Pool of persistent SSH sessions
    session_pool: Arc<SshSessionPool>,
    /// Limits concurrent SSH operations
    connection_pool: Arc<Semaphore>,
    /// Counter for round-robin session selection
    session_counter: AtomicU64,
}

impl SshClient {
    /// Create a new SSH client with persistent connections
    pub fn new(host: String, user: String, max_connections: usize) -> Self {
        log::info!(
            "Initializing SSH client with {} persistent connections to {}@{}",
            max_connections, user, host
        );

        Self {
            session_pool: Arc::new(SshSessionPool::new(
                host.clone(),
                user.clone(),
                max_connections,
            )),
            host,
            user,
            connection_pool: Arc::new(Semaphore::new(max_connections)),
            session_counter: AtomicU64::new(0),
        }
    }

    /// Build the full SSH host string (user@host)
    fn ssh_host(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// Get the datastore path for a file
    fn datastore_path(&self, _datacenter: &str, datastore: &str, path: &str) -> String {
        // ESXi datastore paths: /vmfs/volumes/<datastore>/<path>
        format!("/vmfs/volumes/{}/{}", datastore, path.trim_start_matches('/'))
    }

    /// Check if a path exists and get its size
    async fn stat_file(&self, full_path: &str) -> Result<Option<u64>, Error> {
        let _permit = self.connection_pool.acquire().await?;

        let output = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg(&self.ssh_host())
            .arg(format!(
                "if [ -f '{}' ]; then stat -c %s '{}'; exit 0; elif [ -d '{}' ]; then exit 2; else exit 1; fi",
                full_path, full_path, full_path
            ))
            .output()
            .await
            .context("failed to execute ssh stat command")?;

        match output.status.code() {
            Some(0) => {
                // File exists, parse size
                let size_str = String::from_utf8_lossy(&output.stdout);
                let size = size_str
                    .trim()
                    .parse::<u64>()
                    .context("failed to parse file size")?;
                Ok(Some(size))
            }
            Some(1) => {
                // File not found
                Ok(None)
            }
            Some(2) => {
                // Is a directory
                Err(IsDirectory.into())
            }
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("ssh stat command failed: {}", stderr)
            }
        }
    }

    /// Get the size of a file
    pub async fn get_file_size(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
    ) -> Result<u64, Error> {
        let full_path = self.datastore_path(datacenter, datastore, path);
        match self.stat_file(&full_path).await? {
            Some(size) => Ok(size),
            None => Err(NotFound.into()),
        }
    }

    /// Check if a path exists
    pub async fn path_exists(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
    ) -> Result<bool, Error> {
        let full_path = self.datastore_path(datacenter, datastore, path);
        match self.stat_file(&full_path).await {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(err) if err.downcast_ref::<IsDirectory>().is_some() => Ok(true),
            Err(err) => Err(err),
        }
    }

    /// Open a file for reading
    pub async fn open_file(
        self: &Arc<Self>,
        datacenter: &str,
        datastore: &str,
        path: &str,
    ) -> Result<SshFile, Error> {
        log::debug!("open file via SSH [{datacenter}, {datastore}] {path:?}");
        let full_path = self.datastore_path(datacenter, datastore, path);

        // Get file size - IMPORTANT: preserve IsDirectory error for FUSE filesystem
        let size = match self.get_file_size(datacenter, datastore, path).await {
            Ok(size) => size,
            Err(err) => {
                // Preserve IsDirectory error so FUSE can detect directories
                if err.downcast_ref::<IsDirectory>().is_some() {
                    return Err(err);
                }
                // Add context for other errors
                return Err(err.context(format!(
                    "error when getting file size: {datacenter:?}/{datastore:?}/{path:?}"
                )));
            }
        };

        Ok(SshFile {
            client: Arc::clone(self),
            path: full_path.into(),
            size: AtomicU64::new(size),
            at: 0,
        })
    }

    /// Read a specific byte range from a file using dd over persistent SSH connection
    async fn read_range(&self, path: &str, range: Range<u64>) -> Result<Bytes, Error> {
        // Acquire a permit from the semaphore
        let _permit = self.connection_pool.acquire().await?;
        // Use round-robin to select a session
        let session_index = (self.session_counter.fetch_add(1, Ordering::Relaxed) as usize) % self.session_pool.sessions.len();

        let skip_bytes = range.start;
        let count_bytes = range.end - range.start;

        // Use 1MB blocks for good performance while maintaining byte-level precision
        const BLOCK_SIZE: u64 = 1024 * 1024; // 1MB

        // Calculate skip in blocks and bytes
        let skip_blocks = skip_bytes / BLOCK_SIZE;
        let skip_remainder = skip_bytes % BLOCK_SIZE;

        // Read slightly more than needed if we have a remainder offset,
        // then trim to exact range in memory
        let total_to_read = if skip_remainder > 0 {
            skip_remainder + count_bytes
        } else {
            count_bytes
        };

        let read_blocks = (total_to_read + BLOCK_SIZE - 1) / BLOCK_SIZE;

        log::debug!(
            "SSH dd (session #{}): {} skip={} count={} (offset={}, len={})",
            session_index,
            path,
            skip_blocks,
            read_blocks,
            skip_bytes,
            count_bytes
        );

        // Get a session from the pool
        let mut session_guard = self.session_pool.get_session(session_index).await?;
        let session = session_guard.as_mut()
            .ok_or_else(|| format_err!("failed to get session from pool"))?;

        // Use the persistent session to read the range
        let buffer = session.read_range(path, skip_blocks, read_blocks, BLOCK_SIZE).await?;

        // Trim buffer to exact byte range requested
        let trim_start = skip_remainder as usize;
        let trim_end = trim_start + count_bytes as usize;

        if buffer.len() < trim_end {
            // Reached EOF or short read
            if buffer.len() <= trim_start {
                return Err(EofReached.into());
            }
            // Return what we got, trimmed appropriately
            let trimmed = buffer[trim_start..].to_vec();
            return Ok(Bytes::from(trimmed));
        }

        // Trim to exact range
        let trimmed = buffer[trim_start..trim_end].to_vec();
        Ok(Bytes::from(trimmed))
    }
}

/// Represents an open file accessed via SSH
pub struct SshFile {
    client: Arc<SshClient>,
    path: Arc<str>,
    size: AtomicU64,
    at: u64,
}

impl Clone for SshFile {
    fn clone(&self) -> Self {
        Self {
            client: Arc::clone(&self.client),
            path: Arc::clone(&self.path),
            size: AtomicU64::new(self.size.load(Ordering::Acquire)),
            at: 0, // Reset position for cloned file
        }
    }
}

impl SshFile {
    /// Get the file size
    pub fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }

    /// Read an arbitrary range of data from the file
    pub async fn read_at(&self, range: Range<u64>) -> Result<Bytes, Error> {
        match self.client.read_range(&self.path, range).await {
            Ok(bytes) => Ok(bytes),
            Err(err) if err.is::<EofReached>() => Ok(Bytes::new()),
            Err(err) => Err(err),
        }
    }
}

// Note: AsyncRead implementation for SshFile would be complex and isn't needed
// since we use read_at() directly in the FUSE implementation

impl fmt::Debug for SshFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SshFile")
            .field("path", &self.path)
            .field("size", &self.size())
            .field("at", &self.at)
            .finish()
    }
}

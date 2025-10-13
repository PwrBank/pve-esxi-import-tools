use std::fmt;
use std::ops::Range;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, format_err, Context as _, Error};
use hyper::body::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::esxi::{EofReached, IsDirectory, NotFound};

/// SSH-based client for streaming data directly from ESXi datastores
/// Uses dd + SSH to bypass HTTP API throttling
pub struct SshClient {
    host: String,
    user: String,
    /// Limits concurrent SSH connections
    connection_pool: Arc<Semaphore>,
}

impl SshClient {
    /// Create a new SSH client
    pub fn new(host: String, user: String, max_connections: usize) -> Self {
        Self {
            host,
            user,
            connection_pool: Arc::new(Semaphore::new(max_connections)),
        }
    }

    /// Build the full SSH host string (user@host)
    fn ssh_host(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// Get the datastore path for a file
    fn datastore_path(&self, datacenter: &str, datastore: &str, path: &str) -> String {
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
        let size = self
            .get_file_size(datacenter, datastore, path)
            .await
            .with_context(|| {
                format!("error when getting file size: {datacenter:?}/{datastore:?}/{path:?}")
            })?;

        Ok(SshFile {
            client: Arc::clone(self),
            path: full_path.into(),
            size: AtomicU64::new(size),
            at: 0,
        })
    }

    /// Read a specific byte range from a file using dd over SSH
    async fn read_range(&self, path: &str, range: Range<u64>) -> Result<Bytes, Error> {
        let _permit = self.connection_pool.acquire().await?;

        let skip_bytes = range.start;
        let count_bytes = range.end - range.start;

        // Use dd with bs=1 for precise byte-level control
        // Alternatively, could use bs=1M with calculated skip/count
        let dd_cmd = format!(
            "dd if='{}' bs=1 skip={} count={} 2>/dev/null",
            path, skip_bytes, count_bytes
        );

        log::debug!(
            "SSH dd command: {} ({}..{}, {} bytes)",
            path,
            skip_bytes,
            range.end,
            count_bytes
        );

        let mut child = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg(&self.ssh_host())
            .arg(dd_cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn ssh dd process")?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| format_err!("failed to capture stdout"))?;

        let mut buffer = Vec::with_capacity(count_bytes as usize);
        stdout
            .read_to_end(&mut buffer)
            .await
            .context("failed to read from ssh stdout")?;

        let status = child.wait().await.context("failed to wait for ssh process")?;
        if !status.success() {
            bail!("ssh dd command failed with status: {}", status);
        }

        if buffer.len() < count_bytes as usize {
            // Reached EOF
            if buffer.is_empty() {
                return Err(EofReached.into());
            }
        }

        Ok(Bytes::from(buffer))
    }
}

/// Represents an open file accessed via SSH
pub struct SshFile {
    client: Arc<SshClient>,
    path: Arc<str>,
    size: AtomicU64,
    at: u64,
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

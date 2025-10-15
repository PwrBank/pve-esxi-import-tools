///! Unified client interface for both HTTP and SSH-based datastore access

use std::io;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::Error;
use hyper::body::Bytes;
use tokio::io::AsyncRead;

use crate::esxi::{EsxiClient, EsxiFile};
use crate::ssh_client::{SshClient, SshFile};

/// Unified client that can use either HTTP or SSH backend
pub enum DatastoreClient {
    Http(Arc<EsxiClient>),
    Ssh(Arc<SshClient>),
}

impl DatastoreClient {
    /// Get the size of a file
    pub async fn get_file_size(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
    ) -> Result<u64, Error> {
        match self {
            Self::Http(client) => client.get_file_size(datacenter, datastore, path).await,
            Self::Ssh(client) => client.get_file_size(datacenter, datastore, path).await,
        }
    }

    /// Check if a path exists
    pub async fn path_exists(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
    ) -> Result<bool, Error> {
        match self {
            Self::Http(client) => client.path_exists(datacenter, datastore, path).await,
            Self::Ssh(client) => client.path_exists(datacenter, datastore, path).await,
        }
    }

    /// Open a file for reading
    pub async fn open_file(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
    ) -> Result<DatastoreFile, Error> {
        match self {
            Self::Http(client) => {
                let file = client.open_file(datacenter, datastore, path).await?;
                Ok(DatastoreFile::Http(file))
            }
            Self::Ssh(client) => {
                let file = client.open_file(datacenter, datastore, path).await?;
                Ok(DatastoreFile::Ssh(file))
            }
        }
    }
}

/// Unified file handle that can be either HTTP or SSH-based
#[derive(Clone)]
pub enum DatastoreFile {
    Http(EsxiFile),
    Ssh(SshFile),
}

impl DatastoreFile {
    /// Get the file size
    pub fn size(&self) -> u64 {
        match self {
            Self::Http(file) => file.size(),
            Self::Ssh(file) => file.size(),
        }
    }

    /// Read an arbitrary range of data from the file
    pub async fn read_at(&self, range: Range<u64>) -> Result<Bytes, Error> {
        match self {
            Self::Http(file) => file.read_at(range).await,
            Self::Ssh(file) => file.read_at(range).await,
        }
    }
}

impl AsyncRead for DatastoreFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Http(file) => Pin::new(file).poll_read(cx, buf),
            Self::Ssh(_file) => {
                // SSH files don't support streaming AsyncRead
                // This is mainly used for parsing VMX files which are small
                // In practice, the VMX parser should use read_at() for SSH mode
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "AsyncRead not supported for SSH files, use read_at() instead",
                )))
            }
        }
    }
}

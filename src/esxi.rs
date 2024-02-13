use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::io;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{bail, format_err, Context as _, Error};
use http::{Request, Response};
use hyper::body::Bytes;
use hyper::Body;
use openssl::ssl::SslConnector;
use percent_encoding::{percent_encode, AsciiSet};
use tokio::io::AsyncRead;
use tokio::task::JoinHandle;

use proxmox_http::client::Client;

#[derive(Clone, Copy, Debug)]
pub struct NotFound;

impl fmt::Display for NotFound {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("file not found")
    }
}

impl StdError for NotFound {}

#[derive(Clone, Copy, Debug)]
pub struct IsDirectory;

impl fmt::Display for IsDirectory {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("not a file, may be a directory")
    }
}

impl StdError for IsDirectory {}

const PATH_ESCAPE_ALPHABET: AsciiSet = percent_encoding::NON_ALPHANUMERIC.remove(b'/');

/// The HTTP client used to download files (or parts of files) from an esxi or vcenter host.
pub struct EsxiClient {
    client: Client,
    folder_url: String,
    auth_header: String,
}

impl EsxiClient {
    /// Create a new client - we need a URL, use and password. Additionally we have an SSL
    /// connector which deals with TLS/certificates, this is left up to the caller.
    pub fn new<S0, S1, S2>(base_url: &S0, user: &S1, password: &S2, connector: SslConnector) -> Self
    where
        S0: fmt::Display + ?Sized,
        S1: fmt::Display + ?Sized,
        S2: fmt::Display + ?Sized,
    {
        let creds = openssl::base64::encode_block(format!("{user}:{password}").as_bytes());

        Self {
            folder_url: format!("{base_url}/folder"),
            auth_header: format!("Basic {creds}"),
            client: Client::with_ssl_connector(connector, Default::default()),
        }
    }

    /// Download a complete file.
    pub async fn download_file(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
    ) -> Result<Bytes, Error> {
        self.download(datacenter, datastore, path, None).await
    }

    /// Download a range from a file.
    pub async fn download_range(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
        range: Range<u64>,
    ) -> Result<Bytes, Error> {
        self.download(datacenter, datastore, path, Some(range))
            .await
    }

    fn datacenter_url(&self, datacenter: &str) -> String {
        let datacenter = percent_encode(datacenter.as_bytes(), &percent_encoding::NON_ALPHANUMERIC);

        format!("{}/?dcPath={datacenter}", self.folder_url)
    }

    fn datastore_url(&self, datacenter: &str, datastore: &str) -> String {
        let datacenter = percent_encode(datacenter.as_bytes(), &percent_encoding::NON_ALPHANUMERIC);
        let datastore = percent_encode(datastore.as_bytes(), &percent_encoding::NON_ALPHANUMERIC);

        format!(
            "{}/?dcPath={datacenter}&dsName={datastore}",
            self.folder_url
        )
    }

    fn file_url(&self, datacenter: &str, datastore: &str, path: &str) -> String {
        let datacenter = percent_encode(datacenter.as_bytes(), &percent_encoding::NON_ALPHANUMERIC);
        let datastore = percent_encode(datastore.as_bytes(), &percent_encoding::NON_ALPHANUMERIC);
        let path = percent_encode(path.as_bytes(), &PATH_ESCAPE_ALPHABET);

        format!(
            "{}/{path}?dcPath={datacenter}&dsName={datastore}",
            self.folder_url
        )
    }

    async fn make_request<F>(&self, mut make_req: F) -> Result<Response<Body>, Error>
    where
        F: FnMut() -> Result<http::request::Builder, Error>,
    {
        let mut retry = 0;
        loop {
            retry += 1;

            let req = make_req()?
                .header("authorization", &self.auth_header)
                .body(Body::empty())
                .context("failed to build http request")?;

            let response = self
                .client
                .request(req)
                .await
                .context("http request failed")?;

            let status = response.status();
            if status.as_u16() == 503 {
                if retry < 5 {
                    log::error!("rate limited, retrying ({retry} of 5)...");
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    continue;
                }

                log::error!("rate limited => {response:?}");
                bail!("rate limited");
            }

            if status.as_u16() == 404 {
                return Err(NotFound.into());
            }

            if !status.is_success() {
                bail!("http error code {status:?}");
            }

            return Ok(response);
        }
    }

    /// Download a range from a file.
    pub async fn download(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
        range: Option<Range<u64>>,
    ) -> Result<Bytes, Error> {
        self.download_do(&self.file_url(datacenter, datastore, path), range)
            .await
    }

    async fn download_do(&self, query: &str, range: Option<Range<u64>>) -> Result<Bytes, Error> {
        let (parts, body) = self
            .make_request(|| {
                let mut req = Request::get(query);

                if let Some(range) = &range {
                    req = req.header(
                        "range",
                        &format!("bytes={}-{}", range.start, range.end.saturating_sub(1)),
                    )
                }

                Ok(req)
            })
            .await?
            .into_parts();

        let content_type = parts.headers.get("content-type").ok_or_else(|| {
            format_err!(
                "http response did not declare the content type, expected application/octet-stream"
            )
        })?;
        if content_type != "application/octet-stream" {
            bail!("unexpected content type: {content_type:?}");
        }

        let body = hyper::body::to_bytes(body).await?;

        Ok(body)
    }

    /// Get the size of a file.
    pub async fn get_file_size(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
    ) -> Result<u64, Error> {
        let response = self
            .make_request(|| Ok(Request::head(self.file_url(datacenter, datastore, path))))
            .await?;

        let headers = response.headers();

        let content_type = headers
            .get("content-type")
            .ok_or_else(|| format_err!("http response did not include a content-type"))?
            .to_str()
            .context("content-type header is not a proper string")?;

        if content_type != "application/octet-stream" {
            return Err(IsDirectory.into());
        }

        headers
            .get("content-length")
            .ok_or_else(|| format_err!("http response did not include a content-length"))?
            .to_str()
            .context("content-length header is not a number")?
            .parse::<u64>()
            .context("failed to parse content size")
    }

    /// Get a `Read`able file.
    pub async fn open_file(
        self: &Arc<Self>,
        datacenter: &str,
        datastore: &str,
        path: &str,
    ) -> Result<EsxiFile, Error> {
        log::debug!("open file [{datacenter}, {datastore}] {path:?}");
        let query = self.file_url(datacenter, datastore, path);
        let size = self.get_file_size(datacenter, datastore, path).await?;
        Ok(EsxiFile {
            client: Arc::clone(self),
            query: query.into(),
            size,
            at: 0,
            state: ReadState::New,
        })
    }
}

enum ReadState {
    New,
    Have { data: Bytes, at: usize },
    Reading(JoinHandle<Result<Bytes, Error>>),
    Eof,
}

/// This provides `AsyncRead` for a remote file, so that we can use it with eg. tokio's `BufReader`
/// to parse config files and avoid downloading the entire thing at once.
pub struct EsxiFile {
    client: Arc<EsxiClient>,
    query: Arc<str>,
    size: u64,
    at: u64,
    state: ReadState,
}

impl EsxiFile {
    /// Get the file size. This is cached from the `HEAD` request made at `open_file` time, so if
    /// the file size changes in between, this is not updated.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Read an arbitrary range of data from the file.
    pub async fn read_at(&self, range: Range<u64>) -> Result<Bytes, Error> {
        self.client.download_do(&self.query, Some(range)).await
    }
}

impl AsyncRead for EsxiFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        loop {
            match &mut this.state {
                ReadState::Eof => return Poll::Ready(Ok(())),
                ReadState::New => (), // fall through to the read code
                ReadState::Have { data, at } => {
                    let data = &**data;
                    let data = &data[*at..];
                    if !data.is_empty() {
                        let put = data.len().min(buf.remaining());
                        buf.put_slice(&data[..put]);
                        *at += put;
                        return Poll::Ready(Ok(()));
                    }
                    // otherwise fall through to the read code
                }
                ReadState::Reading(fut) => {
                    let data = match Pin::new(fut).poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(result) => match result {
                            Ok(Ok(bytes)) => bytes,
                            Ok(Err(err)) => {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::Other,
                                    err.to_string(),
                                )));
                            }
                            Err(err) => {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::Other,
                                    err.to_string(),
                                )));
                            }
                        },
                    };

                    this.at = this.at.saturating_add(data.len() as u64).min(this.size);
                    this.state = ReadState::Have { data, at: 0 };
                    continue;
                }
            }

            if this.at == this.size {
                this.state = ReadState::Eof;
                return Poll::Ready(Ok(()));
            }

            let client = Arc::clone(&this.client);
            let query = Arc::clone(&this.query);
            let remaining = buf.remaining() as u64;
            let range = this.at..(this.at + remaining);
            this.state = ReadState::Reading(tokio::spawn(async move {
                client.download_do(&query, Some(range)).await
            }));
        }
    }
}

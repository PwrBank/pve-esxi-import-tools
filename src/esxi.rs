use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::io;
use std::ops::Range;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::task::{ready, Context, Poll};

use anyhow::{bail, format_err, Context as _, Error};
use http::{Request, Response};
use hyper::body::Bytes;
use hyper::Body;
use openssl::ssl::SslConnector;
use percent_encoding::{percent_encode, AsciiSet};
use tokio::io::AsyncRead;
use tokio::task::JoinHandle;

use tokio::sync::SemaphorePermit;

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
pub struct EofReached;

impl fmt::Display for EofReached {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("file not found")
    }
}

impl StdError for EofReached {}

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
    connection_limit: ConnectionLimit,
    session_cookie: StdMutex<Option<String>>,
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
            connection_limit: ConnectionLimit::new(),
            session_cookie: StdMutex::new(None),
        }
    }

    fn file_url(&self, datacenter: &str, datastore: &str, path: &str) -> String {
        let datacenter = percent_encode(datacenter.as_bytes(), percent_encoding::NON_ALPHANUMERIC);
        let datastore = percent_encode(datastore.as_bytes(), percent_encoding::NON_ALPHANUMERIC);
        let path = percent_encode(path.as_bytes(), &PATH_ESCAPE_ALPHABET);

        format!(
            "{}/{path}?dcPath={datacenter}&dsName={datastore}",
            self.folder_url
        )
    }

    fn update_cookie(&self, headers: &hyper::HeaderMap) {
        for cookie in headers.get_all(hyper::header::SET_COOKIE) {
            let Ok(cookie) = cookie.to_str() else {
                continue
            };

            if cookie.starts_with("vmware_soap_session") {
                *self.session_cookie.lock().unwrap() = Some(cookie.to_string());
            }
        }
    }

    async fn make_request<F>(&self, mut make_req: F) -> Result<Response<Body>, Error>
    where
        F: FnMut() -> Result<http::request::Builder, Error>,
    {
        let mut permit = self.connection_limit.acquire().await;

        let mut retry = 0;
        let mut retry_guard = None;
        loop {
            retry += 1;

            let mut req = make_req()?;

            if let Some(cookie) = self.session_cookie.lock().unwrap().as_deref() {
                req = req.header(hyper::header::COOKIE, cookie);
            }

            let req = req
                .header("authorization", &self.auth_header)
                .body(Body::empty())
                .context("failed to build http request")?;

            let response = self
                .client
                .request(req)
                .await
                .context("http request failed")?;

            self.update_cookie(response.headers());

            let status = response.status();
            if status.as_u16() == 503 {
                if retry_guard.is_none() {
                    let guard;
                    (guard, permit) = self.connection_limit.retry_guard(permit).await;
                    retry_guard = Some(guard);
                }

                if retry < 5 {
                    log::warn!("rate limited, retrying ({retry} of 5)...");
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    continue;
                }

                log::error!("rate limited => {response:?}");
                bail!("rate limited");
            }

            if status.as_u16() == 404 {
                return Err(NotFound.into());
            }

            if status.as_u16() == 416 {
                return Err(EofReached.into());
            }

            if !status.is_success() {
                bail!("http error code {status:?}");
            }

            return Ok(response);
        }
    }

    /// This returns the body and, if provided, the file's size.
    async fn download_do(
        &self,
        query: &str,
        range: Option<Range<u64>>,
    ) -> Result<(Bytes, Option<u64>), Error> {
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

        let file_size = parts
            .headers
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes "))
            .and_then(|value| match value.rfind('/') {
                Some(slash) => Some(&value[(slash + 1)..]),
                None => None,
            })
            .and_then(|value| value.parse().ok());

        let body = hyper::body::to_bytes(body).await?;

        Ok((body, file_size))
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

    /// Check the existence of an URL.
    pub async fn path_exists(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
    ) -> Result<bool, Error> {
        match self
            .make_request(|| Ok(Request::head(self.file_url(datacenter, datastore, path))))
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if err.downcast_ref::<NotFound>().is_some() => Ok(false),
            Err(err) => Err(err),
        }
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
        let size = self
            .get_file_size(datacenter, datastore, path)
            .await
            .with_context(|| {
                format!("error when getting file size: {datacenter:?}/{datastore:?}/{path:?}")
            })?;
        Ok(EsxiFile {
            client: Arc::clone(self),
            query: query.into(),
            size: AtomicU64::new(size),
            at: 0,
            state: ReadState::New,
        })
    }
}

enum ReadState {
    New,
    Have { data: Bytes, at: usize },
    Reading(JoinHandle<Result<(Bytes, Option<u64>), Error>>),
    Eof,
}

/// This provides `AsyncRead` for a remote file, so that we can use it with eg. tokio's `BufReader`
/// to parse config files and avoid downloading the entire thing at once.
pub struct EsxiFile {
    client: Arc<EsxiClient>,
    query: Arc<str>,
    size: AtomicU64,
    at: u64,
    state: ReadState,
}

impl EsxiFile {
    /// Get the file size. This is cached from the `HEAD` request made at `open_file` time, so if
    /// the file size changes in between, this is not updated.
    pub fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }

    /// Read an arbitrary range of data from the file.
    pub async fn read_at(&self, range: Range<u64>) -> Result<Bytes, Error> {
        let (body, size) = match self.client.download_do(&self.query, Some(range)).await {
            Ok(res) => res,
            Err(err) if err.is::<EofReached>() => return Ok(Bytes::new()),
            Err(err) => return Err(err),
        };
        if let Some(new_size) = size {
            self.size.store(new_size, Ordering::Release);
        }
        Ok(body)
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
                    let (data, new_size) = match ready!(Pin::new(fut).poll(cx)) {
                        Ok(Ok(result)) => result,
                        Ok(Err(err)) if err.is::<EofReached>() => {
                            this.state = ReadState::Eof;
                            return Poll::Ready(Ok(()));
                        }
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
                    };

                    if let Some(new_size) = new_size {
                        this.size.store(new_size, Ordering::Release);
                    };

                    this.at = this.at.saturating_add(data.len() as u64);
                    this.state = ReadState::Have { data, at: 0 };
                    continue;
                }
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

/// Multiple concurrent tasks can use this as a sort of semaphore for making http requests.
struct ConnectionLimit {
    /// Hyper might open multiple concurrent connections, let's limit this specifically to avoid
    /// esxi from getting bombarded with requests.
    requests: tokio::sync::Semaphore,

    /// When we enter a retry loop, only 1 task should actually actively do the retries.
    retry: tokio::sync::Mutex<()>,
}

impl ConnectionLimit {
    fn new() -> Self {
        Self {
            requests: tokio::sync::Semaphore::new(4),
            retry: tokio::sync::Mutex::new(()),
        }
    }

    /// This is supposed to cover an entire request including its retries.
    async fn acquire(&self) -> SemaphorePermit<'_> {
        // acquire can only fail when the semaphore is closed...
        self.requests
            .acquire()
            .await
            .expect("failed to acquire semaphore")
    }

    /// This ensures only 1 task keeps retrying making new requests.
    ///
    /// To enforce that the retry lock can only be held with a valid permit we require it to be
    /// passed through here.
    async fn retry_guard<'a>(
        &self,
        permit: SemaphorePermit<'a>,
    ) -> (tokio::sync::MutexGuard<()>, SemaphorePermit<'a>) {
        let guard = self.retry.lock().await;
        (guard, permit)
    }
}

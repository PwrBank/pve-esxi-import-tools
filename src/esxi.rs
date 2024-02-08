use std::fmt;
use std::ops::Range;

use anyhow::{bail, format_err, Context as _, Error};
use http::Request;
use hyper::Body;
use openssl::ssl::SslConnector;
use percent_encoding::{percent_encode, AsciiSet};

use proxmox_http::client::Client;

const QUERY_ESC: AsciiSet = percent_encoding::CONTROLS.add(b'?');

pub struct EsxiClient {
    client: Client,
    folder_url: String,
    auth_header: String,
}

impl EsxiClient {
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
    ) -> Result<hyper::body::Bytes, Error> {
        self.download(datacenter, datastore, path, None).await
    }

    /// Download a range from a file.
    pub async fn download_range(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
        range: Range<u64>,
    ) -> Result<hyper::body::Bytes, Error> {
        self.download(datacenter, datastore, path, Some(range))
            .await
    }

    /// Download a range from a file.
    pub async fn download(
        &self,
        datacenter: &str,
        datastore: &str,
        path: &str,
        range: Option<Range<u64>>,
    ) -> Result<hyper::body::Bytes, Error> {
        let datacenter = percent_encode(datacenter.as_bytes(), &percent_encoding::NON_ALPHANUMERIC);
        let datastore = percent_encode(datastore.as_bytes(), &percent_encoding::NON_ALPHANUMERIC);
        let path = percent_encode(path.as_bytes(), &QUERY_ESC);

        let mut req = Request::get(format!(
            "{}/{path}?dcName={datacenter}&dsName={datastore}",
            self.folder_url
        ))
        .header("authorization", &self.auth_header);

        if let Some(range) = range {
            req = req.header(
                "range",
                &format!("bytes={}-{}", range.start, range.end.saturating_sub(1)),
            )
        }

        let req = req
            .body(Body::empty())
            .context("failed to build http request")?;

        let (parts, body) = self
            .client
            .request(req)
            .await
            .context("http request failed")?
            .into_parts();

        if !parts.status.is_success() {
            bail!("http error code {:?}", parts.status);
        }

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
}

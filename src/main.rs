use std::ffi::OsString;
use std::path::Path;

use anyhow::{bail, format_err, Context as _, Error};
use futures::stream::StreamExt;
use openssl::ssl::{SslConnector, SslMethod};

use proxmox_fuse::Fuse;

mod esxi;
mod vmx;

use esxi::EsxiClient;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut args = std::env::args_os().skip(1);

    let path = args
        .next()
        .ok_or_else(|| format_err!("missing path parameter"))?;

    if args.next().is_some() {
        bail!("too many parameters");
    }

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    connector.set_verify(openssl::ssl::SslVerifyMode::NONE);
    let connector = connector.build();

    let reader = EsxiClient::new("https://10.9.2.70", "root", "asdf1234!", connector);

    let config = reader
        .download_file("ha-datacenter", "datastore1", "Test/Test.vmx")
        .await?;
    {
        use std::io::Write as _;
        std::io::stdout().write_all(&config)?;
    }

    // run_fuse(path).await?;

    Ok(())
}

async fn run_fuse(path: OsString) -> Result<(), Error> {
    let mut fuse = Fuse::builder("esxi-folder-fuse")
        .context("failed to create fuse session builder")?
        .enable_open()
        .enable_read()
        .enable_readdir()
        .build()
        .context("failed to create fuse session")?
        .mount(Path::new(&path))
        .with_context(|| format!("failed to mount fuse file system at {path:?}"))?;

    while let Some(request) = fuse.next().await {
        let request = request.context("error fetching next fuse request")?;
        tokio::spawn(handle_request(request));
    }

    Ok(())
}

async fn handle_request(request: proxmox_fuse::Request) {
    use proxmox_fuse::Request;

    todo!();
}

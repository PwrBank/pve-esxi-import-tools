use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, format_err, Context as _, Error};
use futures::stream::StreamExt;
use openssl::ssl::{SslConnector, SslMethod};

use proxmox_fuse::Fuse;

mod esxi;
mod fs;
mod vmx;

use esxi::EsxiClient;

struct Args {
    url: String,
    user: String,
    password: String,
    datacenter: String,
    datastore: String,
    config_file: String,
}

impl Args {
    fn from_vec(arg0: &OsStr, args: Vec<OsString>) -> Self {
        use std::io::Write as _;

        let err = match Self::from_vec_do(args) {
            Ok(this) => return this,
            Err(err) => err,
        };

        eprintln!("error: {err}");

        let _ = std::io::stderr().write_all(b"usage: ");
        let _ = std::io::stderr().write_all(arg0.as_bytes());
        eprintln!(" <baseurl> <user> <password> <datacenter> <datastore> <vm-config-file-path>");

        std::process::exit(1);
    }

    fn from_vec_do(args: Vec<OsString>) -> Result<Self, Error> {
        let mut args = args.into_iter();
        let mut next = || {
            let arg = args
                .next()
                .ok_or_else(|| format_err!("missing parameter"))?;
            arg.into_string()
                .map_err(|_| format_err!("non utf-8 parameter"))
        };

        let this = Self {
            url: next()?,
            user: next()?,
            password: next()?,
            datacenter: next()?,
            datastore: next()?,
            config_file: next()?,
        };

        if args.next().is_some() {
            bail!("too many parameters");
        }

        Ok(this)
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut args = std::env::args_os();
    let arg0 = args.next().unwrap();

    let args = args.collect::<Vec<_>>();
    let args = Args::from_vec(&arg0, args);

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    connector.set_verify(openssl::ssl::SslVerifyMode::NONE);
    let connector = connector.build();

    let reader = Arc::new(EsxiClient::new(
        &args.url,
        &args.user,
        &args.password,
        connector,
    ));

    let mut file = tokio::io::BufReader::new(
        reader
            .open_file(&args.datacenter, &args.datastore, &args.config_file)
            .await?,
    );
    loop {
        use tokio::io::AsyncBufReadExt;
        let mut s = String::new();
        file.read_line(&mut s).await?;
        if s.is_empty() {
            break;
        }
        print!("=> {s}");
    }

    let config = reader
        .download_file(&args.datacenter, &args.datastore, &args.config_file)
        .await?;

    let config = vmx::VmConfig::parse(&config)?;
    println!("{config:#?}");

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

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, format_err, Context as _, Error};
use futures::stream::StreamExt;
use openssl::ssl::{SslConnector, SslMethod};

use proxmox_fuse::Fuse;

mod cache;
mod esxi;
mod fs;
mod vmx;

use esxi::EsxiClient;
use fs::Inode;

struct Args {
    url: String,
    user: String,
    password: String,
    datacenter: String,
    datastore: String,
    config_file: String,
    mount_path: OsString,
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
        eprintln!(" <baseurl> <user> <password> <datacenter> <datastore> <vm-config-file-path> <mount-path>");

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
            mount_path: args
                .next()
                .ok_or_else(|| format_err!("missing mount path"))?,
        };

        if args.next().is_some() {
            bail!("too many parameters");
        }

        Ok(this)
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();

    let mut args = std::env::args_os();
    let arg0 = args.next().unwrap();

    let args = args.collect::<Vec<_>>();
    let args = Args::from_vec(&arg0, args);

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    connector.set_verify(openssl::ssl::SslVerifyMode::NONE);
    let connector = connector.build();

    let client = Arc::new(EsxiClient::new(
        &args.url,
        &args.user,
        &args.password,
        connector,
    ));

    let config = vmx::VmConfig::parse(
        client
            .open_file(&args.datacenter, &args.datastore, &args.config_file)
            .await?,
        &args.config_file,
    )
    .await?;

    println!("{config:#?}");

    let fs = fs::Fs::new(client);
    let datacenter = fs.create_datacenter(&args.datacenter);
    let datastore = datacenter.create_datastore(&args.datastore);

    for disk in config.disks.values() {
        if disk.starts_with('/') {
            log::info!("skipping absolute path - volume mapping required for {disk:?}");
            continue;
        }

        assert_file_exists(&datastore, disk).await?;
    }

    run_fuse(args.mount_path, fs).await?;

    Ok(())
}

async fn assert_file_exists(datastore: &Arc<fs::Dir>, path: &str) -> Result<(), Error> {
    log::info!("checking for path {path:?}");

    let mut at = Arc::clone(datastore);
    let mut iter = path.split('/').peekable();
    while let Some(component) = iter.next() {
        if iter.peek().is_none() {
            // this is a file!
            match at.lookup(component).await? {
                None => bail!("file not found on remote: {path:?}"),
                Some(Inode::File(_)) => {
                    log::info!("found file {path:?}");
                    break;
                }
                Some(_) => bail!("file expected, but found a directory at: {path:?}"),
            }
        }
        // this is a directory
        match at.lookup(component).await? {
            Some(Inode::Dir(dir)) => {
                at = dir;
            }
            _ => bail!("file not found on remote: {path:?}"),
        }
    }

    Ok(())
}

async fn run_fuse(path: OsString, fs: Arc<fs::Fs>) -> Result<(), Error> {
    let mut fuse = Fuse::builder("esxi-folder-fuse")
        .context("failed to create fuse session builder")?
        .enable_open()
        .enable_read()
        .enable_readdirplus()
        .build()
        .context("failed to create fuse session")?
        .mount(Path::new(&path))
        .with_context(|| format!("failed to mount fuse file system at {path:?}"))?;

    while let Some(request) = fuse.next().await {
        let request = request.context("error fetching next fuse request")?;
        let fs = Arc::clone(&fs);
        tokio::spawn(async move { fs.handle_request(request).await });
    }

    Ok(())
}

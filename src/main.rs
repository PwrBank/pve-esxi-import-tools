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
mod manifest;
mod vmx;

use esxi::EsxiClient;
use fs::Inode;

static mut FILE_CACHE_PAGE_SIZE: u64 = 8 << 20;
static mut FILE_CACHE_PAGE_COUNT: usize = 8;
static mut MANIFEST: Option<manifest::Manifest> = None;

pub fn file_cache_page_size() -> u64 {
    unsafe { FILE_CACHE_PAGE_SIZE }
}

pub fn file_cache_page_count() -> usize {
    unsafe { FILE_CACHE_PAGE_COUNT }
}

/// gets filled immediately after argument parsing and will be used throughout
pub fn manifest() -> &'static manifest::Manifest {
    unsafe { MANIFEST.as_ref().unwrap() }
}

struct Args {
    url: String,
    user: String,
    password: String,
    manifest: OsString,
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
        eprintln!(" <baseurl> <user> <password> <manifest-file> <mount-path>");

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
            manifest: args
                .next()
                .ok_or_else(|| format_err!("missing manifest path"))?,
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

fn parse_args() -> Result<Args, Error> {
    let arg0 = std::env::args_os().next().unwrap();

    let mut log_filter_level = None;

    let mut args = pico_args::Arguments::from_env();

    if let Some(value) = args.opt_value_from_str("--cache-page-size")? {
        unsafe {
            FILE_CACHE_PAGE_SIZE = value;
        }
    }
    if let Some(value) = args.opt_value_from_str("--cache-page-count")? {
        unsafe {
            FILE_CACHE_PAGE_COUNT = value;
        }
    }
    while args.contains("--debug") {
        log_filter_level = Some(log::LevelFilter::Debug);
    }
    if let Some(value) = args.opt_value_from_str("--log-level")? {
        log_filter_level = Some(value);
    }

    let mut env_logger = env_logger::builder();
    env_logger
        .filter_level(log::LevelFilter::Info)
        .parse_env("PROXMOX_ESXI_FUSE_LOG");
    if let Some(level) = log_filter_level {
        env_logger.filter_level(level);
    }
    { env_logger }.init();

    Ok(Args::from_vec(&arg0, args.finish()))
}

fn parse_manifest(manifest_path: &OsStr) -> Result<(), Error> {
    let data = std::fs::read(manifest_path).context("failed to read manifest")?;

    let manifest = serde_json::from_slice(&data).context("failed to parse manifest")?;
    unsafe {
        MANIFEST = Some(manifest);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = parse_args()?;
    parse_manifest(&args.manifest)?;

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    connector.set_verify(openssl::ssl::SslVerifyMode::NONE);
    let connector = connector.build();

    let client = Arc::new(EsxiClient::new(
        &args.url,
        &args.user,
        &args.password,
        connector,
    ));

    let fs = fs::Fs::new(Arc::clone(&client));

    for (datacenter, dc) in &manifest().datacenters {
        let fs_datacenter = fs.create_datacenter(datacenter);

        for config in dc.vm_configs.values() {
            let manifest::VmConfig { datastore, path } = config;
            let fs_datastore = fs_datacenter.create_datastore(datastore);

            println!("loading {datacenter:?}/{datastore:?}/{path:?}");
            let config =
                vmx::VmConfig::parse(client.open_file(datacenter, datastore, path).await?, path)
                    .await?;
            println!("{config:#?}");
            for disk in config.disks.values() {
                let other_fs_datastore;
                let (fs_datastore, datastore, path) = if disk.starts_with('/') {
                    if let Some((datastore, path)) = manifest().resolve_path(datacenter, disk) {
                        other_fs_datastore = fs_datacenter.create_datastore(datastore);
                        (&other_fs_datastore, datastore, path)
                    } else {
                        log::info!("ignoring {disk:?} - failed to resolve datastore");
                        continue;
                    }
                } else {
                    (&fs_datastore, datastore.as_str(), disk.as_str())
                };

                if check_file_exists(fs_datastore, path).await? {
                    log::info!(
                        "discovered {disk:?} found at {datacenter:?}/{datastore:?}/{path:?}"
                    );
                } else {
                    log::info!(
                        "ignoring {disk:?} - not found at {datacenter:?}/{datastore:?}/{path:?}"
                    );
                }
            }
        }
    }

    run_fuse(args.mount_path, fs).await?;

    Ok(())
}

async fn check_file_exists(datastore: &Arc<fs::Dir>, path: &str) -> Result<bool, Error> {
    let mut at = Arc::clone(datastore);
    let mut iter = path.split('/').peekable();
    while let Some(component) = iter.next() {
        if component.is_empty() {
            continue;
        }

        if iter.peek().is_none() {
            // this is a file!
            match at.lookup(component).await? {
                None => return Ok(false),
                Some(Inode::File(_)) => {
                    log::debug!("found file {path:?}");
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
            _ => return Ok(false),
        }
    }

    Ok(true)
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

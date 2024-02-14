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

static mut FILE_CACHE_PAGE_SIZE: u64 = 32 << 20;
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

fn usage<W: std::io::Write>(arg0: &OsStr, mut out: W, exit: i32) -> ! {
    let _ = out.write_all(b"usage: ");
    let _ = out.write_all(arg0.as_bytes());
    let _ = writeln!(
        out,
        "[options] <url> <manifest-file> <mount-path>\n\
        options:\n  \
          --cache-page-size=BYTES     size of a per-file cache entry\n  \
          --cache-page-count=COUNT    number of cache entries per file\n  \
          --user=USERNAME             user to login as\n  \
          --password=PASSWORD         the user's password\n  \
          --password-file=FILEM       read password from a file\n  \
          --password-fd=FDNUM         read password from a file descriptor\n  \
          --user-file=PATH            read both user name and password from a file\n  \
          -o MOUNT_OPTIONS            pass a mount option to fuse\n\
        "
    );

    std::process::exit(exit);
}

#[derive(Default)]
struct Args {
    // options:
    user: String,
    password: String,
    mount_options: Vec<OsString>,

    // positional:
    url: String,
    // user: String,
    // password: String,
    manifest: OsString,
    mount_path: OsString,
}

impl Args {
    fn from_vec(&mut self, args: Vec<OsString>) -> Result<(), Error> {
        let mut args = args.into_iter();
        let mut next = || {
            let arg = args
                .next()
                .ok_or_else(|| format_err!("missing parameter"))?;
            arg.into_string()
                .map_err(|_| format_err!("non utf-8 parameter"))
        };

        self.url = next()?;
        self.manifest = args
            .next()
            .ok_or_else(|| format_err!("missing manifest path"))?;
        self.mount_path = args
            .next()
            .ok_or_else(|| format_err!("missing mount path"))?;

        if args.next().is_some() {
            bail!("too many parameters");
        }

        Ok(())
    }
}

fn parse_args() -> Result<Args, Error> {
    let mut log_filter_level = None;

    let mut argparse = pico_args::Arguments::from_env();
    let mut args = Args::default();

    if let Some(value) = argparse.opt_value_from_os_str("-o", |os| Ok::<_, Error>(os.to_owned()))? {
        args.mount_options.push(value);
    }
    if let Some(value) = argparse.opt_value_from_str("--user")? {
        args.user = value;
    }
    if let Some(value) = argparse.opt_value_from_str("--password")? {
        args.password = value;
    }
    if let Some(value) =
        argparse.opt_value_from_os_str("--password-file", |os| Ok::<_, Error>(os.to_owned()))?
    {
        args.password = std::fs::read_to_string(value).context("failed to read file {value:?}")?;
        if args.password.ends_with('\n') {
            args.password.pop();
        }
    }
    if let Some(value) = argparse.opt_value_from_str("--password-fd")? {
        use std::io::Read as _;
        use std::os::fd::FromRawFd as _;

        let mut file = unsafe { std::fs::File::from_raw_fd(value) };
        args.password.clear();
        file.read_to_string(&mut args.password)
            .context("failed to read from password fd")?;
        if args.password.ends_with('\n') {
            args.password.pop();
        }
    }

    if let Some(value) = argparse.opt_value_from_str("--cache-page-size")? {
        unsafe {
            FILE_CACHE_PAGE_SIZE = value;
        }
    }

    if let Some(value) = argparse.opt_value_from_str("--cache-page-count")? {
        unsafe {
            FILE_CACHE_PAGE_COUNT = value;
        }
    }

    while argparse.contains("--debug") {
        log_filter_level = Some(log::LevelFilter::Debug);
    }

    if let Some(value) = argparse.opt_value_from_str("--log-level")? {
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

    args.from_vec(argparse.finish())?;

    Ok(args)
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
    let arg0 = std::env::args_os().next().unwrap();
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("error: {err}");
            usage(&arg0, std::io::stderr(), 1);
        }
    };
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

        for vm in dc.vms.values() {
            let manifest::VmConfig { datastore, path } = &vm.config;
            let fs_datastore = fs_datacenter.create_datastore(datastore);

            log::debug!("loading {datacenter:?}/{datastore:?}/{path:?}");
            let config =
                vmx::VmConfig::parse(client.open_file(datacenter, datastore, path).await?, path)
                    .await?;
            log::debug!("{config:#?}");
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

    run_fuse(args.mount_path, fs, args.mount_options).await?;

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

async fn run_fuse(
    path: OsString,
    fs: Arc<fs::Fs>,
    mount_options: Vec<OsString>,
) -> Result<(), Error> {
    let mut fuse = Fuse::builder("esxi-folder-fuse")
        .context("failed to create fuse session builder")?
        .enable_open()
        .enable_read()
        .enable_readdirplus();

    for opt in mount_options {
        fuse = fuse.options_os(&opt)?;
    }

    let mut fuse = fuse
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

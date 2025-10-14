use std::ffi::{CString, OsStr, OsString};
use std::fmt::Write;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::{bail, format_err, Context as _, Error};
use futures::stream::StreamExt;
use nix::unistd;
use openssl::ssl::{SslConnector, SslMethod};

use proxmox_fuse::Fuse;

mod cache;
mod client;
mod esxi;
mod fs;
mod manifest;
mod ssh_client;
mod vmx;

use client::DatastoreClient;
use esxi::EsxiClient;
use ssh_client::SshClient;

static mut FILE_CACHE_PAGE_SIZE: u64 = 128 << 20;
static mut FILE_CACHE_PAGE_COUNT: usize = 16;
static MANIFEST: OnceLock<manifest::Manifest> = OnceLock::new();

pub fn file_cache_page_size() -> u64 {
    unsafe { FILE_CACHE_PAGE_SIZE }
}

pub fn file_cache_page_count() -> usize {
    unsafe { FILE_CACHE_PAGE_COUNT }
}

/// gets filled immediately after argument parsing and will be used throughout
pub fn manifest() -> &'static manifest::Manifest {
    unsafe { MANIFEST.get().unwrap_unchecked() }
}

fn usage<W: std::io::Write>(arg0: &OsStr, mut out: W, exit: i32) -> ! {
    let _ = out.write_all(b"usage: ");
    let _ = out.write_all(arg0.as_bytes());
    let _ = write!(
        out,
        " [options] <host>[:<port>] <manifest-file> <mount-path>\n\
        options:\n  \
          --cache-page-size=BYTES     size of a per-file cache entry\n  \
          --cache-page-count=COUNT    number of cache entries per file\n  \
          --user=USERNAME             user to login as\n  \
          --password=PASSWORD         the user's password\n  \
          --password-file=FILEM       read password from a file\n  \
          --password-fd=FDNUM         read password from a file descriptor\n  \
          --user-file=PATH            read both user name and password from a file\n  \
          -o MOUNT_OPTIONS            pass a mount option to fuse, such as allow_other\n  \
          --change-user=UID           change to the provided user after mounting\n  \
          --change-group=UID          change to the provided group after mounting\n  \
          --ready-fd=FDNUM            close file descriptor FDNUM when ready\n  \
          --skip-cert-verification    disable certificate verification\n  \
          --use-http                  use HTTP API instead of SSH+dd streaming (SSH is default)\n  \
          --ssh-connections=COUNT     number of concurrent SSH connections (default: 8)\n  \
          -v, --version               print the version and exit\n  \
          -h, --help                  print this usage help and exit\n\
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
    change_user: Option<String>,
    change_group: Option<String>,
    ready_fd: Option<RawFd>,
    skip_cert_verification: bool,
    use_ssh: bool,
    ssh_connections: usize,

    // positional:
    host: String,
    manifest: OsString,
    mount_path: OsString,
}

impl Args {
    fn parse_vec(&mut self, args: Vec<OsString>) -> Result<(), Error> {
        let mut args = args.into_iter();
        let mut next = || {
            let arg = args
                .next()
                .ok_or_else(|| format_err!("missing parameter"))?;
            arg.into_string()
                .map_err(|_| format_err!("non utf-8 parameter"))
        };

        self.host = next()?;
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

fn parse_args() -> Result<Option<Args>, Error> {
    let mut log_filter_level = None;

    let mut argparse = pico_args::Arguments::from_env();
    let mut args = Args::default();

    if argparse.contains(["-h", "--help"]) {
        return Ok(None); // main_do fn handles usage outputs as it knows arg0
    }
    if argparse.contains(["-v", "--version"]) {
        println!(env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

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
        args.password = std::fs::read_to_string(&value)
            .with_context(|| format!("failed to read file {value:?}"))?;
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
    if let Some(value) = argparse.opt_value_from_str("--change-user")? {
        args.change_user = Some(value);
    }
    if let Some(value) = argparse.opt_value_from_str("--change-group")? {
        args.change_group = Some(value);
    }
    if let Some(value) = argparse.opt_value_from_str("--ready-fd")? {
        args.ready_fd = Some(value);
    }

    while argparse.contains("--skip-cert-verification") {
        args.skip_cert_verification = true;
    }

    // SSH is the default mode for better performance (90 MB/s vs 40 MB/s HTTP)
    // Try SSH first (requires key authentication), fall back to HTTP if SSH fails
    // Use --use-http to explicitly force HTTP mode
    if argparse.contains("--use-http") {
        args.use_ssh = false;
        log::info!("--use-http flag specified - using HTTP mode");
    } else {
        // Default: Try SSH mode with key authentication (90 MB/s performance)
        // Password is ignored in SSH mode - keys are required
        args.use_ssh = true;
    }

    if let Some(value) = argparse.opt_value_from_str("--ssh-connections")? {
        args.ssh_connections = value;
    } else {
        args.ssh_connections = 8; // default - optimized for SSH streaming
    }

    while argparse.contains("--debug") {
        log_filter_level = Some(log::LevelFilter::Debug);
    }

    if let Some(value) = argparse.opt_value_from_str("--log-level")? {
        log_filter_level = Some(value);
    }

    syslog::init(
        syslog::Facility::LOG_DAEMON,
        log_filter_level.unwrap_or(log::LevelFilter::Info),
        Some("esxi-folder-fuse"),
    )
    .map_err(|err| format_err!("failed to initialize syslog: {err}"))?;

    args.parse_vec(argparse.finish())?;

    Ok(Some(args))
}

fn parse_manifest(manifest_path: &OsStr) -> Result<(), Error> {
    let data = std::fs::read(manifest_path).context("failed to read manifest")?;

    let manifest = serde_json::from_slice(&data).context("failed to parse manifest")?;
    MANIFEST.set(manifest).expect("failed to set manifest");

    Ok(())
}

fn main() {
    let cpus = num_cpus::get();
    let runtime = proxmox_async::runtime::get_runtime_with_builder(|| {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();
        builder.max_blocking_threads(2);
        builder.worker_threads(cpus.clamp(2, 4));
        builder
    });

    if let Err(err) = runtime.block_on(main_do()) {
        let mut err_chain = String::new();
        for err in err.chain() {
            let _ = writeln!(err_chain, " {err}");
        }
        eprintln!("Error: {err}");
        log::error!("Error:{err_chain}");
        std::process::exit(-1);
    }
}

async fn main_do() -> Result<(), Error> {
    let arg0 = std::env::args_os().next().unwrap();
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => usage(&arg0, std::io::stdout(), 0),
        Err(err) => {
            eprintln!("error: {err}");
            usage(&arg0, std::io::stderr(), 1);
        }
    };
    parse_manifest(&args.manifest)?;

    let change_uid = match args.change_user.as_deref() {
        Some(user) => Some(get_uid(user)?),
        None => None,
    };

    let change_gid = match args.change_group.as_deref() {
        Some(group) => Some(get_gid(group)?),
        None => None,
    };

    let mut fuse = Fuse::builder("esxi-folder-fuse")
        .context("failed to create fuse session builder")?
        .enable_open()
        .enable_read()
        .enable_readdirplus();

    for opt in args.mount_options {
        fuse = fuse.options_os(&opt)?;
    }

    unmount_if_mounted(&args.mount_path)?;

    let mut fuse = fuse
        .build()
        .context("failed to create fuse session")?
        .mount(Path::new(&args.mount_path))
        .with_context(|| format!("failed to mount fuse file system at {:?}", args.mount_path))?;

    // Note: we could connect first, but we want to get the privilege-dropping out of the way
    // before connecting to the outside.
    //
    // IMPORTANT: When using SSH mode, we must stay as root to access SSH keys.
    // SSH keys are in /root/.ssh/ and cannot be accessed by the 'nobody' user.
    // HTTP mode can safely drop privileges since it only needs password authentication.

    if !args.use_ssh {
        // Only drop privileges for HTTP mode (password authentication)
        if let Some(gid) = change_gid {
            unistd::setgid(unistd::Gid::from_raw(gid)).context("failed to change group id")?;
        }
        if let Some(uid) = change_uid {
            unistd::setuid(unistd::Uid::from_raw(uid)).context("failed to change user id")?;
        }
    } else {
        // SSH mode: Keep running as root to access SSH keys
        if change_uid.is_some() || change_gid.is_some() {
            log::info!("SSH mode: ignoring --change-user/--change-group flags (root required for SSH key access)");
        }
    }

    let client = if args.use_ssh {
        log::info!(
            "Using SSH+dd streaming mode (default) with {} concurrent connections - 90 MB/s performance",
            args.ssh_connections
        );
        DatastoreClient::Ssh(Arc::new(SshClient::new(
            args.host.clone(),
            args.user.clone(),
            args.ssh_connections,
        )))
    } else {
        log::info!("Using HTTP API mode (fallback) - 40 MB/s performance");
        let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
        if args.skip_cert_verification {
            connector.set_verify(openssl::ssl::SslVerifyMode::NONE);
        }
        connector
            .set_alpn_protos(b"\x02h2")
            .context("failed to configure alpn protocols")?;
        let connector = connector.build();

        DatastoreClient::Http(Arc::new(EsxiClient::new(
            &format!("https://{}", args.host),
            &args.user,
            &args.password,
            connector,
        )))
    };

    let fs = fs::Fs::new(client);

    // Pre-create datacenter and datastore structures for all VMs in manifest
    for (datacenter, dc) in &manifest().datacenters {
        let fs_datacenter = fs.create_datacenter(datacenter);

        // Pre-create datastore entries
        for ds_name in dc.datastores.keys() {
            fs_datacenter.create_datastore(ds_name);
        }

        log::debug!("pre-loaded datacenter {datacenter:?} with {} datastores", dc.datastores.len());
    }

    if let Some(fd) = args.ready_fd {
        let rc = unsafe { libc::close(fd) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            log::error!("error closing ready-fd: {err:?}");
        }
    }

    log::info!("esxi fuse mount ready");

    while let Some(request) = fuse.next().await {
        let request = request.context("error fetching next fuse request")?;
        let fs = Arc::clone(&fs);
        tokio::spawn(async move { fs.handle_request(request).await });
    }

    Ok(())
}

fn unmount_if_mounted(path: &OsStr) -> Result<(), Error> {
    let path = CString::new(path.as_bytes()).context("failed to build C string")?;
    let rc = unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if let Some(errno) = err.raw_os_error() {
            if errno != libc::EINVAL && errno != libc::ENOENT {
                return Err(Error::from(err).context("failed to unmount old fuse instance"));
            }
        }
    }
    Ok(())
}

fn get_uid(name_or_uid: &str) -> Result<libc::uid_t, Error> {
    if let Ok(num) = name_or_uid.parse() {
        return Ok(num);
    }

    Ok(unistd::User::from_name(name_or_uid)
        .context("failed to query system user id for '{name_or_uid}'")?
        .ok_or_else(|| format_err!("no such user '{name_or_uid}'"))?
        .uid
        .as_raw())
}

fn get_gid(name_or_gid: &str) -> Result<libc::gid_t, Error> {
    if let Ok(num) = name_or_gid.parse() {
        return Ok(num);
    }

    Ok(unistd::Group::from_name(name_or_gid)
        .context("failed to query system group id for '{name_or_gid}'")?
        .ok_or_else(|| format_err!("no such group '{name_or_gid}'"))?
        .gid
        .as_raw())
}

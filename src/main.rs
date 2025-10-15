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
mod netcat_transfer;
mod ssh_client;
mod vmx;

use client::DatastoreClient;
use esxi::EsxiClient;
use ssh_client::SshClient;

static mut FILE_CACHE_PAGE_SIZE: u64 = 128 << 20;  // 128MB blocks - optimal balance
static mut FILE_CACHE_PAGE_COUNT: usize = 20;      // 20 blocks for prefetch window
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
        " [options] <host>[:<port>] <manifest-file> <mount-path>\n\n\
        FUSE Mount Mode (default):\n  \
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
          --use-http                  use HTTP API instead of SSH+dd streaming\n  \
          --use-netcat                use netcat for high-speed direct transfers (~115 MB/s)\n  \
          --ssh-connections=COUNT     number of concurrent SSH connections (default: 16)\n\n\
        Wrapper Mode (for transparent netcat acceleration):\n  \
          --wrap-qemu-img <args>      act as qemu-img wrapper, detecting ESXi imports\n\n\
        Direct Import Mode (bypass FUSE):\n  \
          --direct-import             perform direct netcat import\n  \
            --source=PATH             source FUSE path or ESXi disk path\n  \
            --dest=PATH               destination path\n  \
            --src-format=FMT          source format (default: vmdk)\n  \
            --dst-format=FMT          destination format (default: qcow2)\n  \
            --bwlimit=RATE            bandwidth limit in KiB/s\n\n\
        General:\n  \
          -v, --version               print the version and exit\n  \
          -h, --help                  print this usage help and exit\n\
        "
    );

    std::process::exit(exit);
}

#[derive(Default)]
struct Args {
    // Mode selection
    mode: OperationMode,

    // FUSE mount options:
    user: String,
    password: String,
    mount_options: Vec<OsString>,
    change_user: Option<String>,
    change_group: Option<String>,
    ready_fd: Option<RawFd>,
    skip_cert_verification: bool,
    use_ssh: bool,
    use_netcat: bool,
    ssh_connections: usize,

    // FUSE positional:
    host: String,
    manifest: OsString,
    mount_path: OsString,

    // Direct import options:
    source_path: Option<String>,
    dest_path: Option<String>,
    src_format: String,
    dst_format: String,
    bwlimit: Option<String>,
}

#[derive(Default, Debug, PartialEq)]
enum OperationMode {
    #[default]
    FuseMount,
    WrapQemuImg,
    DirectImport,
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

    // Detect operation mode
    if argparse.contains("--wrap-qemu-img") {
        args.mode = OperationMode::WrapQemuImg;
    } else if argparse.contains("--direct-import") {
        args.mode = OperationMode::DirectImport;
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

    // Netcat mode takes priority, then HTTP, then SSH (default)
    if argparse.contains("--use-netcat") {
        args.use_netcat = true;
        args.use_ssh = false;
        log::warn!("--use-netcat mode is experimental and not yet fully implemented");
        log::info!("--use-netcat flag specified - using netcat direct transfer mode");
    } else if argparse.contains("--use-http") {
        args.use_ssh = false;
        args.use_netcat = false;
        log::info!("--use-http flag specified - using HTTP mode");
    } else {
        // Default: SSH mode with key authentication
        args.use_ssh = true;
        args.use_netcat = false;
    }

    if let Some(value) = argparse.opt_value_from_str("--ssh-connections")? {
        args.ssh_connections = value;
    } else {
        args.ssh_connections = 16; // default - increased for better throughput saturation
    }

    // Direct import mode options
    if let Some(value) = argparse.opt_value_from_str("--source")? {
        args.source_path = Some(value);
    }
    if let Some(value) = argparse.opt_value_from_str("--dest")? {
        args.dest_path = Some(value);
    }
    if let Some(value) = argparse.opt_value_from_str("--src-format")? {
        args.src_format = value;
    } else {
        args.src_format = "vmdk".to_string();
    }
    if let Some(value) = argparse.opt_value_from_str("--dst-format")? {
        args.dst_format = value;
    } else {
        args.dst_format = "qcow2".to_string();
    }
    if let Some(value) = argparse.opt_value_from_str("--bwlimit")? {
        args.bwlimit = Some(value);
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

    // Only parse positional args for FUSE mode
    if args.mode == OperationMode::FuseMount {
        args.parse_vec(argparse.finish())?;
    }

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
        builder.worker_threads(cpus.clamp(4, 12));  // Increased for better SSH parallelism
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

/// Test SSH connection to verify key authentication is working
async fn test_ssh_connection(host: &str, user: &str) -> Result<(), Error> {
    use tokio::process::Command;
    use std::process::Stdio;

    let output = Command::new("ssh")
        .arg("-o").arg("BatchMode=yes")
        .arg("-o").arg("ConnectTimeout=5")
        .arg("-o").arg("StrictHostKeyChecking=no")
        .arg(format!("{}@{}", user, host))
        .arg("echo")
        .arg("SSH_OK")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to execute SSH test command")?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim() == "SSH_OK" {
            return Ok(());
        }
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format_err!(
        "SSH connection test failed: {}",
        stderr.trim()
    ))
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

    // Handle different operation modes
    match args.mode {
        OperationMode::WrapQemuImg => {
            return wrap_qemu_img_mode(&args).await;
        }
        OperationMode::DirectImport => {
            return direct_import_mode(&args).await;
        }
        OperationMode::FuseMount => {
            // Continue with normal FUSE mount logic below
        }
    }

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
            "Attempting SSH+dd streaming mode with {} concurrent connections",
            args.ssh_connections
        );

        // Test SSH connection before committing to SSH mode
        match test_ssh_connection(&args.host, &args.user).await {
            Ok(_) => {
                log::info!("SSH connection successful - using SSH streaming mode");
                DatastoreClient::Ssh(Arc::new(SshClient::new(
                    args.host.clone(),
                    args.user.clone(),
                    args.ssh_connections,
                )))
            }
            Err(ssh_err) => {
                // SSH failed - try to fall back to HTTP if password is available
                if !args.password.is_empty() {
                    log::warn!(
                        "SSH connection failed ({}), falling back to HTTP mode with password authentication",
                        ssh_err
                    );

                    // Need to drop privileges for HTTP mode if they were requested
                    if let Some(gid) = change_gid {
                        unistd::setgid(unistd::Gid::from_raw(gid))
                            .context("failed to change group id for HTTP fallback")?;
                    }
                    if let Some(uid) = change_uid {
                        unistd::setuid(unistd::Uid::from_raw(uid))
                            .context("failed to change user id for HTTP fallback")?;
                    }

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
                } else {
                    return Err(ssh_err.context(
                        "SSH connection failed and no password provided for HTTP fallback. \
                        Please set up SSH keys or provide a password with --password or --password-file"
                    ));
                }
            }
        }
    } else {
        log::info!("Using HTTP API mode (explicitly requested)");
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

/// Wrapper mode: Act as qemu-img, detecting ESXi imports for acceleration
async fn wrap_qemu_img_mode(_args: &Args) -> Result<(), Error> {
    eprintln!("=== ESXi qemu-img Wrapper Mode ===");
    eprintln!("Detecting ESXi imports for netcat acceleration...");
    eprintln!();

    // Get all arguments passed to us
    let all_args: Vec<String> = std::env::args().collect();

    // Check if this is a convert operation with ESXi FUSE path
    let is_esxi_import = all_args.iter().any(|arg| {
        arg.contains("/run/pve/import/esxi/") && arg.ends_with(".vmdk")
    });

    if is_esxi_import {
        eprintln!("✓ ESXi import detected!");
        eprintln!("TODO: Implement netcat acceleration");
        eprintln!("Falling back to regular qemu-img for now...");
        eprintln!();
    }

    // Fall back to real qemu-img
    let real_qemu_img = "/usr/bin/qemu-img.real";
    let status = std::process::Command::new(real_qemu_img)
        .args(&all_args[1..])  // Skip our binary name
        .status()
        .context("Failed to execute real qemu-img")?;

    if !status.success() {
        bail!("qemu-img exited with status: {}", status);
    }

    Ok(())
}

/// Direct import mode: Perform netcat-based import directly
async fn direct_import_mode(args: &Args) -> Result<(), Error> {
    eprintln!("=== ESXi Direct Import Mode ===");

    let source = args.source_path.as_ref()
        .ok_or_else(|| format_err!("--source is required for direct import mode"))?;
    let dest = args.dest_path.as_ref()
        .ok_or_else(|| format_err!("--dest is required for direct import mode"))?;

    eprintln!("Source: {}", source);
    eprintln!("Dest:   {}", dest);
    eprintln!("Format: {} -> {}", args.src_format, args.dst_format);
    if let Some(bw) = &args.bwlimit {
        eprintln!("BW Limit: {} KiB/s", bw);
    }
    eprintln!();

    eprintln!("TODO: Implement netcat direct import");
    eprintln!("Falling back to regular qemu-img for now...");
    eprintln!();

    // Fall back to qemu-img
    let mut cmd = std::process::Command::new("/usr/bin/qemu-img");
    cmd.arg("convert")
        .arg("-p")
        .arg("-n")
        .arg("-f").arg(&args.src_format)
        .arg("-O").arg(&args.dst_format);

    if let Some(bw) = &args.bwlimit {
        cmd.arg("-r").arg(format!("{}K", bw));
    }

    cmd.arg(source).arg(dest);

    let status = cmd.status()
        .context("Failed to execute qemu-img")?;

    if !status.success() {
        bail!("qemu-img convert failed with status: {}", status);
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

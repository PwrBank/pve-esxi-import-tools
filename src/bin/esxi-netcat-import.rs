//! ESXi Netcat Import Tool
//!
//! High-speed VM disk import using netcat streaming.
//! This tool is called by the qemu-img wrapper to perform direct
//! netcat transfers from ESXi to Proxmox, bypassing slow FUSE reads.
//!
//! Flow:
//! 1. Parse FUSE mount path to extract ESXi host and disk path
//! 2. Setup netcat listener on Proxmox
//! 3. SSH to ESXi: dd | pigz | nc proxmox PORT
//! 4. Stream from netcat: nc | pigz -d | qemu-img convert -f vmdk - output
//!
//! Performance: ~115 MB/s vs 76-88 MB/s with FUSE

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::io::{self, Write};

use anyhow::{bail, Context, Result};

#[derive(Debug)]
struct ImportConfig {
    /// Source path from FUSE mount (e.g., /run/pve/import/esxi/storage1/mnt/dc/ds/vm/disk.vmdk)
    source_fuse_path: PathBuf,

    /// Destination path for output
    dest_path: PathBuf,

    /// Source format (e.g., "vmdk")
    src_format: String,

    /// Destination format (e.g., "qcow2")
    dst_format: String,

    /// Bandwidth limit in KiB/s (optional)
    bwlimit: Option<String>,

    /// ESXi storage ID extracted from FUSE path
    storage_id: String,

    /// ESXi host extracted from manifest
    esxi_host: String,

    /// ESXi user
    esxi_user: String,

    /// Actual ESXi disk path
    esxi_disk_path: String,
}

impl ImportConfig {
    fn from_args() -> Result<Self> {
        let args: Vec<String> = env::args().collect();

        let mut source_fuse_path = None;
        let mut dest_path = None;
        let mut src_format = "vmdk".to_string();
        let mut dst_format = "qcow2".to_string();
        let mut bwlimit = None;

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--source" => {
                    source_fuse_path = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                }
                "--dest" => {
                    dest_path = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                }
                "--src-format" => {
                    src_format = args[i + 1].clone();
                    i += 2;
                }
                "--dst-format" => {
                    dst_format = args[i + 1].clone();
                    i += 2;
                }
                "--bwlimit" => {
                    bwlimit = Some(args[i + 1].clone());
                    i += 2;
                }
                _ => {
                    bail!("Unknown argument: {}", args[i]);
                }
            }
        }

        let source_fuse_path = source_fuse_path
            .ok_or_else(|| anyhow::anyhow!("--source is required"))?;
        let dest_path = dest_path
            .ok_or_else(|| anyhow::anyhow!("--dest is required"))?;

        // Extract storage ID from FUSE path
        // Path format: /run/pve/import/esxi/{storage_id}/mnt/...
        let storage_id = Self::extract_storage_id(&source_fuse_path)?;

        // Read manifest to get ESXi host and resolve real disk path
        let (esxi_host, esxi_user, esxi_disk_path) =
            Self::resolve_esxi_path(&storage_id, &source_fuse_path)?;

        Ok(Self {
            source_fuse_path,
            dest_path,
            src_format,
            dst_format,
            bwlimit,
            storage_id,
            esxi_host,
            esxi_user,
            esxi_disk_path,
        })
    }

    fn extract_storage_id(path: &Path) -> Result<String> {
        let path_str = path.to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid UTF-8 in path"))?;

        // Expected format: /run/pve/import/esxi/{storage_id}/mnt/...
        let prefix = "/run/pve/import/esxi/";
        if !path_str.starts_with(prefix) {
            bail!("Path does not start with {}", prefix);
        }

        let rest = &path_str[prefix.len()..];
        let storage_id = rest.split('/').next()
            .ok_or_else(|| anyhow::anyhow!("Could not extract storage ID"))?;

        Ok(storage_id.to_string())
    }

    fn resolve_esxi_path(storage_id: &str, fuse_path: &Path) -> Result<(String, String, String)> {
        // Read the manifest file
        let manifest_path = format!("/run/pve/import/esxi/{}/manifest.json", storage_id);
        let manifest_data = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("Failed to read manifest: {}", manifest_path))?;

        // Parse manifest to get ESXi host
        // For now, we'll need to get this info from the storage config
        // Let's read from /etc/pve/storage.cfg or the manifest structure

        // TODO: Properly parse manifest and storage config
        // For now, return placeholder - this needs to be implemented
        let esxi_host = "10.10.5.67".to_string(); // TEMP
        let esxi_user = "root".to_string();

        // Convert FUSE path to real ESXi path
        // FUSE: /run/pve/import/esxi/storage1/mnt/datacenter/datastore/vm/disk.vmdk
        // ESXi: /vmfs/volumes/{uuid}/vm/disk.vmdk

        let fuse_str = fuse_path.to_str().unwrap();
        let mnt_prefix = format!("/run/pve/import/esxi/{}/mnt/", storage_id);

        if let Some(relative) = fuse_str.strip_prefix(&mnt_prefix) {
            // Skip datacenter/datastore parts and get the actual path
            let parts: Vec<&str> = relative.split('/').collect();
            if parts.len() >= 3 {
                // Format: datacenter/datastore/vm/disk.vmdk
                // We need to convert datastore to actual /vmfs/volumes/ path
                // This requires reading the manifest
                let vm_path = parts[2..].join("/");
                let esxi_disk_path = format!("/vmfs/volumes/TODO/{}", vm_path);

                return Ok((esxi_host, esxi_user, esxi_disk_path));
            }
        }

        bail!("Could not resolve ESXi path from FUSE path");
    }
}

fn main() -> Result<()> {
    env_logger::init();

    eprintln!("=== ESXi Netcat Import Tool ===");
    eprintln!("Note: This is a work in progress");
    eprintln!();

    let config = ImportConfig::from_args()
        .context("Failed to parse arguments")?;

    eprintln!("Source (FUSE):    {}", config.source_fuse_path.display());
    eprintln!("Destination:      {}", config.dest_path.display());
    eprintln!("ESXi Host:        {}", config.esxi_host);
    eprintln!("ESXi Disk Path:   {}", config.esxi_disk_path);
    eprintln!("Source Format:    {}", config.src_format);
    eprintln!("Dest Format:      {}", config.dst_format);
    eprintln!();

    // TODO: Implement the actual netcat transfer
    // For now, just fall back to regular copy
    eprintln!("FALLBACK: Using regular qemu-img (netcat not yet implemented)");

    let mut cmd = Command::new("/usr/bin/qemu-img.real");
    cmd.arg("convert")
        .arg("-p")
        .arg("-n")
        .arg("-f").arg(&config.src_format)
        .arg("-O").arg(&config.dst_format);

    if let Some(bwlimit) = &config.bwlimit {
        cmd.arg("-r").arg(bwlimit);
    }

    cmd.arg(&config.source_fuse_path)
        .arg(&config.dest_path);

    let status = cmd.status()
        .context("Failed to execute qemu-img")?;

    if !status.success() {
        bail!("qemu-img convert failed with status: {}", status);
    }

    Ok(())
}

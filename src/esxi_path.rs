//! ESXi path resolution and manifest parsing
//!
//! Handles parsing of FUSE mount paths to extract ESXi connection information
//! and resolve real disk paths from manifests.

use std::path::Path;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

/// Information extracted from FUSE path and manifest
#[derive(Debug, Clone)]
pub struct EsxiPathInfo {
    /// ESXi host (IP or hostname)
    pub host: String,

    /// ESXi user
    pub user: String,

    /// Real ESXi disk path (e.g., /vmfs/volumes/uuid/vm/disk.vmdk)
    pub disk_path: String,

    /// Storage ID from Proxmox
    pub storage_id: String,

    /// File size if available
    pub size: Option<u64>,
}

/// Simple manifest structure for parsing
#[derive(Debug, Deserialize)]
struct SimpleManifest {
    #[serde(flatten)]
    datacenters: HashMap<String, Datacenter>,
}

#[derive(Debug, Deserialize)]
struct Datacenter {
    datastores: HashMap<String, String>,
}

impl EsxiPathInfo {
    /// Parse a FUSE mount path and extract ESXi information
    ///
    /// Example path: /run/pve/import/esxi/storage1/mnt/datacenter/datastore/vm/disk.vmdk
    pub fn from_fuse_path(fuse_path: &str) -> Result<Self> {
        // Extract storage ID
        let storage_id = Self::extract_storage_id(fuse_path)?;

        // Read manifest
        let manifest_path = format!("/run/pve/import/esxi/{}/manifest.json", storage_id);
        let manifest_data = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("Failed to read manifest: {}", manifest_path))?;

        let manifest: SimpleManifest = serde_json::from_str(&manifest_data)
            .context("Failed to parse manifest")?;

        // Read storage config to get host and user
        let (host, user) = Self::read_storage_config(&storage_id)?;

        // Convert FUSE path to real ESXi path
        let disk_path = Self::resolve_disk_path(fuse_path, &storage_id, &manifest)?;

        // Try to get file size from FUSE stat
        let size = std::fs::metadata(fuse_path)
            .ok()
            .map(|m| m.len());

        Ok(Self {
            host,
            user,
            disk_path,
            storage_id,
            size,
        })
    }

    fn extract_storage_id(path: &str) -> Result<String> {
        // Path format: /run/pve/import/esxi/{storage_id}/mnt/...
        let prefix = "/run/pve/import/esxi/";
        if !path.starts_with(prefix) {
            bail!("Path does not start with {}", prefix);
        }

        let rest = &path[prefix.len()..];
        let storage_id = rest.split('/').next()
            .ok_or_else(|| anyhow::anyhow!("Could not extract storage ID"))?;

        Ok(storage_id.to_string())
    }

    fn read_storage_config(storage_id: &str) -> Result<(String, String)> {
        // Read /etc/pve/storage.cfg to get the ESXi host and user
        let storage_cfg = std::fs::read_to_string("/etc/pve/storage.cfg")
            .context("Failed to read /etc/pve/storage.cfg")?;

        let mut in_section = false;
        let mut host = None;
        let mut user = None;

        for line in storage_cfg.lines() {
            let line = line.trim();

            // Check if we're entering our storage section
            if line.starts_with("esxi:") && line.contains(storage_id) {
                in_section = true;
                continue;
            }

            // If we hit another section, stop
            if in_section && line.contains(':') && !line.starts_with('\t') {
                break;
            }

            if in_section {
                if let Some(value) = line.strip_prefix("server ") {
                    host = Some(value.trim().to_string());
                } else if let Some(value) = line.strip_prefix("username ") {
                    user = Some(value.trim().to_string());
                }
            }
        }

        let host = host.ok_or_else(|| anyhow::anyhow!("Could not find server in storage config"))?;
        let user = user.unwrap_or_else(|| "root".to_string());

        Ok((host, user))
    }

    fn resolve_disk_path(
        fuse_path: &str,
        storage_id: &str,
        manifest: &SimpleManifest,
    ) -> Result<String> {
        // FUSE path: /run/pve/import/esxi/{storage}/mnt/{datacenter}/{datastore}/{vm_path}
        let mnt_prefix = format!("/run/pve/import/esxi/{}/mnt/", storage_id);

        let relative = fuse_path.strip_prefix(&mnt_prefix)
            .ok_or_else(|| anyhow::anyhow!("Invalid FUSE path format"))?;

        // Split into: datacenter / datastore / vm_path
        let parts: Vec<&str> = relative.split('/').collect();
        if parts.len() < 3 {
            bail!("FUSE path too short: {}", fuse_path);
        }

        let datacenter = parts[0];
        let datastore = parts[1];
        let vm_path = parts[2..].join("/");

        // Get datastore UUID from manifest
        let dc = manifest.datacenters.get(datacenter)
            .ok_or_else(|| anyhow::anyhow!("Datacenter {} not found in manifest", datacenter))?;

        let datastore_path = dc.datastores.get(datastore)
            .ok_or_else(|| anyhow::anyhow!("Datastore {} not found in manifest", datastore))?;

        // Construct real ESXi path
        let disk_path = format!("{}{}", datastore_path, vm_path);

        Ok(disk_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_storage_id() {
        let path = "/run/pve/import/esxi/storage1/mnt/dc/ds/vm/disk.vmdk";
        let id = EsxiPathInfo::extract_storage_id(path).unwrap();
        assert_eq!(id, "storage1");
    }
}

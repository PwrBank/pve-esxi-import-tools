//! We use a pythono tool with the pyvmomi api client to find the VMs and datastore names/ids.
//! This produces a json file we use as a manifest to figure out which VMs to provide access to,
//! and how to find their disks.

use std::collections::HashMap;

use serde::Deserialize;

/// At the root of the manifest there are datacenter entries.
///
/// Example:
///
/// ```text
/// {
///   "dacv": {
///     "datastores": {
///       "datastore1": "/vmfs/volumes/65c1f000-fd51f79c-82ae-02ffff771b50/",
///       "fastds": "/vmfs/volumes/65ca32f8-8970e68a-3b43-02ffff771b50/"
///     },
///     "vm-configs": {
///       "Test": {
///         "datastore": "datastore1",
///         "path": "Test/Test.vmx"
///       },
///       "VMware vCenter Server": {
///         "datastore": "datastore1",
///         "path": "VMware vCenter Server/VMware vCenter Server.vmx"
///       },
///       "t2": {
///         "datastore": "datastore1",
///         "path": "t2/t2.vmx"
///       }
///     }
///   }
/// }
/// ```
#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// Datacenter by name.
    #[serde(flatten)]
    pub datacenters: HashMap<String, Datacenter>,
}

/// See the [`Manifest`] for an example.
#[derive(Debug, Deserialize)]
pub struct Datacenter {
    /// Datastores simply map to their paths.
    pub datastores: HashMap<String, String>,

    /// The VM list.
    pub vms: HashMap<String, Vm>,
}

/// Currently only stores the info about the configuration file.
#[derive(Debug, Deserialize)]
pub struct Vm {
    /// VMs just reference their config file via datastore and path.
    pub config: VmConfig,
}

/// Datastore and path of the actual file.
#[derive(Debug, Deserialize)]
pub struct VmConfig {
    pub datastore: String,
    pub path: String,
    // currently not using the checksum
}

impl Manifest {
    /// Try to resolve a `/vmfs/volumes/<uuid>` path into a datastore+path tuple.
    pub fn resolve_path<'a, 'p>(
        &'a self,
        datacenter: &str,
        path: &'p str,
    ) -> Option<(&'a str, &'p str)> {
        self.datacenters.get(datacenter)?.resolve_path(path)
    }
}

impl Datacenter {
    /// Try to resolve a `/vmfs/volumes/<uuid>` path into a datastore+path tuple.
    pub fn resolve_path<'a, 'p>(&'a self, path: &'p str) -> Option<(&'a str, &'p str)> {
        for (datastore, ds_path) in &self.datastores {
            if let Some(path) = path.strip_prefix(ds_path) {
                return Some((datastore, path));
            }
        }
        None
    }
}

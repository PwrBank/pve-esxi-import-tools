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

#[derive(Debug, Deserialize)]
pub struct Datacenter {
    /// Datastores simply map to their paths.
    pub datastores: HashMap<String, String>,

    /// VMs just reference their config file via datastore and path.
    #[serde(rename = "vm-configs")]
    pub vm_configs: HashMap<String, VmConfig>,
}

#[derive(Debug, Deserialize)]
pub struct VmConfig {
    pub datastore: String,
    pub path: String,
}

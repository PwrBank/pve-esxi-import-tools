use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Error;

use proxmox_fuse::requests::{self, FuseRequest};
use proxmox_fuse::{Request, ROOT_ID};

use crate::esxi::{EsxiClient, EsxiFile};
use crate::vmx::VmConfig;

struct RemotePath {
    datacenter: String,
    datastore: String,
    path: String,
}

struct Entry {
    inode: u64,
    remote_path: RemotePath,
    handle: Mutex<Option<Arc<EsxiFile>>>,
}

struct Fs {
    client: Arc<EsxiClient>,
    config: VmConfig,

    /// map inodes to open file handles
    inodes: Mutex<BTreeMap<u64, Entry>>,

    /// map file names to inodes
    files: Mutex<HashMap<String, u64>>,

    /// used to generate inodes
    current_inode: AtomicU64,
}

impl Fs {
    pub fn new(
        client: Arc<EsxiClient>,
        config: VmConfig,
        datacenter: String,
        datastore: String,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            client,
            config,
            inodes: Mutex::new(BTreeMap::new()),
            files: Mutex::new(HashMap::new()),
            current_inode: AtomicU64::new(ROOT_ID + 1),
        });

        for full_path in this.config.disks.values() {
            let file = match full_path.rfind('/') {
                None => &full_path[..],
                Some(slash) => &full_path[(slash + 1)..],
            };

            this.add_file(
                file.to_string(),
                RemotePath {
                    datacenter: datacenter.clone(),
                    datastore: datastore.clone(),
                    path: full_path.clone(),
                },
            );
        }

        this
    }

    fn create_inode(&self) -> u64 {
        self.current_inode.fetch_add(1, Ordering::AcqRel)
    }

    pub fn add_file(&self, path: String, remote_path: RemotePath) {
        let inode = self.create_inode();
        self.files.lock().unwrap().insert(path, inode);
        self.inodes.lock().unwrap().insert(
            inode,
            Entry {
                inode,
                remote_path,
                handle: Mutex::new(None),
            },
        );
    }

    pub async fn handle_request(self: Arc<Self>, request: Request) {
        let res = match request {
            Request::Lookup(r) => self.handle_lookup(r).await,
            _ => todo!("unhandled request: {request:?}"),
        };

        match res {
            Ok(()) => (),
            Err(err) => eprintln!("error handling request: {err:?}"),
        }
    }

    pub async fn handle_lookup(self: Arc<Self>, lookup: requests::Lookup) -> Result<(), Error> {
        if lookup.parent != ROOT_ID {
            lookup.fail(libc::ENOENT)?;
            return Ok(());
        }
        Ok(())
    }
}

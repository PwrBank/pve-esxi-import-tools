use std::collections::{BTreeMap, HashMap};
use std::error::Error as StdError;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Error;
use tokio::sync::watch;

use proxmox_fuse::requests::{self, FuseRequest};
use proxmox_fuse::{Request, ROOT_ID};

use crate::esxi::{EsxiClient, EsxiFile, IsDirectory, NotFound};
use crate::vmx::VmConfig;

const TIMEOUT: f64 = 600.0;

#[derive(Clone, Copy, Debug)]
pub struct Errno(libc::c_int);

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "errno({})", self.0)
    }
}

impl StdError for Errno {}

#[derive(Debug)]
struct RemotePath {
    datacenter: String,
    datastore: String,
    path: String,
}

struct OldFile {
    inode: u64,
    remote_path: RemotePath,
    file: EsxiFile,
    stat: libc::stat,
}

pub struct Fs {
    client: Arc<EsxiClient>,
    config: VmConfig,

    /// map inodes to open file handles
    inodes: Mutex<BTreeMap<u64, Arc<OldFile>>>,

    /// map file names to inodes
    files: Mutex<HashMap<String, u64>>,

    /// used to generate inodes
    current_inode: AtomicU64,

    /// default datacenter to use
    datacenter: String,

    /// default datastore to use
    datastore: String,
}

impl Fs {
    pub async fn new(
        client: Arc<EsxiClient>,
        config: VmConfig,
        datacenter: String,
        datastore: String,
    ) -> Result<Arc<Self>, Error> {
        let this = Arc::new(Self {
            client,
            config,
            inodes: Mutex::new(BTreeMap::new()),
            files: Mutex::new(HashMap::new()),
            current_inode: AtomicU64::new(ROOT_ID + 1),
            datacenter,
            datastore,
        });

        for full_path in this.config.disks.values() {
            // FIXME:
            //   Try creating subdirectories and multiple datastores in esxi and see how that is
            //   represented in the .vmx config file.
            //   for now we just cut off everything except for the final component...
            let file = match full_path.rfind('/') {
                None => &full_path[..],
                Some(slash) => &full_path[(slash + 1)..],
            };

            this.add_file(
                file.to_string(),
                RemotePath {
                    datacenter: this.datacenter.clone(),
                    datastore: this.datastore.clone(),
                    path: full_path.clone(),
                },
            );
        }

        Ok(this)
    }

    fn create_inode(&self) -> u64 {
        self.current_inode.fetch_add(1, Ordering::AcqRel)
    }

    pub async fn add_file(&self, path: String, remote_path: RemotePath) -> Result<u64, Error> {
        log::info!("fixating file {path:?} into {remote_path:?}");

        let file = self
            .client
            .open_file(
                &remote_path.datacenter,
                &remote_path.datastore,
                &remote_path.path,
            )
            .await?;

        let inode = self.create_inode();
        self.files.lock().unwrap().insert(path, inode);
        self.inodes.lock().unwrap().insert(
            inode,
            Arc::new(OldFile {
                inode,
                remote_path,
                stat: file_stat(inode, &file),
                file,
            }),
        );

        Ok(inode)
    }

    pub async fn handle_request(self: Arc<Self>, request: Request) {
        log::info!("FUSE REQUEST: {request:?}");

        let res = match request {
            Request::Getattr(r) => self.handle_getattr(r).await,
            Request::Forget(r) => self.handle_forget(r),
            Request::Lookup(r) => self.handle_lookup(r).await,
            _ => todo!("unhandled request: {request:?}"),
        };

        match res {
            Ok(()) => (),
            Err(err) => eprintln!("error handling request: {err:?}"),
        }
    }

    pub async fn handle_getattr(self: Arc<Self>, getattr: requests::Getattr) -> Result<(), Error> {
        if getattr.inode == ROOT_ID {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };

            stat.st_ino = ROOT_ID;
            stat.st_nlink = 2;
            stat.st_mode = 0o555 | libc::S_IFDIR;
            return Ok(getattr.reply(&stat, TIMEOUT)?);
        }

        let entry = self.inodes.lock().unwrap().get(&getattr.inode).cloned();
        let Some(entry) = entry else {
            return Ok(getattr.fail(libc::ENOENT)?);
        };

        Ok(getattr.reply(&entry.stat, TIMEOUT)?)
    }

    pub fn handle_forget(self: Arc<Self>, forget: requests::Forget) -> Result<(), Error> {
        forget.reply();
        Ok(())
    }

    pub async fn handle_lookup(self: Arc<Self>, lookup: requests::Lookup) -> Result<(), Error> {
        // we currently just have a flat layout of files
        // FIXME:
        //   Try creating subdirectories and multiple datastores in esxi and see how that is
        //   represented in the .vmx config file.

        if lookup.parent != ROOT_ID {
            log::debug!("denying lookup relative to invalid inode");
            return Ok(lookup.fail(libc::ENOENT)?);
        }

        let Some(file_name) = lookup.file_name.to_str() else {
            log::info!("denying non-utf8 file name query");
            return Ok(lookup.fail(libc::ENOENT)?);
        };

        let inode = self.files.lock().unwrap().get(file_name).copied();
        let inode = match inode {
            None => {
                // TODO! See if we need to deal with directories, then this becomes much more
                // annoying!
                return Ok(lookup.fail(libc::ENOENT)?);
            }
            Some(inode) => inode,
        };

        let entry = self.inodes.lock().unwrap().get(&inode).cloned();
        let Some(entry) = entry else {
            return Ok(lookup.fail(libc::ENOENT)?);
        };

        lookup.reply(&proxmox_fuse::EntryParam {
            inode,
            generation: 1,
            attr: entry.stat,
            attr_timeout: TIMEOUT,
            entry_timeout: TIMEOUT,
        })?;

        Ok(())
    }

    pub async fn handle_readdir(self: Arc<Self>, lookup: requests::Readdir) -> Result<(), Error> {
        todo!();
    }
}

fn file_stat(inode: u64, file: &EsxiFile) -> libc::stat {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };

    stat.st_ino = inode;
    stat.st_nlink = 1;
    stat.st_mode = 0o444 | libc::S_IFREG;
    stat.st_size = file.size() as i64;

    stat
}

fn dir_stat(inode: u64) -> libc::stat {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };

    stat.st_ino = inode;
    stat.st_nlink = 2;
    stat.st_mode = 0o555 | libc::S_IFDIR;

    stat
}

struct FsBase {
    client: Arc<EsxiClient>,
    inodes: Mutex<BTreeMap<u64, Inode>>,
    current_inode: AtomicU64,
}

impl FsBase {
    fn new(client: Arc<EsxiClient>) -> Arc<Self> {
        Arc::new(Self {
            client,
            inodes: Mutex::new(BTreeMap::new()),
            current_inode: AtomicU64::new(2),
        })
    }

    fn create_inode(&self) -> u64 {
        self.current_inode.fetch_add(1, Ordering::AcqRel)
    }
}

pub struct NewFs {
    fs: Arc<FsBase>,
    root: Root,
}

impl NewFs {
    pub fn new(client: Arc<EsxiClient>) -> Self {
        let fs = FsBase::new(client);
        Self {
            root: Root::new(Arc::clone(&fs)),
            fs,
        }
    }

    pub fn create_datacenter(&self, name: &str) -> Arc<Datacenter> {
        self.root.create_datacenter(name)
    }

    pub async fn handle_request(self: Arc<Self>, request: Request) {
        log::info!("FUSE REQUEST: {request:?}");

        let res = match request {
            Request::Getattr(r) => self.handle_getattr(r).await,
            Request::Forget(r) => self.handle_forget(r),
            Request::Lookup(r) => self.handle_lookup(r).await,
            _ => todo!("unhandled request: {request:?}"),
        };

        match res {
            Ok(()) => (),
            Err(err) => eprintln!("error handling request: {err:?}"),
        }
    }

    pub fn handle_forget(self: Arc<Self>, forget: requests::Forget) -> Result<(), Error> {
        // TODO: we need to go through also unlink the entry from the parent
        // for this, File needs a parent inode so we can find it (easy enough)
        forget.reply();
        Ok(())
    }

    async fn handle_lookup(self: Arc<Self>, lookup: requests::Lookup) -> Result<(), Error> {
        let Some(file_name) = lookup.file_name.to_str() else {
            log::info!("denying non-utf8 file name query");
            return Ok(lookup.fail(libc::ENOENT)?);
        };

        let inode = if lookup.parent == ROOT_ID {
            self.root.handle_lookup(&file_name)
        } else {
            let parent = self.fs.inodes.lock().unwrap().get(&lookup.parent).cloned();
            match parent {
                None => None,
                Some(parent) => match parent.handle_lookup(&file_name).await {
                    Ok(res) => res,
                    Err(err) => match err.downcast::<Errno>() {
                        Ok(Errno(err)) => return Ok(lookup.fail(err)?),
                        Err(err) => return Err(err),
                    },
                },
            }
        };

        let inode = match inode {
            None => return Ok(lookup.fail(libc::ENOENT)?),
            Some(inode) => inode,
        };

        let entry = self.fs.inodes.lock().unwrap().get(&inode).cloned();
        match entry {
            None => {
                log::error!("lookup produced forgotten inode");
                Ok(lookup.fail(libc::EIO)?)
            }
            Some(entry) => Ok(lookup.reply(&entry.entry_param())?),
        }
    }

    async fn handle_getattr(self: Arc<Self>, getattr: requests::Getattr) -> Result<(), Error> {
        if getattr.inode == ROOT_ID {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };

            stat.st_ino = ROOT_ID;
            stat.st_nlink = 2;
            stat.st_mode = 0o555 | libc::S_IFDIR;
            return Ok(getattr.reply(&stat, TIMEOUT)?);
        }

        let entry = self.fs.inodes.lock().unwrap().get(&getattr.inode).cloned();
        match entry {
            None => {
                log::error!("lookup produced forgotten inode");
                Ok(getattr.fail(libc::EIO)?)
            }
            Some(entry) => Ok(getattr.reply(&entry.stat(), TIMEOUT)?),
        }
    }
}

#[derive(Clone)]
enum Inode {
    Datacenter(Arc<Datacenter>),
    Dir(Arc<Dir>),
    File(Arc<File>),
}

impl Inode {
    fn inode(&self) -> u64 {
        match self {
            Self::Datacenter(entry) => entry.inode,
            Self::Dir(entry) => entry.inode,
            Self::File(entry) => entry.inode,
        }
    }

    async fn handle_lookup(&self, name: &str) -> Result<Option<u64>, Error> {
        Ok(match self {
            Self::Datacenter(dc) => dc.handle_lookup(name),
            Self::Dir(dir) => dir.handle_lookup(name).await?,
            Self::File(_) => return Err(Errno(libc::ENOTDIR).into()),
        })
    }

    fn entry_param(&self) -> proxmox_fuse::EntryParam {
        match self {
            Self::Datacenter(dir) => proxmox_fuse::EntryParam {
                inode: dir.inode,
                generation: 1,
                attr: dir.stat(),
                attr_timeout: TIMEOUT,
                entry_timeout: TIMEOUT,
            },
            Self::Dir(dir) => proxmox_fuse::EntryParam {
                inode: dir.inode,
                generation: 1,
                attr: dir.stat(),
                attr_timeout: TIMEOUT,
                entry_timeout: TIMEOUT,
            },
            Self::File(file) => proxmox_fuse::EntryParam {
                inode: file.inode,
                generation: 1,
                attr: file.stat(),
                attr_timeout: TIMEOUT,
                entry_timeout: TIMEOUT,
            },
        }
    }

    fn stat(&self) -> libc::stat {
        match self {
            Self::Datacenter(dir) => dir.stat(),
            Self::Dir(dir) => dir.stat(),
            Self::File(file) => file.stat(),
        }
    }
}

struct Root {
    fs: Arc<FsBase>,
    datacenters: Mutex<BTreeMap<String, u64>>,
}

impl Root {
    fn new(fs: Arc<FsBase>) -> Self {
        Self {
            fs,
            datacenters: Mutex::new(BTreeMap::new()),
        }
    }

    fn create_datacenter(&self, name: &str) -> Arc<Datacenter> {
        let mut datacenters = self.datacenters.lock().unwrap();
        if let Some(inode) = datacenters.get(name).copied() {
            match self.fs.inodes.lock().unwrap().get(&inode).unwrap() {
                Inode::Datacenter(dc) => return Arc::clone(&dc),
                _ => panic!("create_datacenter hit a non-datacenter inode"),
            }
        }

        let inode = self.fs.create_inode();

        let dc = Arc::new(Datacenter::new(
            Arc::clone(&self.fs),
            inode,
            name.to_string(),
        ));

        self.fs
            .inodes
            .lock()
            .unwrap()
            .insert(inode, Inode::Datacenter(Arc::clone(&dc)));

        datacenters.insert(name.to_string(), inode);

        dc
    }

    fn get_datacenter(&self, name: &str) -> Option<Arc<Datacenter>> {
        let inode = self.handle_lookup(name)?;
        match self.fs.inodes.lock().unwrap().get(&inode)? {
            Inode::Datacenter(dc) => Some(Arc::clone(dc)),
            _ => None,
        }
    }

    fn handle_lookup(&self, name: &str) -> Option<u64> {
        self.datacenters.lock().unwrap().get(name).copied()
    }

    fn forget(&self, forget: requests::Forget) {
        forget.reply();
    }
}

struct Datacenter {
    fs: Arc<FsBase>,
    inode: u64,
    datacenter: String,
    datastores: Mutex<BTreeMap<String, u64>>,
    active_lookups: Mutex<BTreeMap<String, watch::Receiver<Option<u64>>>>,
}

impl Datacenter {
    fn new(fs: Arc<FsBase>, inode: u64, datacenter: String) -> Self {
        Self {
            fs,
            inode,
            datacenter,
            datastores: Mutex::new(BTreeMap::new()),
            active_lookups: Mutex::new(BTreeMap::new()),
        }
    }

    fn create_datastore(&self, name: &str) -> Arc<Dir> {
        let mut datastores = self.datastores.lock().unwrap();
        if let Some(inode) = datastores.get(name).copied() {
            match self.fs.inodes.lock().unwrap().get(&inode).unwrap() {
                Inode::Dir(dir) => return Arc::clone(&dir),
                _ => panic!("create_datastore hit a non-directory inode"),
            }
        }

        let inode = self.fs.create_inode();

        let dir = Arc::new(Dir::new(
            Arc::clone(&self.fs),
            self.inode,
            inode,
            self.datacenter.clone(),
            name.to_string(),
            String::new(),
        ));

        self.fs
            .inodes
            .lock()
            .unwrap()
            .insert(inode, Inode::Dir(Arc::clone(&dir)));

        datastores.insert(name.to_string(), inode);

        dir
    }

    fn get_datastore(&self, name: &str) -> Option<Arc<Dir>> {
        let inode = self.handle_lookup(name)?;
        match self.fs.inodes.lock().unwrap().get(&inode)? {
            Inode::Dir(dc) => Some(Arc::clone(dc)),
            _ => None,
        }
    }

    fn stat(&self) -> libc::stat {
        dir_stat(self.inode)
    }

    fn handle_lookup(&self, name: &str) -> Option<u64> {
        self.datastores.lock().unwrap().get(name).copied()
    }

    fn forget(&self, forget: requests::Forget) {
        forget.reply();
    }
}

struct Dir {
    fs: Arc<FsBase>,
    parent: u64,
    inode: u64,
    datacenter: String,
    datastore: String,
    path: String,
    entries: Mutex<BTreeMap<String, u64>>,
    active_lookups: Mutex<BTreeMap<String, watch::Receiver<Option<u64>>>>,
}

impl Dir {
    fn new(
        fs: Arc<FsBase>,
        parent: u64,
        inode: u64,
        datacenter: String,
        datastore: String,
        path: String,
    ) -> Self {
        Self {
            fs,
            parent,
            inode,
            datacenter,
            datastore,
            path,
            entries: Mutex::new(BTreeMap::new()),
            active_lookups: Mutex::new(BTreeMap::new()),
        }
    }

    async fn handle_lookup(&self, name: &str) -> Result<Option<u64>, Error> {
        let inode = self.entries.lock().unwrap().get(name).copied();
        Ok(match inode {
            None => match self.lookup_new(name).await? {
                None => None,
                inode => inode,
            },
            inode => inode,
        })
    }

    async fn lookup_new(&self, name: &str) -> Result<Option<u64>, Error> {
        let send = {
            let mut active_lookups = self.active_lookups.lock().unwrap();
            if let Some(mut active) = active_lookups.get(name).cloned() {
                drop(active_lookups);
                active.changed().await?;
                return Ok(*active.borrow());
            }

            let (send, recv) = watch::channel(None);
            active_lookups.insert(name.to_string(), recv);
            send
        };

        let (inode, entry) = match self
            .fs
            .client
            .open_file(&self.datacenter, &self.datastore, name)
            .await
        {
            Ok(file) => {
                let inode = self.fs.create_inode();
                let file = Arc::new(File::new(
                    Arc::clone(&self.fs),
                    inode,
                    self.datacenter.clone(),
                    self.datastore.clone(),
                    format!("{}/{name}", self.path),
                    file,
                ));
                (inode, Inode::File(file))
            }
            Err(err) if err.downcast_ref::<IsDirectory>().is_some() => {
                let inode = self.fs.create_inode();
                let dir = Arc::new(Dir::new(
                    Arc::clone(&self.fs),
                    self.inode,
                    inode,
                    self.datacenter.clone(),
                    self.datastore.clone(),
                    format!("{}/{name}", self.path),
                ));
                (inode, Inode::Dir(dir))
            }
            Err(err) if err.downcast_ref::<NotFound>().is_some() => {
                send.send(None)?;
                return Ok(None);
            }
            Err(err) => return Err(err),
        };

        self.fs.inodes.lock().unwrap().insert(inode, entry);

        self.entries.lock().unwrap().insert(name.to_string(), inode);

        send.send(Some(inode))?;

        Ok(Some(inode))
    }

    fn stat(&self) -> libc::stat {
        dir_stat(self.inode)
    }
}

struct File {
    fs: Arc<FsBase>,
    inode: u64,
    datacenter: String,
    datastore: String,
    path: String,
    file: EsxiFile,
    stat: libc::stat,
}

impl File {
    fn new(
        fs: Arc<FsBase>,
        inode: u64,
        datacenter: String,
        datastore: String,
        path: String,
        file: EsxiFile,
    ) -> Self {
        Self {
            fs,
            inode,
            datacenter,
            datastore,
            path,
            stat: file_stat(inode, &file),
            file,
        }
    }

    fn stat(&self) -> libc::stat {
        file_stat(self.inode, &self.file)
    }
}

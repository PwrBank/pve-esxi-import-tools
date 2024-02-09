use std::collections::{BTreeMap, HashMap};
use std::error::Error as StdError;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Error};
use tokio::sync::watch;

use proxmox_fuse::requests::{self, FuseRequest};
use proxmox_fuse::{Request, ROOT_ID};

use crate::esxi::{EsxiClient, EsxiFile, IsDirectory, NotFound};

const TIMEOUT: f64 = 600.0;

#[derive(Clone, Copy, Debug)]
pub struct Errno(libc::c_int);

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "errno({})", self.0)
    }
}

impl StdError for Errno {}

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

pub struct Fs {
    fs: Arc<FsBase>,
    root: Root,
}

impl Fs {
    pub fn new(client: Arc<EsxiClient>) -> Arc<Self> {
        let fs = FsBase::new(client);
        Arc::new(Self {
            root: Root::new(Arc::clone(&fs)),
            fs,
        })
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
            Request::ReaddirPlus(r) => self.handle_readdir(r).await,
            Request::Read(r) => self.handle_read(r).await,
            _ => todo!("unhandled request: {request:?}"),
        };

        match res {
            Ok(()) => (),
            Err(err) => log::error!("error handling request: {err:?}"),
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
            return Ok(getattr.reply(&self.root.stat(), TIMEOUT)?);
        }

        let entry = self.fs.inodes.lock().unwrap().get(&getattr.inode).cloned();
        match entry {
            None => {
                log::error!("lookup produced forgotten inode");
                Ok(getattr.fail(libc::ENOENT)?)
            }
            Some(entry) => Ok(getattr.reply(&entry.stat(), TIMEOUT)?),
        }
    }

    async fn handle_readdir(self: Arc<Self>, readdir: requests::ReaddirPlus) -> Result<(), Error> {
        if readdir.inode == ROOT_ID {
            return self.root.handle_readdir(readdir);
        }

        let entry = self.fs.inodes.lock().unwrap().get(&readdir.inode).cloned();
        match entry {
            None => {
                log::error!("readdir on forgotten inode");
                Ok(readdir.fail(libc::ENOENT)?)
            }
            Some(entry) => entry.handle_readdir(readdir),
        }
    }

    async fn handle_read(self: Arc<Self>, read: requests::Read) -> Result<(), Error> {
        if read.inode == ROOT_ID {
            return Ok(read.fail(libc::EISDIR)?);
        }

        let entry = self.fs.inodes.lock().unwrap().get(&read.inode).cloned();
        match entry {
            None => {
                log::error!("read on forgotten inode");
                Ok(read.fail(libc::ENOENT)?)
            }
            Some(entry) => entry.handle_read(read).await,
        }
    }
}

#[derive(Clone)]
pub(crate) enum Inode {
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

    async fn handle_lookup(&self, name: &str) -> Result<Option<u64>, Error> {
        Ok(match self {
            Self::Datacenter(dc) => dc.handle_lookup(name),
            Self::Dir(dir) => dir.handle_lookup(name).await?,
            Self::File(_) => return Err(Errno(libc::ENOTDIR).into()),
        })
    }

    fn handle_readdir(&self, readdir: requests::ReaddirPlus) -> Result<(), Error> {
        match self {
            Self::Datacenter(dc) => dc.handle_readdir(readdir),
            Self::Dir(dir) => dir.handle_readdir(readdir),
            Self::File(_) => Ok(readdir.fail(libc::ENOTDIR)?),
        }
    }

    async fn handle_read(&self, read: requests::Read) -> Result<(), Error> {
        match self {
            Self::Datacenter(_) => Ok(read.fail(libc::EISDIR)?),
            Self::Dir(_) => Ok(read.fail(libc::EISDIR)?),
            Self::File(file) => file.handle_read(read).await,
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

    fn stat(&self) -> libc::stat {
        dir_stat(ROOT_ID)
    }

    fn handle_readdir(&self, mut readdir: requests::ReaddirPlus) -> Result<(), Error> {
        let datacenters = self.datacenters.lock().unwrap();

        for (count, (name, inode)) in datacenters.iter().skip(readdir.offset as usize).enumerate() {
            if readdir
                .add_entry(
                    name.as_ref(),
                    &dir_stat(*inode),
                    readdir.offset as isize + count as isize + 1,
                    1,
                    TIMEOUT,
                    TIMEOUT,
                )?
                .is_full()
            {
                break;
            }
        }

        Ok(readdir.reply()?)
    }
}

pub(crate) struct Datacenter {
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

    pub fn create_datastore(&self, name: &str) -> Arc<Dir> {
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

    fn handle_readdir(&self, mut readdir: requests::ReaddirPlus) -> Result<(), Error> {
        let datastores = self.datastores.lock().unwrap();

        for (count, (name, inode)) in datastores.iter().skip(readdir.offset as usize).enumerate() {
            if readdir
                .add_entry(
                    name.as_ref(),
                    &dir_stat(*inode),
                    readdir.offset as isize + count as isize + 1,
                    1,
                    TIMEOUT,
                    TIMEOUT,
                )?
                .is_full()
            {
                break;
            }
        }

        Ok(readdir.reply()?)
    }
}

pub(crate) struct Dir {
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

    pub fn create_directory(&self, name: &str) -> Result<Arc<Dir>, Error> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(inode) = entries.get(name).copied() {
            match self.fs.inodes.lock().unwrap().get(&inode).unwrap() {
                Inode::Dir(dir) => return Ok(Arc::clone(&dir)),
                _ => bail!("create_directory() called on an existing non-directory path"),
            }
        }

        let inode = self.fs.create_inode();

        let dir = Arc::new(Dir::new(
            Arc::clone(&self.fs),
            self.inode,
            inode,
            self.datacenter.clone(),
            self.datastore.clone(),
            format!("{}/{name}", self.path),
        ));

        self.fs
            .inodes
            .lock()
            .unwrap()
            .insert(inode, Inode::Dir(Arc::clone(&dir)));

        entries.insert(name.to_string(), inode);

        Ok(dir)
    }

    pub async fn lookup(&self, name: &str) -> Result<Option<Inode>, Error> {
        Ok(match self.handle_lookup(name).await? {
            Some(inode) => self.fs.inodes.lock().unwrap().get(&inode).cloned(),
            None => None,
        })
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
        // static analysis does not understand `drop(mutex_guard)`, so this code is ugly
        // instead...

        let send = 'send: {
            let mut active = {
                let mut active_lookups = self.active_lookups.lock().unwrap();
                match active_lookups.get(name).cloned() {
                    Some(active) => active,
                    None => {
                        let (send, recv) = watch::channel(None);
                        active_lookups.insert(name.to_string(), recv);
                        break 'send send;
                    }
                }
            };

            active.changed().await?;
            return Ok(*active.borrow());
        };

        let full_path = format!("{}/{name}", self.path);
        let (inode, entry) = match self
            .fs
            .client
            .open_file(&self.datacenter, &self.datastore, &full_path)
            .await
        {
            Ok(file) => {
                let inode = self.fs.create_inode();
                let file = Arc::new(File::new(
                    Arc::clone(&self.fs),
                    inode,
                    self.datacenter.clone(),
                    self.datastore.clone(),
                    full_path,
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
                    full_path,
                ));
                (inode, Inode::Dir(dir))
            }
            Err(err) if err.downcast_ref::<NotFound>().is_some() => {
                send.send(None)?;
                return Ok(None);
            }
            Err(err) => {
                log::info!("error? {err:?}");
                return Err(err);
            }
        };

        self.fs.inodes.lock().unwrap().insert(inode, entry);

        self.entries.lock().unwrap().insert(name.to_string(), inode);

        send.send(Some(inode))?;

        Ok(Some(inode))
    }

    fn stat(&self) -> libc::stat {
        dir_stat(self.inode)
    }

    fn handle_readdir(&self, mut readdir: requests::ReaddirPlus) -> Result<(), Error> {
        let skip = readdir.offset;
        let mut at = 0i64;

        let entries = self.entries.lock().unwrap();
        let inodes = self.fs.inodes.lock().unwrap();
        for (name, inode) in entries.iter() {
            let entry = match inodes.get(inode) {
                None => continue,
                Some(entry) => entry,
            };

            at += 1;
            if at <= skip {
                continue;
            }

            if readdir
                .add_entry(
                    name.as_ref(),
                    &entry.stat(),
                    at as isize,
                    1,
                    TIMEOUT,
                    TIMEOUT,
                )?
                .is_full()
            {
                break;
            }
        }

        Ok(readdir.reply()?)
    }
}

pub(crate) struct File {
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

    async fn handle_read(&self, read: requests::Read) -> Result<(), Error> {
        let end = read.offset.saturating_add(read.size as u64);
        let data = self.file.read_at(read.offset..end).await?;
        Ok(read.reply(&data[..])?)
    }
}

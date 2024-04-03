use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::io::IoSlice;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Error;
use tokio::sync::watch;

use proxmox_fuse::requests::{self, FuseRequest};
use proxmox_fuse::{Request, ROOT_ID};

use crate::cache::Cache;
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
        log::debug!("FUSE REQUEST: {request:?}");

        let res = match request {
            Request::Getattr(r) => self.handle_getattr(r).await,
            Request::Forget(r) => self.handle_forget(r),
            Request::Lookup(r) => self.handle_lookup(r).await,
            Request::ReaddirPlus(r) => self.handle_readdir(r).await,
            Request::Read(r) => self.handle_read(r.into()).await,
            Request::Open(r) => self.handle_open(r),
            Request::Release(r) => self.handle_release(r),
            _ => {
                log::debug!("unhandled request: {request:?}");
                return;
            }
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
            log::error!("denying non-utf8 file name query");
            return Ok(lookup.fail(libc::ENOENT)?);
        };

        let inode = if lookup.parent == ROOT_ID {
            self.root.handle_lookup(file_name)
        } else {
            let parent = self.fs.inodes.lock().unwrap().get(&lookup.parent).cloned();
            match parent {
                None => None,
                Some(parent) => match parent.handle_lookup(file_name).await {
                    Ok(res) => res,
                    Err(err) if err.downcast_ref::<NotFound>().is_some() => {
                        // treat the same as Errno(ENOENT)
                        return Ok(lookup.fail(libc::ENOENT)?);
                    }
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

    async fn handle_read(self: Arc<Self>, read: ReadRequest) -> Result<(), Error> {
        if read.inode == ROOT_ID {
            return Ok(read.into_inner().fail(libc::EISDIR)?);
        }

        let entry = self.fs.inodes.lock().unwrap().get(&read.inode).cloned();
        match entry {
            None => {
                log::error!("read on forgotten inode");
                Ok(read.into_inner().fail(libc::ENOENT)?)
            }
            Some(entry) => entry.handle_read(read).await,
        }
    }

    fn handle_open(self: Arc<Self>, open: requests::Open) -> Result<(), Error> {
        if open.inode == ROOT_ID {
            return self.root.handle_open(open);
        }

        let entry = self.fs.inodes.lock().unwrap().get(&open.inode).cloned();
        match entry {
            None => {
                log::error!("open on forgotten inode");
                Ok(open.fail(libc::ENOENT)?)
            }
            Some(entry) => entry.handle_open(open),
        }
    }

    fn handle_release(self: Arc<Self>, release: requests::Release) -> Result<(), Error> {
        if release.inode == ROOT_ID {
            return self.root.handle_release(release);
        }

        let entry = self.fs.inodes.lock().unwrap().get(&release.inode).cloned();
        match entry {
            None => {
                log::error!("release on forgotten inode");
                Ok(release.fail(libc::ENOENT)?)
            }
            Some(entry) => entry.handle_release(release),
        }
    }
}

#[derive(Clone)]
pub enum Inode {
    Datacenter(Arc<Datacenter>),
    Dir(Arc<Dir>),
    File(Arc<File>),
}

impl Inode {
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

    fn inode(&self) -> u64 {
        match self {
            Self::Datacenter(this) => this.inode,
            Self::Dir(this) => this.inode,
            Self::File(this) => this.inode,
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
            Self::Datacenter(dc) => dc.handle_lookup(name).await?,
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

    async fn handle_read(&self, read: ReadRequest) -> Result<(), Error> {
        match self {
            Self::Datacenter(_) => Ok(read.into_inner().fail(libc::EISDIR)?),
            Self::Dir(_) => Ok(read.into_inner().fail(libc::EISDIR)?),
            Self::File(file) => file.handle_read(read).await,
        }
    }

    fn handle_open(&self, open: requests::Open) -> Result<(), Error> {
        match self {
            Self::Datacenter(entry) => entry.handle_open(open),
            Self::Dir(entry) => entry.handle_open(open),
            Self::File(entry) => entry.handle_open(open),
        }
    }

    fn handle_release(&self, release: requests::Release) -> Result<(), Error> {
        match self {
            Self::Datacenter(entry) => entry.handle_release(release),
            Self::Dir(entry) => entry.handle_release(release),
            Self::File(entry) => entry.handle_release(release),
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
                Inode::Datacenter(dc) => return Arc::clone(dc),
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

    fn handle_lookup(&self, name: &str) -> Option<u64> {
        self.datacenters.lock().unwrap().get(name).copied()
    }

    fn stat(&self) -> libc::stat {
        dir_stat(ROOT_ID)
    }

    fn handle_open(&self, open: requests::Open) -> Result<(), Error> {
        Ok(open.reply(0)?)
    }

    fn handle_release(&self, release: requests::Release) -> Result<(), Error> {
        Ok(release.reply()?)
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

pub struct Datacenter {
    fs: Arc<FsBase>,
    inode: u64,
    datacenter: String,
    entries: InodeEntries,
}

impl LookupEntry for Datacenter {
    fn lookup_new_entry<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<(dyn Future<Output = Result<Inode, Error>> + Send + Sync + 'a)>> {
        Box::pin(self.do_lookup(name))
    }
}

impl Datacenter {
    fn new(fs: Arc<FsBase>, inode: u64, datacenter: String) -> Self {
        Self {
            entries: InodeEntries::new(Arc::clone(&fs)),

            fs,
            inode,
            datacenter,
        }
    }

    pub fn do_create_datastore(&self, name: &str) -> Arc<Dir> {
        let mut datastores = self.entries.entries.lock().unwrap();
        if let Some(inode) = datastores.get(name).copied() {
            match self.fs.inodes.lock().unwrap().get(&inode).unwrap() {
                Inode::Dir(dir) => return Arc::clone(dir),
                _ => panic!("create_datastore hit a non-directory inode"),
            }
        }

        let inode = self.fs.create_inode();

        let dir = Arc::new(Dir::new(
            Arc::clone(&self.fs),
            inode,
            self.datacenter.clone(),
            name.to_string(),
            String::new(),
        ));

        datastores.insert(name.to_string(), inode);

        dir
    }

    pub fn create_datastore(&self, name: &str) -> Arc<Dir> {
        let dir = self.do_create_datastore(name);

        self.fs
            .inodes
            .lock()
            .unwrap()
            .insert(dir.inode, Inode::Dir(Arc::clone(&dir)));

        dir
    }

    fn stat(&self) -> libc::stat {
        dir_stat(self.inode)
    }

    async fn handle_lookup(&self, name: &str) -> Result<Option<u64>, Error> {
        self.entries.handle_lookup(name, self).await
    }

    async fn do_lookup(&self, name: &str) -> Result<Inode, Error> {
        if self
            .fs
            .client
            .path_exists(&self.datacenter, name, "")
            .await?
        {
            Ok(Inode::Dir(self.do_create_datastore(name)))
        } else {
            Err(NotFound.into())
        }
    }

    fn handle_readdir(&self, mut readdir: requests::ReaddirPlus) -> Result<(), Error> {
        let datastores = self.entries.entries.lock().unwrap();

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

    fn handle_open(&self, open: requests::Open) -> Result<(), Error> {
        Ok(open.reply(0)?)
    }

    fn handle_release(&self, release: requests::Release) -> Result<(), Error> {
        Ok(release.reply()?)
    }
}

pub struct Dir {
    fs: Arc<FsBase>,
    inode: u64,
    datacenter: String,
    datastore: String,
    path: String,
    entries: InodeEntries,
}

impl LookupEntry for Dir {
    fn lookup_new_entry<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<(dyn Future<Output = Result<Inode, Error>> + Send + Sync + 'a)>> {
        Box::pin(self.do_lookup(name))
    }
}

impl Dir {
    fn new(
        fs: Arc<FsBase>,
        inode: u64,
        datacenter: String,
        datastore: String,
        path: String,
    ) -> Self {
        Self {
            entries: InodeEntries::new(Arc::clone(&fs)),

            fs,
            inode,
            datacenter,
            datastore,
            path,
        }
    }

    pub async fn lookup(&self, name: &str) -> Result<Option<Inode>, Error> {
        Ok(match self.handle_lookup(name).await? {
            Some(inode) => self.fs.inodes.lock().unwrap().get(&inode).cloned(),
            None => None,
        })
    }

    async fn handle_lookup(&self, name: &str) -> Result<Option<u64>, Error> {
        self.entries.handle_lookup(name, self).await
    }

    async fn do_lookup(&self, name: &str) -> Result<Inode, Error> {
        let full_path = format!("{}/{name}", self.path);

        match self
            .fs
            .client
            .open_file(&self.datacenter, &self.datastore, &full_path)
            .await
        {
            Ok(file) => {
                let inode = self.fs.create_inode();
                let file = Arc::new(File::new(inode, file));
                Ok(Inode::File(file))
            }
            Err(err) if err.downcast_ref::<IsDirectory>().is_some() => {
                let inode = self.fs.create_inode();
                let dir = Arc::new(Dir::new(
                    Arc::clone(&self.fs),
                    inode,
                    self.datacenter.clone(),
                    self.datastore.clone(),
                    full_path,
                ));
                Ok(Inode::Dir(dir))
            }
            Err(other) => Err(other),
        }
    }

    fn stat(&self) -> libc::stat {
        dir_stat(self.inode)
    }

    fn handle_readdir(&self, mut readdir: requests::ReaddirPlus) -> Result<(), Error> {
        let skip = readdir.offset;
        let mut at = 0i64;

        let entries = self.entries.entries.lock().unwrap();
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

    fn handle_open(&self, open: requests::Open) -> Result<(), Error> {
        Ok(open.reply(0)?)
    }

    fn handle_release(&self, release: requests::Release) -> Result<(), Error> {
        Ok(release.reply()?)
    }
}

pub struct File {
    inode: u64,
    file: EsxiFile,
    cache: Cache,
}

impl File {
    fn new(inode: u64, file: EsxiFile) -> Self {
        Self {
            inode,
            file,
            cache: Cache::new(
                crate::file_cache_page_size(),
                crate::file_cache_page_count(),
            ),
        }
    }

    fn stat(&self) -> libc::stat {
        file_stat(self.inode, &self.file)
    }

    async fn handle_read(&self, read: ReadRequest) -> Result<(), Error> {
        // Holds the Arcs to the data we reference.
        let mut response_refs = Vec::new();
        // Holds the iovecs.
        let mut response = Vec::new();

        let mut offset = read.offset;

        let end = offset.saturating_add(read.size as u64);
        let mut size = (end - offset) as usize;

        while size != 0 {
            let block = self
                .cache
                .lookup(offset, |from, to| async move {
                    let data = self.file.read_at(from..to).await?;
                    Ok(if data.is_empty() { None } else { Some(data) })
                })
                .await?;

            match block {
                None => break,
                Some(read_result) => {
                    let in_block = (offset - read_result.block_offset) as usize;
                    let bytes: &[u8] = read_result.entry.data.as_ref();
                    let bytes = &bytes[in_block..];
                    let len = size.min(bytes.len());
                    if len == 0 {
                        break;
                    }

                    if response.is_empty() && size <= len {
                        read.into_inner().reply(&bytes[..len])?;
                        return Ok(());
                    }

                    // we need to do a vectored result...
                    response_refs.push(Arc::clone(&read_result.entry));
                    response.push(IoSlice::new(unsafe { &*(&bytes[..len] as *const [u8]) }));
                    offset += len as u64;
                    size -= len;
                }
            }
        }

        if response.is_empty() {
            read.into_inner().reply(&[])?;
        } else {
            read.into_inner().reply_vectored(&response)?;
        }
        Ok(())
    }

    fn handle_open(&self, mut open: requests::Open) -> Result<(), Error> {
        self.cache.enable();
        open.file_info.set_direct_io(true);
        Ok(open.reply(0)?)
    }

    fn handle_release(&self, release: requests::Release) -> Result<(), Error> {
        self.cache.disable();
        Ok(release.reply()?)
    }
}

struct ReadRequest {
    inner: Option<requests::Read>,
}

impl Drop for ReadRequest {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            if let Err(err) = inner.fail(libc::EIO) {
                log::error!("error failing read request: {err}");
            }
        }
    }
}

impl ReadRequest {
    fn into_inner(mut self) -> requests::Read {
        self.inner
            .take()
            .expect("invalid use of read request wrapper")
    }
}

impl std::ops::Deref for ReadRequest {
    type Target = requests::Read;

    fn deref(&self) -> &Self::Target {
        self.inner
            .as_ref()
            .expect("invalid use of read request wrapper")
    }
}

impl From<requests::Read> for ReadRequest {
    fn from(inner: requests::Read) -> Self {
        Self { inner: Some(inner) }
    }
}

trait LookupEntry {
    fn lookup_new_entry<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<(dyn Future<Output = Result<Inode, Error>> + Send + Sync + 'a)>>;
}

/// This handles lookups of inodes for entries. Since lookups of yet-unknown entries require
/// network round trips, this also manages a list of active lookups for the same name, in order to
/// not cause duplicate network access.
struct InodeEntries {
    fs: Arc<FsBase>,
    entries: Mutex<BTreeMap<String, u64>>,
    active_lookups: Mutex<BTreeMap<String, watch::Receiver<Option<u64>>>>,
}

impl InodeEntries {
    fn new(fs: Arc<FsBase>) -> Self {
        Self {
            fs,
            entries: Mutex::new(BTreeMap::new()),
            active_lookups: Mutex::new(BTreeMap::new()),
        }
    }

    async fn handle_lookup<T>(&self, name: &str, lookup: &T) -> Result<Option<u64>, Error>
    where
        T: LookupEntry,
    {
        let inode = self.entries.lock().unwrap().get(name).copied();
        Ok(match inode {
            None => match self.lookup_new(name, lookup).await? {
                None => None,
                inode => inode,
            },
            inode => inode,
        })
    }

    async fn lookup_new<T>(&self, name: &str, lookup: &T) -> Result<Option<u64>, Error>
    where
        T: LookupEntry,
    {
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

            // This will almost always get a RecvError because the sender is immediately dropped,
            // but that's fine.
            let _ = active.changed().await;
            return Ok(*active.borrow());
        };

        let entry = match lookup.lookup_new_entry(name).await {
            Ok(entry) => entry,
            Err(err) if err.downcast_ref::<NotFound>().is_some() => {
                send.send(None)?;
                return Ok(None);
            }
            Err(err) => {
                log::error!("error looking up file or directory: {err:?}");
                return Err(err);
            }
        };

        let inode = entry.inode();
        self.fs.inodes.lock().unwrap().insert(inode, entry);

        // we need to hold the entries lock over the active_lookups lock to make sure a negative
        // entry lookup is not followed by a negative active-lookup lookup by another task.
        let mut entries = self.entries.lock().unwrap();
        let mut active_lookups = self.active_lookups.lock().unwrap();
        entries.insert(name.to_string(), inode);
        send.send_replace(Some(inode));
        active_lookups.remove(name);

        Ok(Some(inode))
    }
}

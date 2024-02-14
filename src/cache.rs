//! Esxi limits read requests over the api by number, so we want to mostly do readahead caching for
//! our use case. This cache dost mostly that, but may also be used for random-access.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::Error;
use hyper::body::Bytes;
use tokio::sync::watch;

pub struct Cache {
    /// Each entry in `entries` has this size.
    block_size: u64,

    /// Entries are powers of two and so we cache the bit mask used to translate arbitrary offsets
    /// to their containing block here.
    block_mask: u64,

    /// This maps an offset to the cached data.
    entries: Mutex<LruMap>,

    /// This contains the currently active lookups, so we don't read the same block multiple times
    /// simultaneously.
    active_lookups: Mutex<BTreeMap<u64, watch::Receiver<Option<Arc<Entry>>>>>,
}

impl Cache {
    pub fn new(block_size: u64, block_count: usize) -> Self {
        assert!(block_size != 0, "Cache::new with empty block size");
        assert!(block_count != 0, "Cache::new with empty block count");
        assert!(
            block_size.is_power_of_two(),
            "Cache::new with non power of 2 block size"
        );

        let block_mask = !(block_size - 1);

        Self {
            block_size,
            block_mask,
            entries: Mutex::new(LruMap::new(block_count)),
            active_lookups: Mutex::new(BTreeMap::new()),
        }
    }

    pub async fn lookup<Fut, F>(&self, offset: u64, fill: F) -> Result<Option<ReadResult>, Error>
    where
        F: FnOnce(u64, u64) -> Fut,
        Fut: Future<Output = Result<Option<Bytes>, Error>> + Send + Sync,
    {
        let block_offset = offset & self.block_mask;
        Ok(self
            .lookup_block(block_offset, fill)
            .await?
            .map(move |entry| ReadResult {
                block_offset,
                entry,
            }))
    }

    async fn lookup_block<Fut, F>(
        &self,
        block_offset: u64,
        fill: F,
    ) -> Result<Option<Arc<Entry>>, Error>
    where
        F: FnOnce(u64, u64) -> Fut,
        Fut: Future<Output = Result<Option<Bytes>, Error>> + Send + Sync,
    {
        {
            let mut entries = self.entries.lock().unwrap();
            if let Some(entry) = entries.get(block_offset) {
                return Ok(Some(Arc::clone(entry)));
            }
        }

        let send = 'send: {
            let mut active = {
                let mut active_lookups = self.active_lookups.lock().unwrap();
                match active_lookups.get(&block_offset).cloned() {
                    None => {
                        let (send, recv) = watch::channel(None);
                        active_lookups.insert(block_offset, recv);
                        break 'send send;
                    }
                    Some(active) => active,
                }
            };
            // This will almost always get a RecvError because the sender is immediately dropped,
            // but that's fine.
            let _ = active.changed().await;
            return Ok(active.borrow().clone());
        };

        let result = match fill(block_offset, block_offset.saturating_add(self.block_size)).await {
            Err(err) => {
                log::error!("cached read failed: {err:?}");
                None
            }
            Ok(None) => None,
            Ok(Some(data)) => Some(Arc::new(Entry { data })),
        };

        // we need to hold the entries lock over the active_lookups lock to make sure a negative
        // entry lookup is not followed by a negative active-lookup lookup by another task.
        let mut entries = self.entries.lock().unwrap();
        let mut active_lookups = self.active_lookups.lock().unwrap();
        if let Some(entry) = &result {
            entries.insert(block_offset, Arc::clone(entry));
        }
        send.send(result.clone())?;
        active_lookups.remove(&block_offset);

        Ok(result)
    }
}

pub struct ReadResult {
    pub block_offset: u64,
    pub entry: Arc<Entry>,
}

pub struct Entry {
    pub data: Bytes,
}

pub struct LruMap {
    /// This is the number of blocks this cache can hold. Currently this is always equal to the
    /// maximum defined by the CLI parameters.
    block_count: usize,

    /// This maps an offset to the cached data.
    entries: BTreeMap<u64, Arc<Entry>>,

    /// This keeps the LRU order of our blocks.
    /// FIXME: this should be replaced with something with better performance...
    order: Vec<u64>,
}

impl LruMap {
    pub fn new(block_count: usize) -> Self {
        Self {
            block_count,
            entries: BTreeMap::new(),
            order: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn insert(&mut self, block_offset: u64, entry: Arc<Entry>) {
        while self.len() + 1 > self.block_count {
            if let Some(oldest) = self.order.pop() {
                log::debug!("dropped cache entry for block at offset {oldest}");
                self.entries.remove(&oldest);
            } else {
                // block_count of 1?
                break;
            }
        }
        self.order.insert(0, block_offset);
        self.entries.insert(block_offset, entry);
    }

    pub fn get(&mut self, block_offset: u64) -> Option<&Arc<Entry>> {
        let entry = self.entries.get(&block_offset)?;
        match self.order.iter().position(|&o| o == block_offset) {
            None => log::debug!("lru order does not contain the current entry"),
            Some(position) => {
                if position != 0 {
                    self.order[..position].rotate_right(1);
                }
            }
        }
        Some(entry)
    }
}

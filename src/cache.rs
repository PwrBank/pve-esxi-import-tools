//! Esxi limits read requests over the api by number, so we want to mostly do readahead caching for
//! our use case. This cache dost mostly that, but may also be used for random-access.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::Error;
use hyper::body::Bytes;
use tokio::sync::watch;

pub struct Cache {
    block_size: u64,
    block_mask: u64,
    block_count: usize,
    entries: Mutex<BTreeMap<u64, Arc<Entry>>>,
    active_lookups: Mutex<BTreeMap<u64, watch::Receiver<Option<Arc<Entry>>>>>,
}

impl Cache {
    pub fn new(block_size: u64, total_bytes: u64) -> Self {
        assert!(block_size != 0, "Cache::new with empty block size");
        assert!(
            block_size.is_power_of_two(),
            "Cache::new with non power of 2 block size"
        );

        let block_mask = !(block_size - 1);

        let total_bytes = (total_bytes + !block_mask) / block_size * block_size;
        assert!(total_bytes != 0, "Cache::new with empty total size");
        let block_count = (total_bytes / block_size) as usize;
        assert!(
            block_count != 0,
            "Cache::new total size causing a block count of zero"
        );

        Self {
            block_size,
            block_mask,
            block_count,
            entries: Mutex::new(BTreeMap::new()),
            active_lookups: Mutex::new(BTreeMap::new()),
        }
    }

    pub async fn lookup<Fut, F>(&self, offset: u64, fill: F) -> Result<Option<ReadResult>, Error>
    where
        F: FnOnce() -> Fut,
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
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<Bytes>, Error>> + Send + Sync,
    {
        {
            let entries = self.entries.lock().unwrap();
            if let Some(entry) = entries.get(&block_offset) {
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
            let _ = active.changed().await?;
            return Ok(active.borrow().clone());
        };

        let result = match fill().await {
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
            while entries.len() > self.block_count {
                // FIXME: We could use an LRU logic here, but we do expect this to be mostly
                // sequential reads...
                entries.pop_first();
            }
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

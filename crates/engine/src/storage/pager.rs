//! The pager hands out pages by number and keeps recently used ones in memory.
//!
//! Reading the store for every access would be slow on a disk, so the pager
//! keeps a small buffer pool: a fixed number of frames, each holding one page.
//! A page that is already in a frame is a hit. Otherwise it is a miss: the
//! least recently used unchanged frame is emptied and the page is read into
//! it, from the write-ahead log if the log has a copy, else from the
//! database file.
//!
//! Changed pages stay in memory until [`Pager::commit`] hands them all to the
//! log at once, or [`Pager::rollback`] drops them. A changed page is never
//! evicted: if every frame holds one, the pool grows past its size until the
//! transaction ends. That keeps uncommitted data out of both files, so
//! recovery never has to undo anything (the textbook name is "no steal").
//! The price is that one transaction's changes must fit in memory.
//!
//! The pager also tracks which pages are free, through a linked list that
//! starts in the header on page 0.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::io;

use super::page::{self, Header, PAGE_SIZE, Page, PageId};
use super::store::{LogStore, PageStore};
use super::wal::{Recovery, Wal};
use crate::error::{Error, Result};

/// Frames in the default buffer pool: 64 pages, 256 KiB.
pub const DEFAULT_POOL_PAGES: usize = 64;

/// Tuning knobs. The defaults suit real use; tests shrink them so that
/// eviction and checkpoints happen often.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Frames in the buffer pool.
    pub pool_pages: usize,
    /// Checkpoint once the log holds this many frames. SQLite's default is
    /// 1000.
    pub checkpoint_frames: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            pool_pages: DEFAULT_POOL_PAGES,
            checkpoint_frames: 1000,
        }
    }
}

/// How the buffer pool has been doing. Useful for tests and for the UI.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

struct Frame {
    id: PageId,
    data: Box<Page>,
    /// Changed since the last commit. Such a frame is never evicted.
    dirty: bool,
    last_used: u64,
}

pub struct Pager {
    wal: Wal,
    frames: Vec<Frame>,
    /// Which frame holds each cached page.
    lookup: HashMap<PageId, usize>,
    capacity: usize,
    checkpoint_frames: u64,
    /// Ticks once per page access, to find the least recently used frame.
    clock: u64,
    page_count: u32,
    header: Header,
    header_dirty: bool,
    /// The header as of the last commit, for rollback.
    committed_header: Header,
    /// Set by an I/O error while writing. After one, nobody knows what the
    /// files hold until recovery reads them, so every later call fails.
    failed: Option<String>,
    recovery: Recovery,
    stats: PoolStats,
}

impl Pager {
    /// Opens a database file and its log, recovering any transactions the
    /// log holds. An empty store becomes a new database with just a header
    /// page, which the first commit writes.
    pub fn open(
        store: Box<dyn PageStore>,
        log: Box<dyn LogStore>,
        options: Options,
    ) -> Result<Pager> {
        let (mut wal, recovery) = Wal::open(store, log)?;
        let (header, page_count, header_dirty) = if wal.page_count() == 0 {
            (Header::default(), 1, true)
        } else {
            let mut page = Box::new([0; PAGE_SIZE]);
            wal.read_page(0, &mut page)?;
            (Header::decode(&page)?, wal.page_count(), false)
        };
        Ok(Pager {
            wal,
            frames: Vec::new(),
            lookup: HashMap::new(),
            capacity: options.pool_pages.max(1),
            checkpoint_frames: options.checkpoint_frames.max(1),
            clock: 0,
            page_count,
            header,
            header_dirty,
            committed_header: header,
            failed: None,
            recovery,
            stats: PoolStats::default(),
        })
    }

    /// Pages in the database, including the header and free pages.
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    pub fn free_count(&self) -> u32 {
        self.header.free_count
    }

    pub fn stats(&self) -> PoolStats {
        self.stats
    }

    /// What recovery found when the database was opened.
    pub fn recovery(&self) -> Recovery {
        self.recovery
    }

    /// Frames in the log that a checkpoint has not copied yet.
    pub fn log_frames(&self) -> u64 {
        self.wal.frames()
    }

    /// Reads a page through the buffer pool.
    pub fn page(&mut self, id: PageId) -> Result<&Page> {
        let index = self.frame(id, true)?;
        Ok(&self.frames[index].data)
    }

    /// A page to replace completely. It is not read from the store first,
    /// since every byte is about to be overwritten.
    pub fn overwrite(&mut self, id: PageId) -> Result<&mut Page> {
        let index = self.frame(id, false)?;
        let frame = &mut self.frames[index];
        frame.dirty = true;
        Ok(&mut frame.data)
    }

    /// A zeroed page, reused from the free list if possible.
    pub fn allocate(&mut self) -> Result<PageId> {
        let id = match self.header.first_free {
            0 => {
                let id = self.page_count;
                self.page_count = id
                    .checked_add(1)
                    .ok_or_else(|| Error::Storage("the database is full".into()))?;
                id
            }
            free => {
                let next = page::decode_free(free, self.page(free)?)?;
                self.header.first_free = next.unwrap_or(0);
                self.header.free_count -= 1;
                self.header_dirty = true;
                free
            }
        };
        self.overwrite(id)?.fill(0);
        Ok(id)
    }

    /// Puts a page on the free list so [`Pager::allocate`] can reuse it.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        let next = self.header.first_free;
        page::encode_free(self.overwrite(id)?, (next != 0).then_some(next));
        self.header.first_free = id;
        self.header.free_count += 1;
        self.header_dirty = true;
        Ok(())
    }

    /// The free list, in order. For consistency checks.
    pub fn free_pages(&mut self) -> Result<Vec<PageId>> {
        let mut pages = Vec::new();
        let mut next = self.header.first_free;
        while next != 0 && pages.len() <= self.page_count as usize {
            pages.push(next);
            next = page::decode_free(next, self.page(next)?)?.unwrap_or(0);
        }
        Ok(pages)
    }

    /// Makes every change since the last commit durable, as one
    /// transaction: each changed page, and the header if it changed, goes to
    /// the log, and the log is synced. Then, if the log has grown long
    /// enough, checkpoints.
    ///
    /// A failed checkpoint does not fail the commit, which is already
    /// durable, but it does stop the pager until the database is reopened.
    pub fn commit(&mut self) -> Result<()> {
        self.usable()?;
        let header = self.header_dirty.then(|| {
            let mut page = Box::new([0; PAGE_SIZE]);
            self.header.encode(&mut page);
            page
        });
        let mut pages: Vec<(PageId, &Page)> = header.iter().map(|page| (0, &**page)).collect();
        pages.extend(
            self.frames
                .iter()
                .filter(|f| f.dirty)
                .map(|f| (f.id, &*f.data)),
        );
        if pages.is_empty() {
            return Ok(());
        }
        pages.sort_unstable_by_key(|&(id, _)| id);
        let committed = self.wal.commit(&pages, self.page_count);
        drop(pages);
        if let Err(e) = committed {
            return Err(self.fail(e));
        }
        for frame in &mut self.frames {
            frame.dirty = false;
        }
        self.header_dirty = false;
        self.committed_header = self.header;
        self.trim();
        if self.wal.frames() >= self.checkpoint_frames
            && let Err(e) = self.wal.checkpoint()
        {
            self.fail(e);
        }
        Ok(())
    }

    /// Forgets every change since the last commit.
    pub fn rollback(&mut self) {
        self.frames.retain(|f| !f.dirty);
        self.reindex();
        let committed = self.wal.page_count();
        self.page_count = committed.max(1);
        self.header = self.committed_header;
        // A database that was never committed still needs its header.
        self.header_dirty = committed == 0;
        self.trim();
    }

    /// Copies the log into the database file and empties it. Changes not
    /// yet committed stay in memory, untouched.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.usable()?;
        self.wal.checkpoint().map_err(|e| self.fail(e))
    }

    /// Hands back the files, for reopening them in tests. Changes not yet
    /// committed are lost, as in a crash.
    pub fn into_stores(self) -> (Box<dyn PageStore>, Box<dyn LogStore>) {
        self.wal.into_stores()
    }

    fn usable(&self) -> Result<()> {
        match &self.failed {
            Some(message) => Err(Error::Storage(message.clone())),
            None => Ok(()),
        }
    }

    fn fail(&mut self, e: io::Error) -> Error {
        let message = format!("{e}; reopen the database to recover");
        self.failed = Some(message.clone());
        Error::Storage(message)
    }

    /// Shrinks a pool that grew during a big transaction back to its size,
    /// keeping the most recently used pages. Only runs when no page is
    /// changed.
    fn trim(&mut self) {
        if self.frames.len() > self.capacity {
            self.frames.sort_unstable_by_key(|f| Reverse(f.last_used));
            self.stats.evictions += (self.frames.len() - self.capacity) as u64;
            self.frames.truncate(self.capacity);
            self.reindex();
        }
    }

    fn reindex(&mut self) {
        self.lookup = self
            .frames
            .iter()
            .enumerate()
            .map(|(i, f)| (f.id, i))
            .collect();
    }

    /// The frame holding page `id`, reading it in on a miss when `load` is set.
    fn frame(&mut self, id: PageId, load: bool) -> Result<usize> {
        self.usable()?;
        // Page 0 is the header, which the pager keeps decoded on its own.
        if id == 0 || id >= self.page_count {
            return Err(Error::Storage(format!("page {id} does not exist")));
        }
        self.clock += 1;
        if let Some(&index) = self.lookup.get(&id) {
            self.stats.hits += 1;
            self.frames[index].last_used = self.clock;
            return Ok(index);
        }
        self.stats.misses += 1;
        let mut data = Box::new([0; PAGE_SIZE]);
        if load {
            self.wal.read_page(id, &mut data)?;
        }
        let frame = Frame {
            id,
            data,
            dirty: false,
            last_used: self.clock,
        };
        // Reuse the least recently used unchanged frame once the pool is
        // full. If every frame is changed, grow instead.
        let victim = (self.frames.len() >= self.capacity)
            .then(|| {
                (0..self.frames.len())
                    .filter(|&i| !self.frames[i].dirty)
                    .min_by_key(|&i| self.frames[i].last_used)
            })
            .flatten();
        let index = match victim {
            Some(victim) => {
                self.lookup.remove(&self.frames[victim].id);
                self.stats.evictions += 1;
                self.frames[victim] = frame;
                victim
            }
            None => {
                self.frames.push(frame);
                self.frames.len() - 1
            }
        };
        self.lookup.insert(id, index);
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::store::{MemoryLog, MemoryStore};

    fn pager(capacity: usize) -> Pager {
        let options = Options {
            pool_pages: capacity,
            ..Options::default()
        };
        Pager::open(
            Box::new(MemoryStore::default()),
            Box::new(MemoryLog::default()),
            options,
        )
        .unwrap()
    }

    #[test]
    fn evicted_pages_are_read_again_after_a_commit() {
        let mut pager = pager(2);
        let ids: Vec<PageId> = (0..5).map(|_| pager.allocate().unwrap()).collect();
        for &id in &ids {
            pager.overwrite(id).unwrap()[100] = id as u8;
        }
        // Changed pages are never evicted, so the pool grew.
        assert_eq!(pager.stats().evictions, 0);
        pager.commit().unwrap();
        for &id in &ids {
            assert_eq!(pager.page(id).unwrap()[100], id as u8);
        }
        assert!(pager.stats().evictions >= 3);

        let (store, log) = pager.into_stores();
        let mut reopened = Pager::open(store, log, Options::default()).unwrap();
        assert_eq!(reopened.recovery().transactions, 1);
        assert_eq!(reopened.page_count(), 6);
        assert_eq!(reopened.page(ids[4]).unwrap()[100], ids[4] as u8);
    }

    #[test]
    fn rollback_forgets_pages_the_free_list_and_new_pages() {
        let mut pager = pager(4);
        let a = pager.allocate().unwrap();
        pager.overwrite(a).unwrap()[0] = 7;
        pager.commit().unwrap();

        pager.overwrite(a).unwrap()[0] = 8;
        let b = pager.allocate().unwrap();
        pager.free(a).unwrap();
        pager.rollback();
        assert_eq!(pager.page(a).unwrap()[0], 7);
        assert_eq!(pager.page_count(), 2);
        assert!(pager.page(b).is_err());
        assert_eq!(pager.free_count(), 0);
        assert_eq!(pager.allocate().unwrap(), b);
    }

    #[test]
    fn freed_pages_are_reused_before_the_file_grows() {
        let mut pager = pager(4);
        let a = pager.allocate().unwrap();
        let b = pager.allocate().unwrap();
        pager.free(a).unwrap();
        pager.free(b).unwrap();
        assert_eq!(pager.free_pages().unwrap(), [b, a]);
        assert_eq!(pager.allocate().unwrap(), b);
        assert_eq!(pager.allocate().unwrap(), a);
        assert_eq!(pager.allocate().unwrap(), 3);
        assert_eq!(pager.free_count(), 0);
    }

    #[test]
    fn the_header_page_is_not_handed_out() {
        let mut pager = pager(4);
        assert!(pager.page(0).is_err());
        assert!(pager.page(1).is_err());
        assert_eq!(pager.allocate().unwrap(), 1);
    }
}

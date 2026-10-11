//! The pager hands out pages by number and keeps recently used ones in memory.
//!
//! Reading the store for every access would be slow on a disk, so the pager
//! keeps a small buffer pool: a fixed number of frames, each holding one page.
//! A page that is already in a frame is a hit. Otherwise it is a miss: the
//! least recently used frame is emptied (written back first if it was
//! changed) and the page is read into it.
//!
//! Changed pages stay in memory until [`Pager::flush`] or until they are
//! evicted. The pager also tracks which pages are free, through a linked list
//! that starts in the header on page 0.

use std::collections::HashMap;

use super::page::{self, Header, PAGE_SIZE, Page, PageId};
use super::store::PageStore;
use crate::error::{Error, Result};

/// Frames in the default buffer pool: 64 pages, 256 KiB.
pub const DEFAULT_POOL_PAGES: usize = 64;

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
    /// Changed since it was read, so it must be written before it is dropped.
    dirty: bool,
    last_used: u64,
}

pub struct Pager {
    store: Box<dyn PageStore>,
    frames: Vec<Frame>,
    /// Which frame holds each cached page.
    lookup: HashMap<PageId, usize>,
    capacity: usize,
    /// Ticks once per page access, to find the least recently used frame.
    clock: u64,
    page_count: u32,
    header: Header,
    header_dirty: bool,
    stats: PoolStats,
}

impl Pager {
    /// Opens a store. An empty store becomes a new database with just a
    /// header page.
    pub fn open(mut store: Box<dyn PageStore>, capacity: usize) -> Result<Pager> {
        let (header, page_count, header_dirty) = if store.page_count() == 0 {
            let empty = Header {
                first_free: 0,
                free_count: 0,
            };
            (empty, 1, true)
        } else {
            let mut page = Box::new([0; PAGE_SIZE]);
            store.read_page(0, &mut page)?;
            (Header::decode(&page)?, store.page_count(), false)
        };
        Ok(Pager {
            store,
            frames: Vec::new(),
            lookup: HashMap::new(),
            capacity: capacity.max(1),
            clock: 0,
            page_count,
            header,
            header_dirty,
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

    /// Writes every changed page and the header to the store, then syncs.
    pub fn flush(&mut self) -> Result<()> {
        for frame in &mut self.frames {
            if frame.dirty {
                self.store.write_page(frame.id, &frame.data)?;
                frame.dirty = false;
            }
        }
        if self.header_dirty {
            let mut page = Box::new([0; PAGE_SIZE]);
            self.header.encode(&mut page);
            self.store.write_page(0, &page)?;
            self.header_dirty = false;
        }
        self.store.sync()?;
        Ok(())
    }

    /// Hands back the store, for reopening it in tests. Call
    /// [`Pager::flush`] first, or unflushed changes are lost.
    pub fn into_store(self) -> Box<dyn PageStore> {
        self.store
    }

    /// The frame holding page `id`, reading it in on a miss when `load` is set.
    fn frame(&mut self, id: PageId, load: bool) -> Result<usize> {
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
            self.store.read_page(id, &mut data)?;
        }
        let frame = Frame {
            id,
            data,
            dirty: false,
            last_used: self.clock,
        };
        let index = if self.frames.len() < self.capacity {
            self.frames.push(frame);
            self.frames.len() - 1
        } else {
            let victim = (0..self.frames.len())
                .min_by_key(|&i| self.frames[i].last_used)
                .expect("the pool has at least one frame");
            let old = &self.frames[victim];
            if old.dirty {
                self.store.write_page(old.id, &old.data)?;
            }
            self.lookup.remove(&old.id);
            self.stats.evictions += 1;
            self.frames[victim] = frame;
            victim
        };
        self.lookup.insert(id, index);
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::store::MemoryStore;

    fn pager(capacity: usize) -> Pager {
        Pager::open(Box::new(MemoryStore::default()), capacity).unwrap()
    }

    #[test]
    fn evicted_pages_are_written_back_and_read_again() {
        let mut pager = pager(2);
        let ids: Vec<PageId> = (0..5).map(|_| pager.allocate().unwrap()).collect();
        for &id in &ids {
            pager.overwrite(id).unwrap()[100] = id as u8;
        }
        for &id in &ids {
            assert_eq!(pager.page(id).unwrap()[100], id as u8);
        }
        assert!(pager.stats().evictions >= 3);

        pager.flush().unwrap();
        let mut reopened = Pager::open(pager.into_store(), 2).unwrap();
        assert_eq!(reopened.page_count(), 6);
        assert_eq!(reopened.page(ids[4]).unwrap()[100], ids[4] as u8);
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

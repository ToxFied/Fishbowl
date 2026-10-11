//! Page stores and log stores: where bytes physically live.
//!
//! The pager only ever asks a store to read or write a whole page by number,
//! so the same database code runs on a real file (native builds) and on plain
//! memory (the browser, which has no file system). The write-ahead log is a
//! plain run of bytes that grows at the end, so it has its own, smaller
//! interface. `fault.rs` has a third pair of stores that lose power on
//! purpose, for the crash tests.
//!
//! Neither kind of store promises anything about a write until `sync`
//! returns. Before that, a crash may keep any of the writes, lose any of
//! them, or keep part of one. See `docs/learn/durability.md`.

use std::io;

use super::page::{PAGE_SIZE, Page, PageId};

pub trait PageStore {
    /// How many pages the store holds. Pages are numbered `0..page_count`.
    fn page_count(&self) -> u32;
    fn read_page(&mut self, id: PageId, page: &mut Page) -> io::Result<()>;
    /// Writes a page. Writing at `page_count` or beyond grows the store.
    fn write_page(&mut self, id: PageId, page: &Page) -> io::Result<()>;
    /// Makes every write so far durable, as far as the backend can promise.
    fn sync(&mut self) -> io::Result<()>;
}

/// A file of bytes for the write-ahead log.
pub trait LogStore {
    /// Bytes in the file.
    fn size(&self) -> u64;
    /// Fills `buf` from `offset`. Reading past the end is an error.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Writes `data` at `offset`, growing the file if needed.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    /// Empties the file.
    fn truncate(&mut self) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
}

/// Pages in a vector. Used in the browser and in tests.
#[derive(Debug, Default)]
pub struct MemoryStore {
    pages: Vec<Box<Page>>,
}

impl PageStore for MemoryStore {
    fn page_count(&self) -> u32 {
        self.pages.len() as u32
    }

    fn read_page(&mut self, id: PageId, page: &mut Page) -> io::Result<()> {
        let stored = self.pages.get(id as usize).ok_or_else(|| past_end(id))?;
        page.copy_from_slice(&stored[..]);
        Ok(())
    }

    fn write_page(&mut self, id: PageId, page: &Page) -> io::Result<()> {
        let index = id as usize;
        if index >= self.pages.len() {
            self.pages
                .resize_with(index + 1, || Box::new([0; PAGE_SIZE]));
        }
        self.pages[index].copy_from_slice(page);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A log in a vector. Used in the browser and in tests.
#[derive(Debug, Default)]
pub struct MemoryLog {
    bytes: Vec<u8>,
}

impl LogStore for MemoryLog {
    fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = offset as usize;
        let stored = self
            .bytes
            .get(start..start + buf.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read past end of log"))?;
        buf.copy_from_slice(stored);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        write_bytes(&mut self.bytes, offset, data);
        Ok(())
    }

    fn truncate(&mut self) -> io::Result<()> {
        self.bytes.clear();
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Writes `data` into `bytes` at `offset`, zero-filling any gap.
pub(crate) fn write_bytes(bytes: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let start = offset as usize;
    if bytes.len() < start + data.len() {
        bytes.resize(start + data.len(), 0);
    }
    bytes[start..start + data.len()].copy_from_slice(data);
}

fn past_end(id: PageId) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("page {id} is past the end of the database"),
    )
}

#[cfg(not(target_arch = "wasm32"))]
pub use file::{FileLog, FileStore};

#[cfg(not(target_arch = "wasm32"))]
mod file {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::Path;

    use super::{LogStore, PAGE_SIZE, Page, PageId, PageStore, past_end};

    /// Pages in one file on disk: page `n` is bytes `n * 4096` to
    /// `(n + 1) * 4096`.
    #[derive(Debug)]
    pub struct FileStore {
        file: File,
        pages: u32,
        /// Bytes in the file. Only a crash can leave this short of a whole
        /// number of pages.
        len: u64,
    }

    impl FileStore {
        /// Opens the file, creating an empty one if it does not exist.
        ///
        /// A crash during a checkpoint can tear the last page of a growing
        /// file, leaving it short. That page counts as a page whose missing
        /// bytes are zeros; recovery rewrites it from the log, which still
        /// holds every page the checkpoint was copying.
        pub fn open(path: &Path) -> io::Result<FileStore> {
            let file = create_or_open(path)?;
            let len = file.metadata()?.len();
            let pages = u32::try_from(len.div_ceil(PAGE_SIZE as u64))
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file is too large"))?;
            Ok(FileStore { file, pages, len })
        }

        fn seek_to(&mut self, id: PageId) -> io::Result<()> {
            self.file
                .seek(SeekFrom::Start(u64::from(id) * PAGE_SIZE as u64))
                .map(|_| ())
        }
    }

    impl PageStore for FileStore {
        fn page_count(&self) -> u32 {
            self.pages
        }

        fn read_page(&mut self, id: PageId, page: &mut Page) -> io::Result<()> {
            if id >= self.pages {
                return Err(past_end(id));
            }
            self.seek_to(id)?;
            let start = u64::from(id) * PAGE_SIZE as u64;
            let available = (self.len - start).min(PAGE_SIZE as u64) as usize;
            self.file.read_exact(&mut page[..available])?;
            page[available..].fill(0);
            Ok(())
        }

        fn write_page(&mut self, id: PageId, page: &Page) -> io::Result<()> {
            self.seek_to(id)?;
            self.file.write_all(page)?;
            self.pages = self.pages.max(id + 1);
            self.len = self.len.max(u64::from(id + 1) * PAGE_SIZE as u64);
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            self.file.sync_data()
        }
    }

    /// The write-ahead log next to a database file, named like SQLite's:
    /// `fish.db-wal` for `fish.db`.
    #[derive(Debug)]
    pub struct FileLog {
        file: File,
        len: u64,
    }

    impl FileLog {
        pub fn open(path: &Path) -> io::Result<FileLog> {
            let file = create_or_open(path)?;
            let len = file.metadata()?.len();
            Ok(FileLog { file, len })
        }
    }

    impl LogStore for FileLog {
        fn size(&self) -> u64 {
            self.len
        }

        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.read_exact(buf)
        }

        fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(data)?;
            self.len = self.len.max(offset + data.len() as u64);
            Ok(())
        }

        fn truncate(&mut self) -> io::Result<()> {
            self.file.set_len(0)?;
            self.len = 0;
            Ok(())
        }

        /// `sync_data` also flushes a change of length, which is what a
        /// truncate or an append needs. On macOS, Rust implements it with
        /// `F_FULLFSYNC`, which also empties the drive's own write cache.
        fn sync(&mut self) -> io::Result<()> {
            self.file.sync_data()
        }
    }

    /// Opens a file for reading and writing, creating it if needed. A new
    /// file's name lives in its directory, so the directory is synced too:
    /// otherwise a crash could forget the file even after its contents
    /// were synced.
    fn create_or_open(path: &Path) -> io::Result<File> {
        let existed = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        #[cfg(unix)]
        if !existed {
            let dir = match path.parent() {
                Some(dir) if !dir.as_os_str().is_empty() => dir,
                _ => Path::new("."),
            };
            File::open(dir)?.sync_all()?;
        }
        #[cfg(not(unix))]
        let _ = existed;
        Ok(file)
    }
}

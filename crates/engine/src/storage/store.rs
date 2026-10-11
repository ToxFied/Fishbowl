//! Page stores: where numbered pages physically live.
//!
//! The pager only ever asks a store to read or write a whole page by number,
//! so the same database code runs on a real file (native builds) and on plain
//! memory (the browser, which has no file system). A later milestone adds a
//! store that fails on purpose, to test crash recovery.

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

fn past_end(id: PageId) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("page {id} is past the end of the database"),
    )
}

#[cfg(not(target_arch = "wasm32"))]
pub use file::FileStore;

#[cfg(not(target_arch = "wasm32"))]
mod file {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::Path;

    use super::{PAGE_SIZE, Page, PageId, PageStore, past_end};

    /// Pages in one file on disk: page `n` is bytes `n * 4096` to
    /// `(n + 1) * 4096`.
    #[derive(Debug)]
    pub struct FileStore {
        file: File,
        pages: u32,
    }

    impl FileStore {
        /// Opens the file, creating an empty one if it does not exist.
        pub fn open(path: &Path) -> io::Result<FileStore> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            let len = file.metadata()?.len();
            if len % PAGE_SIZE as u64 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a whole number of pages", path.display()),
                ));
            }
            let pages = u32::try_from(len / PAGE_SIZE as u64)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file is too large"))?;
            Ok(FileStore { file, pages })
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
            self.file.read_exact(page)
        }

        fn write_page(&mut self, id: PageId, page: &Page) -> io::Result<()> {
            self.seek_to(id)?;
            self.file.write_all(page)?;
            self.pages = self.pages.max(id + 1);
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            self.file.sync_data()
        }
    }
}

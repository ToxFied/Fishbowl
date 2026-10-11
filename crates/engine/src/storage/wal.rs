//! The write-ahead log: how a transaction reaches the disk all at once.
//!
//! A commit never writes the database file. It appends a copy of every page
//! the transaction changed to the log, one *frame* per page, marks the last
//! frame as the commit, and syncs the log. That sync is the moment the
//! transaction becomes durable. Reads look in the log first, for the newest
//! committed copy of a page, and fall back to the database file.
//!
//! Once the log is long enough, a *checkpoint* copies the newest copy of
//! each page into the database file, syncs the file, and only then empties
//! the log. Opening a database starts with *recovery*: read the log from the
//! start, keep every transaction whose frames are all intact up to and
//! including its commit frame, and checkpoint them.
//!
//! Uncommitted changes never reach either file (the pager keeps them in
//! memory), so recovery only ever redoes. It never has to undo. This is
//! SQLite's WAL mode, minus the shared-memory index that lets several
//! processes share one log.
//!
//! ```text
//! Log header (24 bytes)
//!   0..8    magic "FISHWAL1"
//!   8..12   page size (u32, always 4096)
//!   12..16  salt (u32): changes every time the log is emptied
//!   16..24  checksum of bytes 0..16
//!
//! Frame (20-byte header, then the page)
//!   0..4    page number (u32)
//!   4..8    database size in pages after the commit, on a commit frame;
//!           0 on every other frame (u32)
//!   8..12   salt, a copy of the header's (u32)
//!   12..20  checksum of bytes 0..8 and of the page, continuing from the
//!           checksum before it: the previous frame's, or the header's
//!   20..    the page (4096 bytes)
//! ```
//!
//! The checksums form a chain, so each one covers every frame before it.
//! Recovery stops at the first frame that is torn, missing or left over from
//! an earlier attempt, and nothing after that frame can count, even a frame
//! that happens to be intact. The salt makes a frame from before the last
//! checkpoint unmistakable even if its bytes survived somewhere.

use std::collections::HashMap;
use std::io;

use super::page::{PAGE_SIZE, Page, PageId};
use super::store::{LogStore, PageStore};

const MAGIC: &[u8; 8] = b"FISHWAL1";
pub const HEADER_SIZE: u64 = 24;
pub const FRAME_HEADER: usize = 20;
pub const FRAME_SIZE: u64 = (FRAME_HEADER + PAGE_SIZE) as u64;

type Checksum = (u32, u32);

/// What recovery found in the log when the database was opened.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Recovery {
    /// Committed transactions copied from the log into the database file.
    pub transactions: u32,
    /// Their frames.
    pub frames: u64,
    /// Bytes after the last intact commit frame, thrown away: a transaction
    /// whose commit never finished, or a torn write.
    pub discarded_bytes: u64,
}

pub struct Wal {
    db: Box<dyn PageStore>,
    log: Box<dyn LogStore>,
    /// Where the newest committed copy of each page in the log starts.
    index: HashMap<PageId, u64>,
    /// Frames in the log. Every one belongs to a committed transaction.
    frames: u64,
    /// The checksum the next frame continues from.
    chain: Checksum,
    salt: u32,
    /// The database size in pages as of the last commit.
    page_count: u32,
}

impl Wal {
    /// Opens a database file and its log, and recovers: every committed
    /// transaction in the log is copied into the database file, and the log
    /// starts again empty.
    pub fn open(db: Box<dyn PageStore>, log: Box<dyn LogStore>) -> io::Result<(Wal, Recovery)> {
        let page_count = db.page_count();
        let mut wal = Wal {
            db,
            log,
            index: HashMap::new(),
            frames: 0,
            chain: (0, 0),
            salt: 0,
            page_count,
        };
        let recovery = wal.recover()?;
        Ok((wal, recovery))
    }

    /// The database size in pages as of the last commit.
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    /// Frames waiting in the log for a checkpoint.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Reads the newest committed copy of a page.
    pub fn read_page(&mut self, id: PageId, page: &mut Page) -> io::Result<()> {
        match self.index.get(&id) {
            Some(&frame) => self.log.read_at(frame + FRAME_HEADER as u64, page),
            None => self.db.read_page(id, page),
        }
    }

    /// Appends one frame per page, the last one marked as the commit, and
    /// syncs the log. When this returns `Ok`, the transaction is durable.
    /// On an error it may or may not be; only recovery can tell.
    pub fn commit(&mut self, pages: &[(PageId, &Page)], page_count: u32) -> io::Result<()> {
        assert!(!pages.is_empty(), "a commit needs at least one page");
        let mut chain = self.chain;
        if self.frames == 0 {
            // The log was emptied by the last checkpoint: start it again.
            let header = encode_header(self.salt);
            self.log.write_at(0, &header)?;
            chain = checksum(&header[..16], (0, 0));
        }
        let mut offset = HEADER_SIZE + self.frames * FRAME_SIZE;
        let mut frame = vec![0; FRAME_SIZE as usize];
        let mut placed = Vec::with_capacity(pages.len());
        for (i, &(id, page)) in pages.iter().enumerate() {
            let db_size = if i + 1 == pages.len() { page_count } else { 0 };
            chain = encode_frame(&mut frame, id, db_size, self.salt, page, chain);
            self.log.write_at(offset, &frame)?;
            placed.push((id, offset));
            offset += FRAME_SIZE;
        }
        self.log.sync()?;
        self.chain = chain;
        self.frames += pages.len() as u64;
        self.page_count = page_count;
        self.index.extend(placed);
        Ok(())
    }

    /// Copies the newest copy of every page in the log into the database
    /// file, then empties the log. The order is what makes it safe: the log
    /// is emptied only after the database file is synced, so a crash at any
    /// point leaves every committed page in at least one durable place.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        if self.frames == 0 {
            return Ok(());
        }
        let mut pages: Vec<(PageId, u64)> = self.index.iter().map(|(&id, &at)| (id, at)).collect();
        pages.sort_unstable();
        let mut page = Box::new([0; PAGE_SIZE]);
        for (id, frame) in pages {
            self.log
                .read_at(frame + FRAME_HEADER as u64, &mut page[..])?;
            self.db.write_page(id, &page)?;
        }
        self.db.sync()?;
        self.log.truncate()?;
        self.log.sync()?;
        self.index.clear();
        self.frames = 0;
        self.salt = self.salt.wrapping_add(1);
        Ok(())
    }

    /// Hands back the files, for reopening them in tests.
    pub fn into_stores(self) -> (Box<dyn PageStore>, Box<dyn LogStore>) {
        (self.db, self.log)
    }

    fn recover(&mut self) -> io::Result<Recovery> {
        let len = self.log.size();
        let mut recovery = Recovery {
            discarded_bytes: len,
            ..Recovery::default()
        };
        let mut header = [0; HEADER_SIZE as usize];
        let found = if len >= HEADER_SIZE {
            self.log.read_at(0, &mut header)?;
            decode_header(&header)
        } else {
            None
        };
        let Some((salt, mut chain)) = found else {
            // No log, or not even its header survived: nothing was committed
            // since the last checkpoint.
            self.salt = fresh_salt();
            if len > 0 {
                self.log.truncate()?;
                self.log.sync()?;
            }
            return Ok(recovery);
        };
        self.salt = salt;

        // Frames since the last commit frame, which count only once a
        // commit frame closes them.
        let mut pending = Vec::new();
        let mut offset = HEADER_SIZE;
        let mut end = HEADER_SIZE;
        let mut frame = vec![0; FRAME_SIZE as usize];
        while offset + FRAME_SIZE <= len {
            self.log.read_at(offset, &mut frame)?;
            let Some((id, db_size, next)) = decode_frame(&frame, salt, chain) else {
                break;
            };
            chain = next;
            pending.push((id, offset));
            offset += FRAME_SIZE;
            if db_size != 0 {
                self.index.extend(pending.drain(..));
                self.page_count = db_size;
                end = offset;
                recovery.transactions += 1;
            }
        }
        self.frames = (end - HEADER_SIZE) / FRAME_SIZE;
        recovery.frames = self.frames;
        recovery.discarded_bytes = len - end;
        if self.frames > 0 {
            self.checkpoint()?;
        } else {
            self.log.truncate()?;
            self.log.sync()?;
            self.salt = self.salt.wrapping_add(1);
        }
        Ok(recovery)
    }
}

fn encode_header(salt: u32) -> [u8; HEADER_SIZE as usize] {
    let mut header = [0; HEADER_SIZE as usize];
    header[0..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&(PAGE_SIZE as u32).to_be_bytes());
    header[12..16].copy_from_slice(&salt.to_be_bytes());
    let (a, b) = checksum(&header[..16], (0, 0));
    header[16..20].copy_from_slice(&a.to_be_bytes());
    header[20..24].copy_from_slice(&b.to_be_bytes());
    header
}

/// The salt and the checksum the first frame continues from, if the header
/// is intact.
fn decode_header(header: &[u8; HEADER_SIZE as usize]) -> Option<(u32, Checksum)> {
    let sum = checksum(&header[..16], (0, 0));
    let intact = &header[0..8] == MAGIC
        && u32_at(header, 8) == PAGE_SIZE as u32
        && (u32_at(header, 16), u32_at(header, 20)) == sum;
    intact.then(|| (u32_at(header, 12), sum))
}

/// Fills `frame` and returns its checksum.
fn encode_frame(
    frame: &mut [u8],
    id: PageId,
    db_size: u32,
    salt: u32,
    page: &Page,
    chain: Checksum,
) -> Checksum {
    frame[0..4].copy_from_slice(&id.to_be_bytes());
    frame[4..8].copy_from_slice(&db_size.to_be_bytes());
    frame[8..12].copy_from_slice(&salt.to_be_bytes());
    let sum = checksum(page, checksum(&frame[0..8], chain));
    frame[12..16].copy_from_slice(&sum.0.to_be_bytes());
    frame[16..20].copy_from_slice(&sum.1.to_be_bytes());
    frame[FRAME_HEADER..].copy_from_slice(page);
    sum
}

/// The page number, commit size and checksum of an intact frame that
/// continues `chain`.
fn decode_frame(frame: &[u8], salt: u32, chain: Checksum) -> Option<(PageId, u32, Checksum)> {
    if u32_at(frame, 8) != salt {
        return None;
    }
    let sum = checksum(&frame[FRAME_HEADER..], checksum(&frame[0..8], chain));
    ((u32_at(frame, 12), u32_at(frame, 16)) == sum)
        .then(|| (u32_at(frame, 0), u32_at(frame, 4), sum))
}

/// SQLite's WAL checksum: two running sums over 32-bit words, each feeding
/// the other, so a word's position changes its effect. Not cryptographic,
/// but a torn or missing write changes it, and it is cheap.
fn checksum(data: &[u8], (mut s0, mut s1): Checksum) -> Checksum {
    debug_assert_eq!(data.len() % 8, 0);
    for pair in data.chunks_exact(8) {
        s0 = s0.wrapping_add(u32_at(pair, 0)).wrapping_add(s1);
        s1 = s1.wrapping_add(u32_at(pair, 4)).wrapping_add(s0);
    }
    (s0, s1)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// A salt unlike the last log's, for when that log left no header behind.
fn fresh_salt() -> u32 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(0u8) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::store::{MemoryLog, MemoryStore};

    fn page(fill: u8) -> Box<Page> {
        Box::new([fill; PAGE_SIZE])
    }

    fn read(wal: &mut Wal, id: PageId) -> u8 {
        let mut buf = page(0);
        wal.read_page(id, &mut buf).unwrap();
        buf[0]
    }

    fn empty() -> Wal {
        let (wal, _) = Wal::open(
            Box::new(MemoryStore::default()),
            Box::new(MemoryLog::default()),
        )
        .unwrap();
        wal
    }

    /// Two transactions: pages 0 and 1, then pages 1 and 2.
    fn two_commits() -> Wal {
        let mut wal = empty();
        wal.commit(&[(0, &page(10)), (1, &page(11))], 2).unwrap();
        wal.commit(&[(1, &page(21)), (2, &page(22))], 3).unwrap();
        wal
    }

    /// The log's bytes, changed by `edit`, reopened with the same database
    /// file.
    fn reopen_with(wal: Wal, edit: impl FnOnce(&mut Vec<u8>)) -> (Wal, Recovery) {
        let (db, mut log) = wal.into_stores();
        let mut bytes = vec![0; log.size() as usize];
        log.read_at(0, &mut bytes).unwrap();
        edit(&mut bytes);
        let mut log = MemoryLog::default();
        log.write_at(0, &bytes).unwrap();
        Wal::open(db, Box::new(log)).unwrap()
    }

    fn frame_start(n: u64) -> usize {
        (HEADER_SIZE + n * FRAME_SIZE) as usize
    }

    #[test]
    fn pages_come_from_the_log_until_a_checkpoint_moves_them() {
        let mut wal = two_commits();
        assert_eq!(wal.frames(), 4);
        assert_eq!(
            (read(&mut wal, 0), read(&mut wal, 1), read(&mut wal, 2)),
            (10, 21, 22)
        );
        wal.checkpoint().unwrap();
        assert_eq!(wal.frames(), 0);
        assert_eq!(
            (read(&mut wal, 0), read(&mut wal, 1), read(&mut wal, 2)),
            (10, 21, 22)
        );
        // The next commit starts a new log with a new salt.
        wal.commit(&[(2, &page(32))], 3).unwrap();
        assert_eq!(read(&mut wal, 2), 32);
    }

    #[test]
    fn recovery_replays_every_committed_transaction() {
        let (mut wal, recovery) = reopen_with(two_commits(), |_| {});
        assert_eq!(
            recovery,
            Recovery {
                transactions: 2,
                frames: 4,
                discarded_bytes: 0
            }
        );
        assert_eq!(wal.page_count(), 3);
        assert_eq!(wal.frames(), 0, "recovery checkpoints");
        assert_eq!((read(&mut wal, 1), read(&mut wal, 2)), (21, 22));
    }

    #[test]
    fn a_transaction_without_its_commit_frame_is_dropped() {
        // Cut the log inside the second transaction's commit frame.
        let (mut wal, recovery) =
            reopen_with(two_commits(), |log| log.truncate(frame_start(3) + 100));
        assert_eq!(recovery.transactions, 1);
        assert_eq!(recovery.discarded_bytes, FRAME_SIZE + 100);
        assert_eq!(wal.page_count(), 2);
        assert_eq!(read(&mut wal, 1), 11);
    }

    #[test]
    fn a_damaged_frame_ends_the_log_even_if_later_frames_are_intact() {
        // One flipped bit in the first transaction's first frame.
        let (mut wal, recovery) = reopen_with(two_commits(), |log| log[frame_start(0) + 500] ^= 1);
        assert_eq!(recovery.transactions, 0);
        assert_eq!(wal.page_count(), 0);
        // So nothing was copied into the database file.
        assert!(wal.read_page(1, &mut page(0)).is_err());
    }

    #[test]
    fn frames_from_an_older_log_do_not_count() {
        // A frame whose bytes are intact but whose salt is another log's.
        let (_, recovery) = reopen_with(two_commits(), |log| log[frame_start(2) + 8] ^= 1);
        assert_eq!(recovery.transactions, 1);
    }

    #[test]
    fn a_damaged_header_means_an_empty_log() {
        let (mut wal, recovery) = reopen_with(two_commits(), |log| log[3] = b'X');
        assert_eq!(recovery.transactions, 0);
        assert_eq!(recovery.discarded_bytes, frame_start(4) as u64);
        assert_eq!(wal.page_count(), 0);
        // The log was emptied, so new commits start cleanly.
        wal.commit(&[(0, &page(1))], 1).unwrap();
        let (mut wal, recovery) = reopen_with(wal, |_| {});
        assert_eq!(recovery.transactions, 1);
        assert_eq!(read(&mut wal, 0), 1);
    }
}

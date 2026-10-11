//! A simulated disk that loses power on purpose, for crash tests.
//!
//! A [`SimDisk`] holds two files, the database and its log. For each one it
//! keeps what is durable, which is everything up to the file's last sync,
//! and the changes made since. [`SimDisk::crash_at`] picks an operation to
//! cut the power at: that operation and every one after it fail.
//! [`SimDisk::reboot`] then builds the disk as it might look when the power
//! comes back, under the model of a disk the engine promises to survive:
//!
//! - Everything written before a file's last finished sync is there.
//! - Each write or truncate since then may or may not be there, each one
//!   independently of the others. The operating system and the drive are
//!   free to reorder them.
//! - The write in flight at the crash may be partly there: a prefix of it of
//!   any length, as SQLite also assumes. If it was growing the file, the
//!   file may also have grown to its full new length with zeros after the
//!   prefix, since some file systems record a file's length before its data.
//!
//! The model does not cover a disk that loses synced data, or one that
//! flips bits; checksums catch the second, nothing catches the first.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;

use super::page::{PAGE_SIZE, Page, PageId};
use super::store::{LogStore, PageStore, write_bytes};

/// Something that changes the disk. Syncs are counted here too, since the
/// power can go out during one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    PageWrite,
    PageSync,
    LogWrite,
    LogTruncate,
    LogSync,
}

/// How much of the write in flight at a crash reached the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tear {
    /// Bytes from its start that landed.
    pub landed: usize,
    /// The file grew to the write's end anyway, with zeros after the bytes
    /// that landed.
    pub grown: bool,
}

#[derive(Debug, Clone)]
enum Change {
    Write { offset: u64, data: Vec<u8> },
    Truncate,
}

impl Change {
    fn apply(&self, bytes: &mut Vec<u8>) {
        match self {
            Change::Write { offset, data } => write_bytes(bytes, *offset, data),
            Change::Truncate => bytes.clear(),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct File {
    /// Survives any crash.
    durable: Vec<u8>,
    /// Changes since the last sync, oldest first.
    pending: Vec<Change>,
    /// What reads see: `durable` with every pending change applied.
    current: Vec<u8>,
}

impl File {
    fn change(&mut self, change: Change) {
        change.apply(&mut self.current);
        self.pending.push(change);
    }

    fn sync(&mut self) {
        for change in self.pending.drain(..) {
            change.apply(&mut self.durable);
        }
    }
}

#[derive(Debug, Default)]
struct State {
    db: File,
    log: File,
    /// Every operation so far, in order.
    ops: Vec<Op>,
    crash_at: Option<usize>,
    crashed: bool,
    /// The write the power was cut during, if it was a write.
    in_flight: Option<InFlight>,
}

#[derive(Debug)]
struct InFlight {
    to_log: bool,
    offset: u64,
    data: Vec<u8>,
    /// It reached past the end of the file.
    grows: bool,
}

/// A database file and a log file on a disk that can lose power. Cloning
/// shares the disk.
#[derive(Debug, Clone, Default)]
pub struct SimDisk {
    state: Rc<RefCell<State>>,
}

impl SimDisk {
    pub fn new() -> SimDisk {
        SimDisk::default()
    }

    /// The database file, as a page store.
    pub fn database(&self) -> Box<dyn PageStore> {
        Box::new(SimPages(self.clone()))
    }

    /// The log file.
    pub fn log(&self) -> Box<dyn LogStore> {
        Box::new(SimLog(self.clone()))
    }

    /// Cuts the power as operation `n` starts, counting from 0. Every
    /// operation from then on fails, reads included.
    pub fn crash_at(&self, n: usize) {
        self.state.borrow_mut().crash_at = Some(n);
    }

    pub fn crashed(&self) -> bool {
        self.state.borrow().crashed
    }

    /// Every operation so far, in order, including the one cut short.
    pub fn ops(&self) -> Vec<Op> {
        self.state.borrow().ops.clone()
    }

    /// Changes since each file's last sync, which a crash may keep or lose.
    pub fn pending(&self) -> usize {
        let state = self.state.borrow();
        state.db.pending.len() + state.log.pending.len()
    }

    /// Ways the write in flight at the crash could have been cut short. A
    /// prefix of any length can land; these cut it at every 512-byte sector
    /// boundary inside it, and after its first byte, its middle and all but
    /// its last byte. A write that grows the file is also cut with the file
    /// grown, after each of those and after nothing at all. Empty if the
    /// crash was not during a write.
    pub fn tears(&self) -> Vec<Tear> {
        let state = self.state.borrow();
        let Some(write) = &state.in_flight else {
            return Vec::new();
        };
        let len = write.data.len();
        let mut cuts: Vec<usize> = (1..len)
            .filter(|cut| (write.offset + *cut as u64) % 512 == 0)
            .chain([1, len / 2, len - 1])
            .filter(|&cut| cut > 0 && cut < len)
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut tears: Vec<Tear> = cuts
            .iter()
            .map(|&landed| Tear {
                landed,
                grown: false,
            })
            .collect();
        if write.grows {
            tears.extend(std::iter::once(0).chain(cuts).map(|landed| Tear {
                landed,
                grown: true,
            }));
        }
        tears
    }

    /// The disk as it might be after the power comes back: what was
    /// durable, plus the pending changes for which `keep` returns true
    /// (numbered in the order they were made, database file first), plus
    /// what `tear` says of the write in flight.
    pub fn reboot(&self, mut keep: impl FnMut(usize) -> bool, tear: Option<Tear>) -> SimDisk {
        let state = self.state.borrow();
        let mut number = 0;
        let mut survive = |file: &File| {
            let mut bytes = file.durable.clone();
            for change in &file.pending {
                if keep(number) {
                    change.apply(&mut bytes);
                }
                number += 1;
            }
            bytes
        };
        let mut db = survive(&state.db);
        let mut log = survive(&state.log);
        if let (Some(tear), Some(write)) = (tear, &state.in_flight) {
            let bytes = if write.to_log { &mut log } else { &mut db };
            if tear.grown {
                let end = write.offset as usize + write.data.len();
                bytes.resize(bytes.len().max(end), 0);
            }
            write_bytes(bytes, write.offset, &write.data[..tear.landed]);
        }
        let file = |bytes: Vec<u8>| File {
            current: bytes.clone(),
            durable: bytes,
            pending: Vec::new(),
        };
        SimDisk {
            state: Rc::new(RefCell::new(State {
                db: file(db),
                log: file(log),
                ..State::default()
            })),
        }
    }

    /// An independent copy of what the disk holds now, as if every pending
    /// change had reached it.
    pub fn duplicate(&self) -> SimDisk {
        self.reboot(|_| true, None)
    }

    /// Counts an operation, or fails it if the power is out or goes out
    /// now. `write` is what a write would have put on the disk.
    fn start(&self, op: Op, write: Option<(u64, &[u8])>) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        if state.crashed {
            return Err(power_off());
        }
        let n = state.ops.len();
        state.ops.push(op);
        if state.crash_at == Some(n) {
            state.crashed = true;
            let to_log = matches!(op, Op::LogWrite);
            let len = if to_log { &state.log } else { &state.db }.current.len();
            state.in_flight = write.map(|(offset, data)| InFlight {
                to_log,
                offset,
                data: data.to_vec(),
                grows: offset as usize + data.len() > len,
            });
            return Err(power_off());
        }
        Ok(())
    }

    fn check_power(&self) -> io::Result<()> {
        match self.crashed() {
            true => Err(power_off()),
            false => Ok(()),
        }
    }
}

fn power_off() -> io::Error {
    io::Error::other("the power went out")
}

struct SimPages(SimDisk);

/// Follows the same rules as `FileStore`: a short last page counts, and
/// its missing bytes read as zeros.
impl PageStore for SimPages {
    fn page_count(&self) -> u32 {
        self.0.state.borrow().db.current.len().div_ceil(PAGE_SIZE) as u32
    }

    fn read_page(&mut self, id: PageId, page: &mut Page) -> io::Result<()> {
        self.0.check_power()?;
        let state = self.0.state.borrow();
        let bytes = &state.db.current;
        let start = id as usize * PAGE_SIZE;
        if start >= bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("page {id} is past the end of the database"),
            ));
        }
        let end = bytes.len().min(start + PAGE_SIZE);
        page[..end - start].copy_from_slice(&bytes[start..end]);
        page[end - start..].fill(0);
        Ok(())
    }

    fn write_page(&mut self, id: PageId, page: &Page) -> io::Result<()> {
        let offset = u64::from(id) * PAGE_SIZE as u64;
        self.0.start(Op::PageWrite, Some((offset, page)))?;
        self.0.state.borrow_mut().db.change(Change::Write {
            offset,
            data: page.to_vec(),
        });
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.0.start(Op::PageSync, None)?;
        self.0.state.borrow_mut().db.sync();
        Ok(())
    }
}

struct SimLog(SimDisk);

impl LogStore for SimLog {
    fn size(&self) -> u64 {
        self.0.state.borrow().log.current.len() as u64
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.check_power()?;
        let state = self.0.state.borrow();
        let start = offset as usize;
        let stored = state
            .log
            .current
            .get(start..start + buf.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read past end of log"))?;
        buf.copy_from_slice(stored);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.0.start(Op::LogWrite, Some((offset, data)))?;
        self.0.state.borrow_mut().log.change(Change::Write {
            offset,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn truncate(&mut self) -> io::Result<()> {
        self.0.start(Op::LogTruncate, None)?;
        self.0.state.borrow_mut().log.change(Change::Truncate);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.0.start(Op::LogSync, None)?;
        self.0.state.borrow_mut().log.sync();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_bytes(disk: &SimDisk) -> Vec<u8> {
        disk.state.borrow().log.current.clone()
    }

    #[test]
    fn synced_writes_survive_and_later_ones_are_up_to_the_crash() {
        let disk = SimDisk::new();
        let mut log = disk.log();
        log.write_at(0, b"one").unwrap();
        log.sync().unwrap();
        log.write_at(3, b"two").unwrap();
        log.write_at(6, b"three").unwrap();
        disk.crash_at(4);
        assert!(log.write_at(11, b"four").is_err());
        assert!(
            log.read_at(0, &mut [0; 3]).is_err(),
            "nothing runs after a crash"
        );
        assert_eq!(disk.pending(), 2);

        assert_eq!(log_bytes(&disk.reboot(|_| false, None)), b"one");
        assert_eq!(log_bytes(&disk.reboot(|_| true, None)), b"onetwothree");
        // The second write landed and the first did not.
        assert_eq!(log_bytes(&disk.reboot(|n| n == 1, None)), b"one\0\0\0three");
        let landed: Vec<(usize, bool)> = disk.tears().iter().map(|t| (t.landed, t.grown)).collect();
        assert_eq!(
            landed,
            [
                (1, false),
                (2, false),
                (3, false),
                (0, true),
                (1, true),
                (2, true),
                (3, true)
            ]
        );
        let torn = |landed, grown| log_bytes(&disk.reboot(|_| true, Some(Tear { landed, grown })));
        assert_eq!(torn(2, false), b"onetwothreefo");
        // The file grew to the write's end, but only "fo" arrived.
        assert_eq!(torn(2, true), b"onetwothreefo\0\0");
    }

    #[test]
    fn tears_include_every_sector_boundary() {
        let disk = SimDisk::new();
        disk.crash_at(0);
        let page = [7; PAGE_SIZE];
        assert!(disk.database().write_page(1, &page).is_err());
        let cuts: Vec<usize> = disk
            .tears()
            .iter()
            .filter(|t| !t.grown)
            .map(|t| t.landed)
            .collect();
        assert_eq!(cuts, [1, 512, 1024, 1536, 2048, 2560, 3072, 3584, 4095]);
        let torn = disk.reboot(
            |_| true,
            Some(Tear {
                landed: 512,
                grown: false,
            }),
        );
        let mut store = torn.database();
        assert_eq!(store.page_count(), 2, "a short last page still counts");
        let mut read = [0; PAGE_SIZE];
        store.read_page(1, &mut read).unwrap();
        assert_eq!(read[511], 7);
        assert_eq!(read[512], 0);
    }
}

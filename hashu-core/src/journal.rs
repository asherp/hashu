//! Append-only JSON-lines journal, the on-disk form of redemption state.
//!
//! Each record is one line, written with a single `write` call while the
//! owner holds its own lock, so records land in the order their changes
//! were applied. A process killed mid-write leaves at most one partial
//! last line; [`Journal::open`] skips it and starts the next record on a
//! fresh line. Records are not fsynced: a container restart or redeploy
//! keeps the page cache, and losing the last instant of share credit on a
//! host power cut only under-credits a redemption slightly.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde::Serialize;

#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    file: Mutex<File>,
}

impl Journal {
    /// Open (creating if missing) the journal at `path` and return it with
    /// every record that parses. Lines that do not parse are skipped, and
    /// their count is returned so the caller can log it.
    pub fn open<T: DeserializeOwned>(path: &Path) -> io::Result<(Self, Vec<T>, usize)> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let mut records = Vec::new();
        let mut skipped = 0;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str(line) {
                Ok(r) => records.push(r),
                Err(_) => skipped += 1,
            }
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        if !text.is_empty() && !text.ends_with('\n') {
            // Torn last record: terminate it so the next one parses.
            file.write_all(b"\n")?;
        }
        Ok((
            Self {
                path: path.to_path_buf(),
                file: Mutex::new(file),
            },
            records,
            skipped,
        ))
    }

    /// Replace the journal's contents with `records` (atomic rename), and
    /// keep appending after them.
    pub fn rewrite<T: Serialize>(&self, records: &[T]) -> io::Result<()> {
        let mut buf = Vec::new();
        for r in records {
            serde_json::to_writer(&mut buf, r)?;
            buf.push(b'\n');
        }
        let tmp = self.path.with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&buf)?;
            f.sync_all()?;
        }
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        fs::rename(&tmp, &self.path)?;
        *guard = OpenOptions::new().append(true).open(&self.path)?;
        Ok(())
    }

    /// Append one record. Errors are logged by the caller's choice; state
    /// in memory is already updated, so a failed write only loses durability.
    pub fn append<T: Serialize>(&self, record: &T) -> io::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(&line)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

//! The build database: for every output, the hash of the command that made it and a stamp (time stamp and content
//! hash) of every file it read. Records append to a journal as they happen, so a crash or Ctrl-C loses nothing;
//! the journal folds into the main file now and then. Paths are interned to keep the files small.

use crate::util;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"ANVILDB2";

// Journal entry tags
const TAG_PATH: u8 = 1;
const TAG_RECORD: u8 = 2;

// Fold the journal into the main file once it holds this many records
const COMPACT_AFTER_RECORDS: usize = 2000;

/// What one input looked like when its output was built. A hash of 0 never matches (the file changed mid-build).
#[derive(Debug, Clone, Copy)]
pub struct Stamp {
    pub path: u32,
    pub mtime: i64,
    pub hash: u64,
}

#[derive(Debug, Clone)]
pub struct Record {
    pub command_hash: u64,
    pub inputs: Vec<Stamp>,
}

pub struct Database {
    file: PathBuf,
    journal_file: PathBuf,
    journal: Option<BufWriter<File>>,
    journal_records: usize,

    // Interned paths: id -> spelling, case-folded key -> id
    paths: Vec<String>,
    path_ids: HashMap<String, u32>,

    // Output path id -> record
    records: HashMap<u32, Record>,

    // Held for the database's lifetime: one build per output folder at a time
    _lock: File,
}

impl Database {
    /// Opens (or creates) the database in `dir`, waiting for any other build of the same folder to finish.
    pub fn open(dir: &Path) -> Result<Database> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

        // One build per output folder at a time
        let lock = File::options().create(true).truncate(false).write(true).open(dir.join("lock"))?;
        if lock.try_lock().is_err() {
            crate::ui::status("Waiting", "for another anvil build of this folder to finish");
            lock.lock().context("locking the build folder")?;
        }

        let mut database = Database {
            file: dir.join("db"),
            journal_file: dir.join("db.journal"),
            journal: None,
            journal_records: 0,
            paths: Vec::new(),
            path_ids: HashMap::new(),
            records: HashMap::new(),
            _lock: lock,
        };

        // A damaged database only costs a rebuild
        if let Err(error) = database.load() {
            crate::ui::warn(&format!("build database unreadable ({error:#}); rebuilding everything"));
            database.paths.clear();
            database.path_ids.clear();
            database.records.clear();
            database.journal_records = 0;
            let _ = std::fs::remove_file(&database.journal_file);
        }

        Ok(database)
    }

    /// The id of a path, interning it on first sight.
    pub fn intern(&mut self, path: &str) -> u32 {
        let key = util::path_key(Path::new(path));
        if let Some(id) = self.path_ids.get(&key) {
            return *id;
        }

        let id = self.paths.len() as u32;
        self.paths.push(path.to_string());
        self.path_ids.insert(key, id);
        self.journal_entry_path(path);

        id
    }

    pub fn path(&self, id: u32) -> &str {
        &self.paths[id as usize]
    }

    pub fn get(&self, output: &Path) -> Option<&Record> {
        let id = self.path_ids.get(&util::path_key(output))?;
        self.records.get(id)
    }

    /// Stores the record for an output, in memory and in the journal.
    pub fn put(&mut self, output: &Path, record: Record) {
        let output_id = self.intern(&output.to_string_lossy());
        self.journal_entry_record(output_id, &record);
        self.records.insert(output_id, record);
    }

    /// Every (path, mtime, hash) this database knows, to seed the content-hash cache.
    pub fn known_stamps(&self) -> impl Iterator<Item = (&str, i64, u64)> {
        self.records
            .values()
            .flat_map(|record| record.inputs.iter())
            .map(|stamp| (self.path(stamp.path), stamp.mtime, stamp.hash))
    }

    /// Flushes the journal, folding it into the main file when it has grown.
    pub fn finish(&mut self) -> Result<()> {
        if let Some(journal) = &mut self.journal {
            journal.flush()?;
        }

        if self.journal_records >= COMPACT_AFTER_RECORDS {
            self.compact()?;
        }

        Ok(())
    }

    /// Rewrites the main file with only the paths still in use, and empties the journal.
    pub fn compact(&mut self) -> Result<()> {
        self.journal = None;

        // Renumber the paths records still use
        let mut renumbered: HashMap<u32, u32> = HashMap::new();
        let mut paths: Vec<String> = Vec::new();
        let old_paths = &self.paths;
        let mut new_id = |id: u32| -> u32 {
            *renumbered.entry(id).or_insert_with(|| {
                paths.push(old_paths[id as usize].clone());
                (paths.len() - 1) as u32
            })
        };

        let mut body = Vec::new();
        for (output, record) in &self.records {
            put_u32(&mut body, new_id(*output));
            put_record_body(&mut body, record, &mut new_id);
        }
        let record_count = self.records.len() as u32;

        // Header, path table, records
        let mut bytes = MAGIC.to_vec();
        put_u32(&mut bytes, paths.len() as u32);
        for path in &paths {
            put_string(&mut bytes, path);
        }
        put_u32(&mut bytes, record_count);
        bytes.extend(body);

        util::write_atomic(&self.file, &bytes)?;
        let _ = std::fs::remove_file(&self.journal_file);

        // Reload so ids match the new file
        self.paths.clear();
        self.path_ids.clear();
        self.records.clear();
        self.journal_records = 0;
        self.load()
    }

    fn load(&mut self) -> Result<()> {
        // Main file
        if let Ok(bytes) = std::fs::read(&self.file) {
            let mut reader = Reader { bytes: &bytes, position: 0 };
            if reader.take(8)? != MAGIC {
                bail!("not an Anvil database");
            }

            let path_count = reader.u32()?;
            for _ in 0..path_count {
                let path = reader.string()?;
                self.add_path(path);
            }

            let record_count = reader.u32()?;
            for _ in 0..record_count {
                let output = reader.u32()?;
                let record = reader.record()?;
                self.records.insert(output, record);
            }
        }

        // Journal, up to the last complete entry
        if let Ok(mut file) = File::open(&self.journal_file) {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let mut reader = Reader { bytes: &bytes, position: 0 };

            while reader.position < bytes.len() {
                let start = reader.position;
                let entry = (|| -> Result<()> {
                    match reader.u8()? {
                        TAG_PATH => {
                            let path = reader.string()?;
                            self.add_path(path);
                        }
                        TAG_RECORD => {
                            let output = reader.u32()?;
                            let record = reader.record()?;
                            self.records.insert(output, record);
                            self.journal_records += 1;
                        }
                        _ => bail!("bad journal entry"),
                    }
                    Ok(())
                })();

                // A torn last entry (killed mid-write) is dropped
                if entry.is_err() {
                    bytes.truncate(start);
                    std::fs::write(&self.journal_file, &bytes)?;
                    break;
                }
            }
        }

        // Every record must name known paths
        let path_count = self.paths.len() as u32;
        let valid = self
            .records
            .iter()
            .all(|(output, record)| *output < path_count && record.inputs.iter().all(|stamp| stamp.path < path_count));
        if !valid {
            bail!("database refers to unknown paths");
        }

        Ok(())
    }

    fn add_path(&mut self, path: String) {
        let id = self.paths.len() as u32;
        self.path_ids.insert(util::path_key(Path::new(&path)), id);
        self.paths.push(path);
    }

    fn journal(&mut self) -> Option<&mut BufWriter<File>> {
        if self.journal.is_none() {
            let file = File::options().create(true).append(true).open(&self.journal_file).ok()?;
            self.journal = Some(BufWriter::new(file));
        }
        self.journal.as_mut()
    }

    fn journal_entry_path(&mut self, path: &str) {
        let mut bytes = vec![TAG_PATH];
        put_string(&mut bytes, path);

        if let Some(journal) = self.journal() {
            let _ = journal.write_all(&bytes);
        }
    }

    fn journal_entry_record(&mut self, output: u32, record: &Record) {
        let mut bytes = vec![TAG_RECORD];
        put_u32(&mut bytes, output);
        put_record_body(&mut bytes, record, |id| id);

        if let Some(journal) = self.journal() {
            let _ = journal.write_all(&bytes);
            // Whole entries reach the disk together
            let _ = journal.flush();
        }
        self.journal_records += 1;
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl Reader<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8]> {
        if self.position + count > self.bytes.len() {
            bail!("truncated");
        }

        let slice = &self.bytes[self.position..self.position + count];
        self.position += count;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }

    fn string(&mut self) -> Result<String> {
        let length = self.u32()? as usize;
        Ok(String::from_utf8(self.take(length)?.to_vec())?)
    }

    fn record(&mut self) -> Result<Record> {
        let command_hash = self.u64()?;
        let count = self.u32()? as usize;

        let mut inputs = Vec::with_capacity(count.min(100_000));
        for _ in 0..count {
            inputs.push(Stamp {
                path: self.u32()?,
                mtime: self.u64()? as i64,
                hash: self.u64()?,
            });
        }

        Ok(Record { command_hash, inputs })
    }
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_string(bytes: &mut Vec<u8>, text: &str) {
    put_u32(bytes, text.len() as u32);
    bytes.extend_from_slice(text.as_bytes());
}

fn put_record_body(bytes: &mut Vec<u8>, record: &Record, mut path_id: impl FnMut(u32) -> u32) {
    put_u64(bytes, record.command_hash);
    put_u32(bytes, record.inputs.len() as u32);

    for stamp in &record.inputs {
        put_u32(bytes, path_id(stamp.path));
        put_u64(bytes, stamp.mtime as u64);
        put_u64(bytes, stamp.hash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn survives_reopen_and_compaction() {
        let dir = std::env::temp_dir().join(format!("anvil-db-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let output = PathBuf::from("C:/out/a.obj");

        // Write through the journal
        {
            let mut database = Database::open(&dir).unwrap();
            let input = database.intern("C:/src/a.cpp");
            database.put(
                &output,
                Record {
                    command_hash: 42,
                    inputs: vec![Stamp {
                        path: input,
                        mtime: 7,
                        hash: 9,
                    }],
                },
            );
            database.finish().unwrap();
        }

        // Read back, compact, read back again
        {
            let mut database = Database::open(&dir).unwrap();
            assert_eq!(database.get(&output).unwrap().command_hash, 42);
            database.compact().unwrap();
        }
        let database = Database::open(&dir).unwrap();
        let record = database.get(&output).unwrap();
        assert_eq!(database.path(record.inputs[0].path), "C:/src/a.cpp");
        assert_eq!(record.inputs[0].hash, 9);

        drop(database);
        std::fs::remove_dir_all(&dir).ok();
    }
}

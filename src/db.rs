//! The build database: for every output, the hash of the command that made it, the command itself (for --explain),
//! the outputs' time stamps and a stamp (time stamp and content hash) of every file it read. Records append to a
//! journal as they happen, so a crash or Ctrl-C loses nothing; the journal folds into the main file now and then.
//! Paths and arguments are interned to keep the files small.

use crate::util;
use anyhow::{bail, Context, Result};
use std::cell::OnceCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"ANVILDB3";
const JOURNAL_MAGIC: &[u8; 8] = b"ANVILJN3";

// What an older Anvil's database starts with: read as empty (everything rebuilds once)
const MAGIC_FAMILY: &[u8] = b"ANVILDB";

// Journal entry tags
const TAG_PATH: u8 = 1;
const TAG_RECORD: u8 = 2;
const TAG_ARGUMENT: u8 = 3;
const TAG_FORGET: u8 = 4;

// Fold the journal into the main file once it holds this many records
const COMPACT_AFTER_RECORDS: usize = 2000;

/// A time stamp no file has: an output missing once its action finished never passes for up to date.
pub const NO_TIME_STAMP: i64 = i64::MIN;

/// What one input looked like when its output was built. A hash of 0: it changed while its action ran, which runs again.
#[derive(Debug, Clone, Copy)]
pub struct Stamp {
    pub path: u32,
    pub mtime: i64,
    pub hash: u64,
}

#[derive(Debug, Clone)]
pub struct Record {
    pub command_hash: u64,

    /// The program and its arguments (argument ids).
    pub command: Vec<u32>,

    /// Each output's time stamp once the action finished, in the action's order.
    pub outputs: Vec<i64>,

    pub inputs: Vec<Stamp>,
}

pub struct Database {
    file: PathBuf,
    journal_file: PathBuf,
    journal: Option<BufWriter<File>>,
    journal_records: usize,

    // Changes with every compaction; the journal names the one it extends
    generation: u64,

    // Interned paths: id -> spelling, case-folded key -> id
    paths: Vec<String>,
    path_ids: HashMap<String, u32>,

    // Interned command arguments, decoded on first use: a build with nothing to do never needs them
    arguments: OnceCell<Arguments>,
    argument_source: ArgumentSource,

    // Output path id -> record
    records: HashMap<u32, Record>,

    // Held for the database's lifetime: one build per output folder at a time
    _lock: File,
}

/// Interned command arguments: id -> text, text -> id.
#[derive(Default)]
struct Arguments {
    texts: Vec<String>,
    ids: HashMap<String, u32>,
}

/// Where the arguments come from until they are decoded: the main file's table as stored (its strings, without the
/// count), then the journal's.
#[derive(Default)]
struct ArgumentSource {
    table: Vec<u8>,
    journal: Vec<String>,
    count: u32,
}

/// Old ids to new ones while compacting, keeping only what records still use.
#[derive(Default)]
struct Renumbering {
    ids: HashMap<u32, u32>,
    kept: Vec<String>,
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
            generation: 0,
            paths: Vec::new(),
            path_ids: HashMap::new(),
            arguments: OnceCell::new(),
            argument_source: ArgumentSource::default(),
            records: HashMap::new(),
            _lock: lock,
        };

        // A damaged database only costs a rebuild, once: its files are dropped so the next build starts clean
        if let Err(error) = database.load() {
            crate::ui::warn(&format!("build database unreadable ({error:#}); rebuilding everything"));
            database.clear();
            let _ = std::fs::remove_file(&database.file);
            let _ = std::fs::remove_file(&database.journal_file);
        }

        Ok(database)
    }

    /// The id of a path, interning it on first sight.
    pub fn intern(&mut self, path: &str) -> u32 {
        self.intern_keyed(util::path_key(Path::new(path)), path)
    }

    /// `intern` with the path's `util::path_key` already made.
    pub fn intern_keyed(&mut self, key: String, path: &str) -> u32 {
        if let Some(id) = self.path_ids.get(&key) {
            return *id;
        }

        let id = self.paths.len() as u32;
        self.paths.push(path.to_string());
        self.path_ids.insert(key, id);
        self.journal_entry_text(TAG_PATH, path);

        id
    }

    pub fn path(&self, id: u32) -> &str {
        &self.paths[id as usize]
    }

    /// The id of a path already interned.
    pub fn id(&self, path: &Path) -> Option<u32> {
        self.path_ids.get(&util::path_key(path)).copied()
    }

    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    /// The id of a command argument (exact text), interning it on first sight.
    pub fn intern_argument(&mut self, text: &str) -> u32 {
        self.decode_arguments();
        let arguments = self.arguments.get_mut().expect("arguments decoded");
        if let Some(id) = arguments.ids.get(text) {
            return *id;
        }

        let id = arguments.texts.len() as u32;
        arguments.texts.push(text.to_string());
        arguments.ids.insert(text.to_string(), id);
        self.journal_entry_text(TAG_ARGUMENT, text);

        id
    }

    pub fn argument(&self, id: u32) -> &str {
        &self.decode_arguments().texts[id as usize]
    }

    /// The record of an output, by its path id.
    pub fn record(&self, output: u32) -> Option<&Record> {
        self.records.get(&output)
    }

    /// Stores the record for an output, in memory and in the journal.
    pub fn put(&mut self, output: u32, record: Record) {
        self.journal_entry_record(output, &record);
        self.records.insert(output, record);
    }

    /// Drops the record of an output about to be rebuilt: should the build fail or stop, whatever it left behind is
    /// never taken for up to date.
    pub fn forget(&mut self, output: u32) {
        if self.records.remove(&output).is_none() {
            return;
        }

        let mut bytes = vec![TAG_FORGET];
        put_u32(&mut bytes, output);
        self.journal_write(&bytes);
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

    /// Rewrites the main file with only the paths and arguments still in use, and empties the journal.
    pub fn compact(&mut self) -> Result<()> {
        self.journal = None;

        // Renumber what records still use
        let mut paths = Renumbering::default();
        let mut arguments = Renumbering::default();
        let argument_texts = &self.decode_arguments().texts;
        let mut body = Vec::new();
        for (output, record) in &self.records {
            put_u32(&mut body, paths.id(*output, &self.paths));
            put_record_body(&mut body, record, |id| paths.id(id, &self.paths), |id| arguments.id(id, argument_texts));
        }

        // Header, path table, argument table, records
        let generation = self.generation.wrapping_add(1);
        let mut bytes = MAGIC.to_vec();
        put_u64(&mut bytes, generation);
        put_strings(&mut bytes, &paths.kept);
        put_strings(&mut bytes, &arguments.kept);
        put_u32(&mut bytes, self.records.len() as u32);
        bytes.extend(body);

        // A journal a crash leaves behind here names the old generation, so it is never replayed against the new ids
        util::write_atomic(&self.file, &bytes)?;
        let _ = std::fs::remove_file(&self.journal_file);

        // Reload so ids match the new file
        self.clear();
        self.load()
    }

    fn clear(&mut self) {
        self.generation = 0;
        self.paths.clear();
        self.path_ids.clear();
        self.arguments = OnceCell::new();
        self.argument_source = ArgumentSource::default();
        self.records.clear();
        self.journal_records = 0;
    }

    fn load(&mut self) -> Result<()> {
        // Main file
        if let Ok(bytes) = std::fs::read(&self.file) {
            let mut reader = Reader { bytes: &bytes, position: 0 };
            let magic = reader.take(8)?;
            if magic != MAGIC {
                if !magic.starts_with(MAGIC_FAMILY) {
                    bail!("not an Anvil database");
                }

                // An older Anvil's: start over
                std::fs::remove_file(&self.file)?;
                let _ = std::fs::remove_file(&self.journal_file);
                return Ok(());
            }
            self.generation = reader.u64()?;

            for path in reader.strings()? {
                self.add_path(path);
            }
            // Arguments: only measured here (decoded when needed)
            let argument_count = reader.u32()?;
            let table_start = reader.position;
            for _ in 0..argument_count {
                let length = reader.u32()? as usize;
                reader.take(length)?;
            }
            self.argument_source.table = bytes[table_start..reader.position].to_vec();
            self.argument_source.count = argument_count;

            let record_count = reader.u32()?;
            for _ in 0..record_count {
                let output = reader.u32()?;
                let record = reader.record()?;
                self.records.insert(output, record);
            }
        }

        // Journal, if it extends this main file, up to the last complete entry
        if let Ok(mut bytes) = std::fs::read(&self.journal_file) {
            let mut reader = Reader { bytes: &bytes, position: 0 };
            let extends_main = reader.take(8).ok() == Some(JOURNAL_MAGIC.as_slice()) && reader.u64().ok() == Some(self.generation);
            if !extends_main {
                std::fs::remove_file(&self.journal_file)?;
                return self.validate();
            }

            while reader.position < bytes.len() {
                let start = reader.position;
                let entry = (|| -> Result<()> {
                    match reader.u8()? {
                        TAG_PATH => {
                            let path = reader.string()?;
                            self.add_path(path);
                        }
                        TAG_ARGUMENT => {
                            let argument = reader.string()?;
                            self.argument_source.journal.push(argument);
                            self.argument_source.count += 1;
                        }
                        TAG_RECORD => {
                            let output = reader.u32()?;
                            let record = reader.record()?;
                            self.records.insert(output, record);
                            self.journal_records += 1;
                        }
                        TAG_FORGET => {
                            let output = reader.u32()?;
                            self.records.remove(&output);
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

        self.validate()
    }

    /// Every record must name known paths and arguments.
    fn validate(&self) -> Result<()> {
        let path_count = self.paths.len() as u32;
        let argument_count = self.argument_source.count;
        let valid = self.records.iter().all(|(output, record)| {
            *output < path_count
                && record.inputs.iter().all(|stamp| stamp.path < path_count)
                && record.command.iter().all(|argument| *argument < argument_count)
        });
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

    /// The arguments, decoded from where they were loaded on first use. A damaged text only loses its spelling: a
    /// command is compared by its hash, its arguments only explain what changed.
    fn decode_arguments(&self) -> &Arguments {
        self.arguments.get_or_init(|| {
            let source = &self.argument_source;
            let mut arguments = Arguments {
                texts: Vec::with_capacity(source.count as usize),
                ids: HashMap::with_capacity(source.count as usize),
            };

            let mut reader = Reader {
                bytes: &source.table,
                position: 0,
            };
            while let Ok(length) = reader.u32() {
                let text = String::from_utf8_lossy(reader.take(length as usize).unwrap_or_default()).into_owned();
                arguments.add(text);
            }
            for text in &source.journal {
                arguments.add(text.clone());
            }

            arguments
        })
    }

    fn journal(&mut self) -> Option<&mut BufWriter<File>> {
        if self.journal.is_none() {
            let file = File::options().create(true).append(true).open(&self.journal_file).ok()?;
            let is_new = file.metadata().ok()?.len() == 0;
            let mut journal = BufWriter::new(file);

            // A new journal names the main file it extends
            if is_new {
                journal.write_all(JOURNAL_MAGIC).ok()?;
                journal.write_all(&self.generation.to_le_bytes()).ok()?;
            }
            self.journal = Some(journal);
        }
        self.journal.as_mut()
    }

    fn journal_entry_text(&mut self, tag: u8, text: &str) {
        let mut bytes = vec![tag];
        put_string(&mut bytes, text);

        if let Some(journal) = self.journal() {
            let _ = journal.write_all(&bytes);
        }
    }

    fn journal_entry_record(&mut self, output: u32, record: &Record) {
        let mut bytes = vec![TAG_RECORD];
        put_u32(&mut bytes, output);
        put_record_body(&mut bytes, record, |id| id, |id| id);
        self.journal_write(&bytes);
    }

    /// Appends a whole entry and flushes it, so entries reach the disk together.
    fn journal_write(&mut self, bytes: &[u8]) {
        if let Some(journal) = self.journal() {
            let _ = journal.write_all(bytes);
            let _ = journal.flush();
        }
        self.journal_records += 1;
    }
}

impl Arguments {
    fn add(&mut self, text: String) {
        let id = self.texts.len() as u32;
        self.ids.insert(text.clone(), id);
        self.texts.push(text);
    }
}

impl Renumbering {
    fn id(&mut self, old: u32, table: &[String]) -> u32 {
        *self.ids.entry(old).or_insert_with(|| {
            self.kept.push(table[old as usize].clone());
            (self.kept.len() - 1) as u32
        })
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

    fn strings(&mut self) -> Result<Vec<String>> {
        let count = self.u32()? as usize;
        (0..count).map(|_| self.string()).collect()
    }

    /// A count, then that many items (capped up front, so a damaged count cannot reserve gigabytes).
    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let count = self.u32()? as usize;
        let mut items = Vec::with_capacity(count.min(100_000));
        for _ in 0..count {
            items.push(item(self)?);
        }

        Ok(items)
    }

    fn record(&mut self) -> Result<Record> {
        let command_hash = self.u64()?;
        let command = self.list(|reader| reader.u32())?;
        let outputs = self.list(|reader| Ok(reader.u64()? as i64))?;
        let inputs = self.list(|reader| {
            Ok(Stamp {
                path: reader.u32()?,
                mtime: reader.u64()? as i64,
                hash: reader.u64()?,
            })
        })?;

        Ok(Record {
            command_hash,
            command,
            outputs,
            inputs,
        })
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

fn put_strings(bytes: &mut Vec<u8>, texts: &[String]) {
    put_u32(bytes, texts.len() as u32);
    for text in texts {
        put_string(bytes, text);
    }
}

fn put_record_body(bytes: &mut Vec<u8>, record: &Record, mut path_id: impl FnMut(u32) -> u32, mut argument_id: impl FnMut(u32) -> u32) {
    put_u64(bytes, record.command_hash);

    put_u32(bytes, record.command.len() as u32);
    for argument in &record.command {
        put_u32(bytes, argument_id(*argument));
    }

    put_u32(bytes, record.outputs.len() as u32);
    for mtime in &record.outputs {
        put_u64(bytes, *mtime as u64);
    }

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

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("anvil-db-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_record(database: &mut Database) -> Record {
        let input = database.intern("C:/src/a.cpp");
        let program = database.intern_argument("cl.exe");
        let define = database.intern_argument("/DFOO=1");

        Record {
            command_hash: 42,
            command: vec![program, define],
            outputs: vec![5],
            inputs: vec![Stamp {
                path: input,
                mtime: 7,
                hash: 9,
            }],
        }
    }

    #[test]
    fn survives_reopen_and_compaction() {
        let dir = test_dir("reopen");

        // Write through the journal
        {
            let mut database = Database::open(&dir).unwrap();
            let output = database.intern("C:/out/a.obj");
            let record = sample_record(&mut database);
            database.put(output, record);
            database.finish().unwrap();
        }

        // Read back, compact, read back again
        {
            let mut database = Database::open(&dir).unwrap();
            let output = database.intern("C:/out/a.obj");
            assert_eq!(database.record(output).unwrap().command_hash, 42);
            database.compact().unwrap();
        }
        let mut database = Database::open(&dir).unwrap();
        let output = database.intern("C:/out/a.obj");
        let record = database.record(output).unwrap().clone();
        assert_eq!(database.path(record.inputs[0].path), "C:/src/a.cpp");
        assert_eq!(database.argument(record.command[1]), "/DFOO=1");
        assert_eq!(record.inputs[0].hash, 9);
        assert_eq!(record.outputs, vec![5]);

        drop(database);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn forgets_records() {
        let dir = test_dir("forget");

        {
            let mut database = Database::open(&dir).unwrap();
            let output = database.intern("C:/out/a.obj");
            let record = sample_record(&mut database);
            database.put(output, record);
            database.forget(output);
            database.finish().unwrap();
        }

        let mut database = Database::open(&dir).unwrap();
        let output = database.intern("C:/out/a.obj");
        assert!(database.record(output).is_none());

        drop(database);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn starts_over_from_an_older_format() {
        let dir = test_dir("older");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("db"), b"ANVILDB2\0\0\0\0").unwrap();
        std::fs::write(dir.join("db.journal"), b"\x01\x02").unwrap();

        {
            let mut database = Database::open(&dir).unwrap();
            let output = database.intern("C:/out/a.obj");
            let record = sample_record(&mut database);
            database.put(output, record);
            database.finish().unwrap();
        }

        let mut database = Database::open(&dir).unwrap();
        let output = database.intern("C:/out/a.obj");
        assert!(database.record(output).is_some());

        drop(database);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn starts_clean_after_damage() {
        let dir = test_dir("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("db"), b"ANVILDB3\0\0\0\0\0\0\0\0\xff\xff\xff").unwrap();

        // The damaged file goes, so what this build records survives the next open
        {
            let mut database = Database::open(&dir).unwrap();
            let output = database.intern("C:/out/a.obj");
            let record = sample_record(&mut database);
            database.put(output, record);
            database.finish().unwrap();
        }

        let mut database = Database::open(&dir).unwrap();
        let output = database.intern("C:/out/a.obj");
        assert!(database.record(output).is_some());

        drop(database);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ignores_a_journal_of_another_generation() {
        let dir = test_dir("generation");

        // A compacted file, then a journal entry against it
        {
            let mut database = Database::open(&dir).unwrap();
            let output = database.intern("C:/out/a.obj");
            let record = sample_record(&mut database);
            database.put(output, record);
            database.compact().unwrap();

            let other = database.intern("C:/out/b.obj");
            let record = sample_record(&mut database);
            database.put(other, record);
            database.finish().unwrap();
        }
        let journal = std::fs::read(dir.join("db.journal")).unwrap();

        // Compacted again (renumbered), but the old journal survived the crash
        {
            let mut database = Database::open(&dir).unwrap();
            database.compact().unwrap();
        }
        std::fs::write(dir.join("db.journal"), journal).unwrap();

        let database = Database::open(&dir).unwrap();
        assert!(!dir.join("db.journal").exists());
        assert_eq!(database.records.len(), 2);

        drop(database);
        std::fs::remove_dir_all(&dir).ok();
    }
}

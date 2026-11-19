//! The on-disk search index: one document per session plus one per message,
//! kept up to date incrementally by [`Index::refresh`].
//!
//! Directory layout: `index/` (the Tantivy index), `state.json`
//! (`{schema_version, heartbeat}`), `writer.lock` (held while writing) and
//! `enrich.lock` (held for the whole of an enrichment run).

mod query;
mod refresh;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tantivy::collector::DocSetCollector;
use tantivy::directory::error::{OpenDirectoryError, OpenReadError};
use tantivy::query::{BooleanQuery, Query, TermQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};
use tantivy::{
    IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, TantivyError, Term,
};

use crate::enrich::Enrichment;
use crate::model::{Harness, SessionKey};
use crate::platform::Platform;

pub use query::{TermHit, TermMode, TextSearch};
pub use refresh::RefreshStats;

/// Bumped whenever the schema or stored layout changes; a mismatch rebuilds the index.
pub const SCHEMA_VERSION: u32 = 1;

/// Name of the analyzer used for message text: splits on non-alphanumeric
/// characters and lowercases, without stemming.
pub(crate) const TEXT_TOKENIZER: &str = "hound_text";
/// Name of the `session` field, also a fast field.
pub(crate) const SESSION_FIELD: &str = "session";
/// Name of the `subagent` field, also a fast field.
pub(crate) const SUBAGENT_FIELD: &str = "subagent";
/// Value of `kind` on session documents.
pub(crate) const KIND_SESSION: &str = "session";
/// Value of `kind` on message documents.
pub(crate) const KIND_MESSAGE: &str = "message";

const INDEX_DIR: &str = "index";
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = "writer.lock";
/// Held for the whole of an enrichment run, so runs do not overlap.
const ENRICH_LOCK_FILE: &str = "enrich.lock";
const WRITER_HEAP_BYTES: usize = 64 * 1024 * 1024;
const TITLE_CHARS: usize = 80;
const UNTITLED: &str = "(untitled)";
/// How long [`Index::open`] waits for another process holding `writer.lock`
/// when the index needs rebuilding.
const OPEN_WAIT: Duration = Duration::from_secs(1);
const OPEN_RETRY: Duration = Duration::from_millis(20);

/// How far one transcript file has been indexed.
#[derive(Clone, Debug, PartialEq)]
pub struct FileState {
    pub path: PathBuf,
    pub subagent: bool,
    /// File size when last indexed.
    pub size: u64,
    /// Byte offset just past the last line indexed.
    pub offset: u64,
    /// Hash of the first `min(size, 4096)` bytes.
    pub head_hash: u64,
}

/// One indexed session.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionDoc {
    pub key: SessionKey,
    pub title: String,
    pub cwds: Vec<String>,
    pub resume_cwd: String,
    /// Unix seconds.
    pub started_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
    pub summary: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    /// `files[0]` is the main transcript.
    pub files: Vec<FileState>,
    /// `total_bytes()` when the enrichment fields were last written.
    pub enriched_at_bytes: u64,
}

impl SessionDoc {
    /// Bytes indexed across all files.
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.offset).sum()
    }
}

/// The schema's fields.
///
/// Message documents carry `kind = "message"`, `session`, `role`
/// (`"user"`/`"assistant"`/`"tool"`), `seq`, `subagent`, `text` and, when the
/// harness recorded one, `ts`. `seq` orders a session's messages by file then
/// position: `(index of the file in SessionDoc::files) << 32 | position`, so the
/// main file's messages come first. Session documents carry
/// `kind = "session"`, `session` and `data` (the stored JSON of the session).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fields {
    pub kind: Field,
    pub session: Field,
    pub role: Field,
    pub seq: Field,
    pub subagent: Field,
    /// Indexed with positions using [`TEXT_TOKENIZER`], and stored.
    pub text: Field,
    pub ts: Field,
    pub data: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let text_options = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TEXT_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let fields = Fields {
        kind: b.add_text_field("kind", STRING),
        session: b.add_text_field(SESSION_FIELD, STRING | STORED | FAST),
        role: b.add_text_field("role", STRING | STORED),
        seq: b.add_u64_field("seq", INDEXED | STORED | FAST),
        subagent: b.add_bool_field(SUBAGENT_FIELD, INDEXED | STORED | FAST),
        text: b.add_text_field("text", text_options),
        ts: b.add_i64_field("ts", INDEXED | STORED | FAST),
        data: b.add_text_field("data", STORED),
    };
    (b.build(), fields)
}

fn text_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .build()
}

/// One transcript file as stored inside a session document.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoredFile {
    pub path: PathBuf,
    pub subagent: bool,
    pub size: u64,
    pub offset: u64,
    pub head_hash: u64,
    /// Messages indexed from this file so far; the next one's position.
    pub messages: u64,
}

/// Everything stored for a session, including what an incremental parse needs
/// to merge with: a parse from a later offset sees no earlier cwds, prompt or
/// title.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoredSession {
    pub harness: String,
    pub id: String,
    /// Latest title recorded by the harness in the main file.
    pub harness_title: Option<String>,
    /// First user prompt of the main file.
    pub first_prompt: Option<String>,
    pub cwds: Vec<String>,
    pub resume_cwd: String,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    /// Main file mtime (Unix seconds), used when no timestamps were recorded.
    pub main_mtime: i64,
    pub summary: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub enriched_at_bytes: u64,
    pub files: Vec<StoredFile>,
}

impl StoredSession {
    pub fn new(key: &SessionKey) -> Self {
        StoredSession {
            harness: key.harness.as_str().to_string(),
            id: key.id.clone(),
            harness_title: None,
            first_prompt: None,
            cwds: Vec::new(),
            resume_cwd: String::new(),
            first_ts: None,
            last_ts: None,
            main_mtime: 0,
            summary: None,
            category: None,
            tags: Vec::new(),
            enriched_at_bytes: 0,
            files: Vec::new(),
        }
    }

    /// A fresh session for `key` carrying over this one's enrichment fields.
    pub fn reset_keeping_enrichment(&self, key: &SessionKey) -> Self {
        StoredSession {
            summary: self.summary.clone(),
            category: self.category.clone(),
            tags: self.tags.clone(),
            enriched_at_bytes: self.enriched_at_bytes,
            ..StoredSession::new(key)
        }
    }

    pub fn key(&self) -> anyhow::Result<SessionKey> {
        let harness = Harness::parse(&self.harness)
            .with_context(|| format!("unknown harness {:?} in index", self.harness))?;
        Ok(SessionKey {
            harness,
            id: self.id.clone(),
        })
    }

    pub fn key_string(&self) -> String {
        format!("{}:{}", self.harness, self.id)
    }

    /// One line: whitespace runs collapse to a space, and the first-prompt
    /// fallback is cut to `TITLE_CHARS`.
    fn title(&self) -> String {
        if let Some(t) = &self.harness_title {
            one_line(t)
        } else if let Some(p) = &self.first_prompt {
            one_line(p).chars().take(TITLE_CHARS).collect()
        } else {
            UNTITLED.to_string()
        }
    }

    pub fn to_doc(&self) -> anyhow::Result<SessionDoc> {
        Ok(SessionDoc {
            key: self.key()?,
            title: self.title(),
            cwds: self.cwds.clone(),
            resume_cwd: self.resume_cwd.clone(),
            started_at: self.first_ts.unwrap_or(self.main_mtime),
            updated_at: self.last_ts.unwrap_or(self.main_mtime),
            summary: self.summary.clone(),
            category: self.category.clone(),
            tags: self.tags.clone(),
            files: self
                .files
                .iter()
                .map(|f| FileState {
                    path: f.path.clone(),
                    subagent: f.subagent,
                    size: f.size,
                    offset: f.offset,
                    head_hash: f.head_hash,
                })
                .collect(),
            enriched_at_bytes: self.enriched_at_bytes,
        })
    }
}

/// `s` with every whitespace run collapsed to one space, trimmed.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Serialize, Deserialize)]
struct State {
    schema_version: u32,
    /// Unix seconds of the last completed refresh; 0 if none.
    heartbeat: i64,
}

/// Holds `writer.lock`; the lock is released when the guard is dropped.
pub struct WriterGuard {
    _file: File,
}

/// Holds `enrich.lock`; the lock is released when the guard is dropped.
pub struct EnrichGuard {
    _file: File,
}

pub struct Index {
    dir: PathBuf,
    index: tantivy::Index,
    reader: IndexReader,
    fields: Fields,
}

impl Index {
    /// Opens the index in `dir`, creating it if needed.
    ///
    /// A missing or different schema version, or an index that cannot be
    /// decoded, is wiped and recreated, but only while holding `writer.lock`.
    /// If another process holds the lock, this waits up to `OPEN_WAIT` (1 s) for it
    /// to leave a usable index (e.g. a concurrent first open creating it), and
    /// otherwise fails without touching the directory. Other errors, such as
    /// running out of file descriptors, are returned and wipe nothing.
    ///
    /// If the old index cannot be deleted during a rebuild (on Windows, while
    /// another process has its files open), this fails saying so, leaving no
    /// `state.json`, so the next open rebuilds.
    pub fn open(dir: &Path) -> anyhow::Result<Index> {
        Index::open_with(dir, Platform::current(), &|path| fs::remove_dir_all(path))
    }

    /// [`Index::open`] on `platform`, deleting the old index directory with
    /// `remove_dir`.
    fn open_with(
        dir: &Path,
        platform: Platform,
        remove_dir: &dyn Fn(&Path) -> io::Result<()>,
    ) -> anyhow::Result<Index> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let (schema, fields) = build_schema();
        let index_dir = dir.join(INDEX_DIR);
        let deadline = Instant::now() + OPEN_WAIT;

        let (index, reader) = loop {
            if let Some(opened) = open_existing(dir, &schema)? {
                break opened;
            }
            if let Some(_guard) = lock_dir(dir) {
                // Another process may have rebuilt it before we got the lock.
                if let Some(opened) = open_existing(dir, &schema)? {
                    break opened;
                }
                break recreate(dir, &index_dir, schema, platform, remove_dir)?;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "index in {} needs rebuilding but another process holds {LOCK_FILE}",
                dir.display()
            );
            thread::sleep(OPEN_RETRY);
        };
        Ok(Index {
            dir: dir.to_path_buf(),
            index,
            reader,
            fields,
        })
    }

    /// Takes `writer.lock` without blocking; `None` if another holder has it.
    pub fn try_lock(&self) -> Option<WriterGuard> {
        lock_dir(&self.dir)
    }

    /// Takes `enrich.lock` without blocking; `None` if another enrichment run
    /// has it. Independent of `writer.lock`. An OS lock, so it is released when
    /// the holder exits, even by crashing.
    pub fn try_lock_enrich(&self) -> Option<EnrichGuard> {
        try_lock_file(&self.dir.join(ENRICH_LOCK_FILE)).map(|file| EnrichGuard { _file: file })
    }

    /// All indexed sessions, in no particular order.
    pub fn sessions(&self) -> anyhow::Result<Vec<SessionDoc>> {
        self.stored_sessions()?
            .iter()
            .map(StoredSession::to_doc)
            .collect()
    }

    pub fn session(&self, key: &SessionKey) -> anyhow::Result<Option<SessionDoc>> {
        self.stored_session(key)?.map(|s| s.to_doc()).transpose()
    }

    pub(crate) fn fields(&self) -> &Fields {
        &self.fields
    }

    /// A searcher over the latest commit, including commits by other processes.
    ///
    /// Reloading can fail while another process is committing (its garbage
    /// collection may delete segments between our reading `meta.json` and
    /// opening them). It is retried once; after that the last successfully
    /// loaded generation is used.
    pub(crate) fn searcher(&self) -> Searcher {
        self.reload();
        self.reader.searcher()
    }

    /// Reloads the reader, retrying once; on failure keeps the previous generation.
    fn reload(&self) {
        if self.reader.reload().is_err() {
            let _ = self.reader.reload();
        }
    }

    pub(crate) fn stored_sessions(&self) -> anyhow::Result<Vec<StoredSession>> {
        let query = TermQuery::new(
            Term::from_field_text(self.fields.kind, KIND_SESSION),
            IndexRecordOption::Basic,
        );
        self.load_sessions(&query)
    }

    pub(crate) fn stored_session(&self, key: &SessionKey) -> anyhow::Result<Option<StoredSession>> {
        let query = self.session_doc_query(&key.as_string());
        Ok(self.load_sessions(&query)?.into_iter().next())
    }

    fn load_sessions(&self, query: &dyn Query) -> anyhow::Result<Vec<StoredSession>> {
        let searcher = self.searcher();
        let addrs = searcher.search(query, &DocSetCollector)?;
        let mut out = Vec::with_capacity(addrs.len());
        for addr in addrs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let data = doc
                .get_first(self.fields.data)
                .and_then(|v| v.as_str())
                .context("session document without data")?;
            out.push(serde_json::from_str(data).context("decoding session document")?);
        }
        Ok(out)
    }

    /// Matches the session document (not the messages) of the session `key`.
    fn session_doc_query(&self, key: &str) -> BooleanQuery {
        BooleanQuery::intersection(vec![
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.kind, KIND_SESSION),
                IndexRecordOption::Basic,
            )),
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.session, key),
                IndexRecordOption::Basic,
            )),
        ])
    }

    /// A writer; the caller must hold the [`WriterGuard`].
    pub(crate) fn writer(&self) -> anyhow::Result<IndexWriter> {
        Ok(self.index.writer(WRITER_HEAP_BYTES)?)
    }

    /// Replaces the session document of `session`, leaving its messages alone.
    pub(crate) fn replace_session_doc(
        &self,
        writer: &IndexWriter,
        session: &StoredSession,
    ) -> anyhow::Result<()> {
        writer.delete_query(Box::new(self.session_doc_query(&session.key_string())))?;
        self.add_session_doc(writer, session)
    }

    /// Adds a session document; any existing one must already be deleted.
    fn add_session_doc(&self, writer: &IndexWriter, session: &StoredSession) -> anyhow::Result<()> {
        let mut doc = TantivyDocument::default();
        doc.add_text(self.fields.kind, KIND_SESSION);
        doc.add_text(self.fields.session, session.key_string());
        doc.add_text(self.fields.data, serde_json::to_string(session)?);
        writer.add_document(doc)?;
        Ok(())
    }

    /// Deletes the session document and every message of the session `key`.
    fn delete_session(&self, writer: &IndexWriter, key: &str) {
        writer.delete_term(Term::from_field_text(self.fields.session, key));
    }

    /// Commits `writer` and, if reloading succeeds, makes the commit visible to
    /// this index's reader (later `searcher()` calls retry the reload).
    pub(crate) fn commit(&self, writer: &mut IndexWriter) -> anyhow::Result<()> {
        writer.commit()?;
        self.reload();
        Ok(())
    }

    /// Stores `e` on the session `key`, with `enriched_at_bytes = at_bytes`,
    /// leaving its messages alone. Returns `false`, doing nothing, if another
    /// writer holds `writer.lock`; fails if the session is not indexed.
    pub fn set_enrichment(
        &self,
        key: &SessionKey,
        e: &Enrichment,
        at_bytes: u64,
    ) -> anyhow::Result<bool> {
        let Some(_guard) = self.try_lock() else {
            return Ok(false);
        };
        // Read under the lock so a concurrent refresh's changes are kept.
        let mut session = self
            .stored_session(key)?
            .with_context(|| format!("session {} is not indexed", key.as_string()))?;
        session.summary = Some(e.summary.clone());
        session.category = Some(e.category.clone());
        session.tags = e.tags.clone();
        session.enriched_at_bytes = at_bytes;
        let mut writer = self.writer()?;
        self.replace_session_doc(&writer, &session)?;
        self.commit(&mut writer)?;
        Ok(true)
    }

    /// Seconds from the last completed refresh to `now` (negative if the
    /// heartbeat is in the future); `None` if no refresh has completed or
    /// `state.json` cannot be read.
    pub fn heartbeat_age(&self, now: i64) -> Option<i64> {
        let state = read_state(&self.dir).ok().flatten()?;
        (state.heartbeat > 0).then(|| now - state.heartbeat)
    }

    /// Records the time of the last completed refresh in `state.json`.
    fn write_heartbeat(&self, now: i64) -> anyhow::Result<()> {
        write_state(
            &self.dir,
            &State {
                schema_version: SCHEMA_VERSION,
                heartbeat: now,
            },
        )
    }
}

/// The index in `dir` if `state.json` has the current schema version and the
/// index decodes with the current schema; `None` if it must be rebuilt. Errors
/// that say nothing about the index's contents (permissions, file descriptors,
/// ...) are returned instead.
fn open_existing(
    dir: &Path,
    schema: &Schema,
) -> anyhow::Result<Option<(tantivy::Index, IndexReader)>> {
    if read_state(dir)?.is_none_or(|s| s.schema_version != SCHEMA_VERSION) {
        return Ok(None);
    }
    let index = match tantivy::Index::open_in_dir(dir.join(INDEX_DIR)) {
        Ok(index) => index,
        Err(e) if needs_rebuild(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if &index.schema() != schema {
        return Ok(None);
    }
    match with_reader(index) {
        Ok(opened) => Ok(Some(opened)),
        Err(e) if needs_rebuild(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Whether `e` means the index is missing or cannot be decoded.
fn needs_rebuild(e: &TantivyError) -> bool {
    matches!(
        e,
        TantivyError::DataCorruption(_)
            | TantivyError::SchemaError(_)
            | TantivyError::OpenReadError(
                OpenReadError::FileDoesNotExist(_) | OpenReadError::IncompatibleIndex(_)
            )
            | TantivyError::OpenDirectoryError(
                OpenDirectoryError::DoesNotExist(_) | OpenDirectoryError::NotADirectory(_)
            )
    )
}

/// Wipes (with `remove_dir`) and recreates the index; the caller must hold
/// `writer.lock`. `state.json` is removed first and written last, so a failure
/// at any step leaves an index the next open rebuilds.
fn recreate(
    dir: &Path,
    index_dir: &Path,
    schema: Schema,
    platform: Platform,
    remove_dir: &dyn Fn(&Path) -> io::Result<()>,
) -> anyhow::Result<(tantivy::Index, IndexReader)> {
    // Without a state file, concurrent opens wait for the lock instead of
    // reading the index while it is being replaced.
    match fs::remove_file(dir.join(STATE_FILE)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    if index_dir.exists() {
        remove_dir(index_dir).map_err(|e| removal_error(platform, e, index_dir))?;
    }
    fs::create_dir_all(index_dir)?;
    let opened = with_reader(tantivy::Index::create_in_dir(index_dir, schema)?)?;
    write_state(
        dir,
        &State {
            schema_version: SCHEMA_VERSION,
            heartbeat: 0,
        },
    )?;
    Ok(opened)
}

/// The error for failing to remove the index directory `path` with `e`. On
/// Windows, access denied, sharing violation and lock violation mean another
/// process has the index's files open, and say so; anything else is reported
/// as a failure removing `path`.
fn removal_error(platform: Platform, e: io::Error, path: &Path) -> anyhow::Error {
    /// ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION.
    const IN_USE_CODES: [i32; 3] = [5, 32, 33];
    let in_use = platform == Platform::Windows
        && (e.kind() == io::ErrorKind::PermissionDenied
            || e.raw_os_error().is_some_and(|c| IN_USE_CODES.contains(&c)));
    if in_use {
        anyhow::anyhow!(
            "cannot rebuild the index in {}: another hound process is using the index; \
             close it and retry ({e})",
            path.display()
        )
    } else {
        anyhow::Error::new(e).context(format!("removing {}", path.display()))
    }
}

fn with_reader(index: tantivy::Index) -> tantivy::Result<(tantivy::Index, IndexReader)> {
    index.tokenizers().register(TEXT_TOKENIZER, text_analyzer());
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    Ok((index, reader))
}

/// Takes `<dir>/writer.lock` without blocking.
fn lock_dir(dir: &Path) -> Option<WriterGuard> {
    try_lock_file(&dir.join(LOCK_FILE)).map(|file| WriterGuard { _file: file })
}

/// Opens (creating if needed) and exclusively locks `path` without blocking.
fn try_lock_file(path: &Path) -> Option<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// `state.json`, or `None` if it is missing or does not parse.
fn read_state(dir: &Path) -> anyhow::Result<Option<State>> {
    let path = dir.join(STATE_FILE);
    match fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Replaces `state.json` atomically, through a temporary file unique to this
/// call so concurrent writers do not collide.
fn write_state(dir: &Path, state: &State) -> anyhow::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{STATE_FILE}.{}.{n}.tmp", process::id()));
    let result = (|| -> anyhow::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(state)?)?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join(STATE_FILE))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // A unit test rather than one under `tests/`, because the injection point
    // (`Index::open_with`) is private.
    #[test]
    fn rebuild_reports_index_in_use_when_removal_fails() {
        let dir = tempfile::tempdir().unwrap();
        drop(Index::open(dir.path()).unwrap());
        // A stale schema version forces a rebuild.
        fs::write(
            dir.path().join(STATE_FILE),
            r#"{"schema_version":0,"heartbeat":0}"#,
        )
        .unwrap();

        let in_use = |_: &Path| -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "file in use by another process",
            ))
        };
        let err = match Index::open_with(dir.path(), Platform::Windows, &in_use) {
            Ok(_) => panic!("open succeeded although removal failed"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("another hound process is using the index"),
            "{err}"
        );
        assert!(err.contains("close it and retry"), "{err}");
        assert!(!dir.path().join(STATE_FILE).exists());

        // The next open, once removal works, rebuilds.
        Index::open(dir.path()).unwrap();
        let state = read_state(dir.path()).unwrap().unwrap();
        assert_eq!(state.schema_version, SCHEMA_VERSION);
    }

    fn removal_text(platform: Platform, e: io::Error) -> String {
        format!(
            "{:#}",
            removal_error(platform, e, Path::new("/state/index"))
        )
    }

    const IN_USE: &str = "another hound process is using the index";

    #[test]
    fn windows_permission_denied_is_index_in_use() {
        let text = removal_text(
            Platform::Windows,
            io::Error::new(io::ErrorKind::PermissionDenied, "access denied"),
        );
        assert!(text.contains(IN_USE), "{text}");
        assert!(text.contains("close it and retry"), "{text}");
        assert!(text.contains("access denied"), "{text}");
    }

    #[test]
    fn windows_sharing_violation_is_index_in_use() {
        for code in [5, 32, 33] {
            let text = removal_text(Platform::Windows, io::Error::from_raw_os_error(code));
            assert!(text.contains(IN_USE), "{code}: {text}");
            assert!(text.contains("close it and retry"), "{code}: {text}");
        }
    }

    #[test]
    fn windows_other_errors_say_removing() {
        let text = removal_text(
            Platform::Windows,
            io::Error::new(io::ErrorKind::NotFound, "gone"),
        );
        assert!(text.starts_with("removing /state/index: gone"), "{text}");
        assert!(!text.contains(IN_USE), "{text}");
    }

    #[test]
    fn unix_permission_denied_says_removing() {
        let text = removal_text(
            Platform::Unix,
            io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        );
        assert_eq!(text, "removing /state/index: denied");
    }
}

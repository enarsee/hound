//! Incremental refresh: bring the index in line with what the adapters discover.

use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use tantivy::{IndexWriter, TantivyDocument};

use super::{Index, StoredFile, StoredSession};
use crate::adapters::{Adapter, RootUnavailable};
use crate::model::{Message, Role, SessionFiles, SourceFile};

/// Bytes at the start of a file covered by its head hash.
const HEAD_BYTES: u64 = 4096;
/// A refresh commits after this many changed sessions, so its progress
/// survives the process ending part-way.
const BATCH_SESSIONS: usize = 20;

/// What one refresh did, counted per session.
#[derive(Debug, Default, PartialEq)]
pub struct RefreshStats {
    pub added: usize,
    /// Sessions with new content, including those fully reindexed.
    pub updated: usize,
    pub removed: usize,
    /// Sessions whose files were rewritten or truncated, reparsed from scratch.
    pub full_reindex: usize,
    pub unchanged: usize,
    /// Roots that could not be read; their harnesses' sessions were left as indexed.
    pub unavailable: Vec<RootUnavailable>,
}

/// A message to index, with its position in the session.
struct NewMessage {
    seq: u64,
    message: Message,
}

/// What to do with one discovered session.
enum Change {
    Unchanged,
    /// Messages appended; the session's existing messages stay.
    Append(StoredSession, Vec<NewMessage>),
    /// Everything reparsed; the session's existing messages go.
    Rebuild(StoredSession, Vec<NewMessage>),
}

impl Index {
    /// [`Index::refresh_notifying`] without commit notifications.
    pub fn refresh(
        &self,
        adapters: &[Box<dyn Adapter>],
        progress: &mut dyn FnMut(usize, usize),
    ) -> anyhow::Result<Option<RefreshStats>> {
        self.refresh_notifying(adapters, progress, &mut || {})
    }

    /// Indexes new and changed sessions and drops vanished ones. Returns
    /// `None`, doing nothing, if another writer holds `writer.lock`.
    /// `progress(done, total)` is called after each discovered session.
    ///
    /// Changes are committed after every 20 changed sessions and at the end,
    /// where the removal of vanished sessions is committed too; `committed()`
    /// is called after each commit. A session's document and its messages are
    /// always in the same commit.
    ///
    /// A session whose files cannot be read is skipped and left as indexed; the
    /// next refresh retries it. An adapter whose root cannot be read is listed
    /// in [`RefreshStats::unavailable`] and its harness's sessions are kept.
    pub fn refresh_notifying(
        &self,
        adapters: &[Box<dyn Adapter>],
        progress: &mut dyn FnMut(usize, usize),
        committed: &mut dyn FnMut(),
    ) -> anyhow::Result<Option<RefreshStats>> {
        let Some(_guard) = self.try_lock() else {
            return Ok(None);
        };

        let mut existing: HashMap<String, StoredSession> = self
            .stored_sessions()?
            .into_iter()
            .map(|s| (s.key_string(), s))
            .collect();
        let mut stats = RefreshStats::default();
        let mut discovered: Vec<(&dyn Adapter, SessionFiles)> = Vec::new();
        for adapter in adapters {
            match adapter.try_discover() {
                Ok(found) => discovered.extend(found.into_iter().map(|s| (adapter.as_ref(), s))),
                Err(unavailable) => {
                    // Nothing is known about this harness's sessions: keep them.
                    let harness = adapter.harness().as_str();
                    existing.retain(|_, s| s.harness != harness);
                    stats.unavailable.push(unavailable);
                }
            }
        }

        let mut writer: Option<IndexWriter> = None;
        // Changed sessions written since the last commit.
        let mut pending = 0;
        let total = discovered.len();

        for (done, (adapter, files)) in discovered.iter().enumerate() {
            let key = files.key.as_string();
            // Whatever is left in `existing` afterwards was not discovered.
            let old = existing.remove(&key);
            let is_new = old.is_none();
            let change = match &old {
                None => rebuild(*adapter, files, StoredSession::new(&files.key))
                    .map(|(s, m)| Change::Rebuild(s, m)),
                Some(old) => update(*adapter, files, old),
            };
            match &change {
                Err(_) => {}
                Ok(_) if is_new => stats.added += 1,
                Ok(Change::Unchanged) => stats.unchanged += 1,
                Ok(Change::Append(..)) => stats.updated += 1,
                Ok(Change::Rebuild(..)) => {
                    stats.updated += 1;
                    stats.full_reindex += 1;
                }
            }
            match change {
                Ok(Change::Unchanged) | Err(_) => {}
                Ok(Change::Append(session, messages)) => {
                    let w = self.open_writer(&mut writer)?;
                    self.replace_session_doc(w, &session)?;
                    self.add_messages(w, &key, messages)?;
                    pending += 1;
                }
                Ok(Change::Rebuild(session, messages)) => {
                    let w = self.open_writer(&mut writer)?;
                    self.delete_session(w, &key);
                    self.add_session_doc(w, &session)?;
                    self.add_messages(w, &key, messages)?;
                    pending += 1;
                }
            }
            if pending == BATCH_SESSIONS {
                self.commit_batch(&mut writer, committed)?;
                pending = 0;
            }
            progress(done + 1, total);
        }

        for key in existing.keys() {
            let w = self.open_writer(&mut writer)?;
            self.delete_session(w, key);
            stats.removed += 1;
        }

        if pending > 0 || stats.removed > 0 {
            self.commit_batch(&mut writer, committed)?;
        }
        // The refresh has been committed; a missed heartbeat is not a failure.
        let _ = self.write_heartbeat(unix_secs(SystemTime::now()));
        Ok(Some(stats))
    }

    /// Commits what `writer` holds, if it was opened, and reports it.
    fn commit_batch(
        &self,
        writer: &mut Option<IndexWriter>,
        committed: &mut dyn FnMut(),
    ) -> anyhow::Result<()> {
        if let Some(w) = writer.as_mut() {
            self.commit(w)?;
            committed();
        }
        Ok(())
    }

    fn open_writer<'a>(
        &self,
        writer: &'a mut Option<IndexWriter>,
    ) -> anyhow::Result<&'a IndexWriter> {
        if writer.is_none() {
            *writer = Some(self.writer()?);
        }
        Ok(writer.as_ref().expect("writer just opened"))
    }

    fn add_messages(
        &self,
        writer: &IndexWriter,
        key: &str,
        messages: Vec<NewMessage>,
    ) -> anyhow::Result<()> {
        let f = self.fields();
        for NewMessage { seq, message } in messages {
            let mut doc = TantivyDocument::default();
            doc.add_text(f.kind, super::KIND_MESSAGE);
            doc.add_text(f.session, key);
            doc.add_text(f.role, role_str(message.role));
            doc.add_u64(f.seq, seq);
            doc.add_bool(f.subagent, message.subagent);
            doc.add_text(f.text, &message.text);
            if let Some(ts) = message.ts {
                doc.add_i64(f.ts, ts);
            }
            writer.add_document(doc)?;
        }
        Ok(())
    }
}

pub(crate) fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Parses every file of `files` from the start into `base`.
fn rebuild(
    adapter: &dyn Adapter,
    files: &SessionFiles,
    mut base: StoredSession,
) -> io::Result<(StoredSession, Vec<NewMessage>)> {
    let mut messages = Vec::new();
    for file in &files.files {
        let meta = fs::metadata(&file.path)?;
        let idx = push_file(&mut base, file);
        parse_into(adapter, file, &meta, &mut base, idx, &mut messages)?;
    }
    Ok((base, messages))
}

/// Compares `files` with what `old` recorded and parses what is new.
fn update(adapter: &dyn Adapter, files: &SessionFiles, old: &StoredSession) -> io::Result<Change> {
    let full = || {
        rebuild(adapter, files, old.reset_keeping_enrichment(&files.key))
            .map(|(s, m)| Change::Rebuild(s, m))
    };
    // A file that is no longer part of the session leaves stale messages behind.
    if old
        .files
        .iter()
        .any(|f| !files.files.iter().any(|d| d.path == f.path))
    {
        return full();
    }

    let mut session = old.clone();
    let mut messages = Vec::new();
    let mut changed = false;
    for file in &files.files {
        let meta = fs::metadata(&file.path)?;
        let size = meta.len();
        let idx = match session.files.iter().position(|f| f.path == file.path) {
            Some(idx) => {
                let known = &session.files[idx];
                if size == known.size {
                    continue;
                }
                if size < known.size
                    || head_hash(&file.path, known.size.min(HEAD_BYTES))? != known.head_hash
                {
                    return full();
                }
                idx
            }
            None => push_file(&mut session, file),
        };
        parse_into(adapter, file, &meta, &mut session, idx, &mut messages)?;
        changed = true;
    }
    Ok(if changed {
        Change::Append(session, messages)
    } else {
        Change::Unchanged
    })
}

/// Appends an unindexed entry for `file` and returns its index.
fn push_file(session: &mut StoredSession, file: &SourceFile) -> usize {
    session.files.push(StoredFile {
        path: file.path.clone(),
        subagent: file.subagent,
        size: 0,
        offset: 0,
        head_hash: 0,
        messages: 0,
    });
    session.files.len() - 1
}

/// Parses `file` from its stored offset and merges the result into `session`.
fn parse_into(
    adapter: &dyn Adapter,
    file: &SourceFile,
    meta: &Metadata,
    session: &mut StoredSession,
    idx: usize,
    out: &mut Vec<NewMessage>,
) -> io::Result<()> {
    let parsed = adapter.parse(file, session.files[idx].offset)?;
    // The file may have grown since `meta` was taken.
    let size = meta.len().max(parsed.new_offset);
    let hash = head_hash(&file.path, size.min(HEAD_BYTES))?;

    if !file.subagent {
        if parsed.title.is_some() {
            session.harness_title = parsed.title;
        }
        if session.first_prompt.is_none() {
            session.first_prompt = parsed.first_prompt;
        }
        if session.resume_cwd.is_empty()
            && let Some(cwd) = parsed.cwds.first()
        {
            session.resume_cwd = cwd.clone();
        }
        if let Ok(mtime) = meta.modified() {
            session.main_mtime = unix_secs(mtime);
        }
    }
    for cwd in parsed.cwds {
        if !session.cwds.contains(&cwd) {
            session.cwds.push(cwd);
        }
    }
    if let Some(t) = parsed.first_ts {
        session.first_ts = Some(session.first_ts.map_or(t, |f| f.min(t)));
    }
    if let Some(t) = parsed.last_ts {
        session.last_ts = Some(session.last_ts.map_or(t, |l| l.max(t)));
    }

    let state = &mut session.files[idx];
    for message in parsed.messages {
        out.push(NewMessage {
            seq: ((idx as u64) << 32) | state.messages,
            message,
        });
        state.messages += 1;
    }
    state.size = size;
    state.offset = parsed.new_offset;
    state.head_hash = hash;
    Ok(())
}

/// FNV-1a over the first `len` bytes of `path`; stable across builds since it
/// is stored in the index.
fn head_hash(path: &Path, len: u64) -> io::Result<u64> {
    let mut buf = Vec::with_capacity(len as usize);
    File::open(path)?.take(len).read_to_end(&mut buf)?;
    Ok(buf.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    }))
}

fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::PathBuf;

    use serde_json::json;
    use tantivy::collector::DocSetCollector;
    use tantivy::query::{QueryParser, TermQuery};
    use tantivy::schema::{IndexRecordOption, Value};
    use tantivy::{TantivyDocument, Term};

    use super::*;
    use crate::adapters::claude::ClaudeAdapter;
    use crate::model::{Harness, SessionKey};

    const S: &str = "11111111-1111-1111-1111-111111111111";

    fn user(text: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": text},
               "timestamp": "2026-11-01T10:00:00Z", "cwd": "/p"})
        .to_string()
    }

    fn assistant(text: &str) -> String {
        json!({"type": "assistant", "timestamp": "2026-11-01T10:05:00Z",
               "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}})
        .to_string()
    }

    fn write(path: &Path, lines: &[String]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
    }

    fn append(path: &Path, line: &str) {
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    struct Setup {
        _dir: tempfile::TempDir,
        ix: Index,
        adapters: Vec<Box<dyn Adapter>>,
        main: PathBuf,
        sub: PathBuf,
        key: SessionKey,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        let main = root.join("-p").join(format!("{S}.jsonl"));
        let sub = root
            .join("-p")
            .join(S)
            .join("subagents")
            .join("agent-a.jsonl");
        write(
            &main,
            &[user("Fix the Depot-alert timer"), assistant("done")],
        );
        let ix = Index::open(&dir.path().join("state")).unwrap();
        let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(ClaudeAdapter::new(root, true))];
        let key = SessionKey {
            harness: Harness::Claude,
            id: S.to_string(),
        };
        ix.refresh(&adapters, &mut |_, _| {}).unwrap().unwrap();
        Setup {
            _dir: dir,
            ix,
            adapters,
            main,
            sub,
            key,
        }
    }

    impl Setup {
        fn refresh(&self) -> RefreshStats {
            self.ix
                .refresh(&self.adapters, &mut |_, _| {})
                .unwrap()
                .unwrap()
        }

        /// `(seq, role, subagent, text)` of the session's messages, by `seq`.
        fn messages(&self) -> Vec<(u64, String, bool, String)> {
            let f = self.ix.fields();
            let searcher = self.ix.searcher();
            let q = TermQuery::new(
                Term::from_field_text(f.session, &self.key.as_string()),
                IndexRecordOption::Basic,
            );
            let mut out: Vec<_> = searcher
                .search(&q, &DocSetCollector)
                .unwrap()
                .into_iter()
                .map(|a| searcher.doc::<TantivyDocument>(a).unwrap())
                .filter(|d| d.get_first(f.text).is_some())
                .map(|d| {
                    (
                        d.get_first(f.seq).unwrap().as_u64().unwrap(),
                        d.get_first(f.role).unwrap().as_str().unwrap().to_string(),
                        d.get_first(f.subagent).unwrap().as_bool().unwrap(),
                        d.get_first(f.text).unwrap().as_str().unwrap().to_string(),
                    )
                })
                .collect();
            out.sort();
            out
        }

        fn set_summary(&self, summary: &str) {
            let _guard = self.ix.try_lock().unwrap();
            let mut s = self.ix.stored_session(&self.key).unwrap().unwrap();
            s.summary = Some(summary.to_string());
            s.tags = vec!["alerts".to_string()];
            s.enriched_at_bytes = 7;
            let mut w = self.ix.writer().unwrap();
            self.ix.replace_session_doc(&w, &s).unwrap();
            self.ix.commit(&mut w).unwrap();
        }
    }

    fn texts(m: &[(u64, String, bool, String)]) -> Vec<&str> {
        m.iter().map(|m| m.3.as_str()).collect()
    }

    #[test]
    fn messages_indexed_in_file_order_main_first() {
        let s = setup();
        write(&s.sub, &[assistant("sub one")]);
        append(&s.main, &assistant("main later"));
        s.refresh();
        let m = s.messages();
        assert_eq!(
            texts(&m),
            vec!["Fix the Depot-alert timer", "done", "main later", "sub one"]
        );
        assert_eq!((m[0].1.as_str(), m[0].2), ("user", false));
        assert_eq!((m[3].1.as_str(), m[3].2), ("assistant", true));
    }

    #[test]
    fn full_reindex_replaces_messages() {
        let s = setup();
        write(
            &s.main,
            &[user("a different start entirely"), assistant("ok")],
        );
        assert_eq!(s.refresh().full_reindex, 1);
        assert_eq!(
            texts(&s.messages()),
            vec!["a different start entirely", "ok"]
        );
    }

    #[test]
    fn text_is_lowercased_and_split_without_stemming() {
        let s = setup();
        let f = s.ix.fields();
        let parser = QueryParser::for_index(&s.ix.index, vec![f.text]);
        let searcher = s.ix.searcher();
        let hits = |q: &str| {
            searcher
                .search(&parser.parse_query(q).unwrap(), &DocSetCollector)
                .unwrap()
                .len()
        };
        assert_eq!(hits("depot"), 1);
        assert_eq!(hits("\"depot alert\""), 1);
        assert_eq!(hits("alerts"), 0);
        assert_eq!(hits("timers"), 0);
    }

    #[test]
    fn enrichment_survives_append_and_full_reindex() {
        let s = setup();
        s.set_summary("Fixes the alert");
        append(&s.main, &assistant("more"));
        assert_eq!(s.refresh().updated, 1);
        let d = s.ix.session(&s.key).unwrap().unwrap();
        assert_eq!(d.summary.as_deref(), Some("Fixes the alert"));
        assert_eq!(
            (d.tags.as_slice(), d.enriched_at_bytes),
            (&["alerts".to_string()][..], 7)
        );

        write(&s.main, &[user("rewritten from the very top")]);
        assert_eq!(s.refresh().full_reindex, 1);
        let d = s.ix.session(&s.key).unwrap().unwrap();
        assert_eq!(d.summary.as_deref(), Some("Fixes the alert"));
        assert_eq!(d.enriched_at_bytes, 7);
        assert_eq!(s.ix.sessions().unwrap().len(), 1);
    }
}

//! The JSON a hound service and its clients exchange. Clients ignore fields
//! they do not know.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::adapters::valid_session_id;
use crate::index::{FileState, SessionDoc, TermMode};
use crate::model::{Harness, SessionKey};

/// Every response body: the protocol version and the server's clock (Unix
/// seconds) next to the body's own fields.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Envelope<T> {
    pub protocol: u32,
    pub now: i64,
    #[serde(flatten)]
    pub body: T,
}

/// The body of a 400 or 500 response: a fixed reason that never echoes input.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ErrorBody {
    pub error: String,
}

/// `GET /v1/info`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Info {
    pub hound_version: String,
    pub host: String,
    pub home: String,
    pub sessions: usize,
}

/// One session's metadata. No conversation text beyond the title, summary and
/// tags.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireSession {
    pub harness: String,
    pub id: String,
    /// The harness-recorded title or, for a session without one, its first
    /// prompt collapsed to one line and cut to 80 characters; the only
    /// conversation text here besides the stored summary and tags.
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
    /// The main transcript's path on the serving machine.
    pub main_file: Option<String>,
    /// Unix seconds; see [`SessionFacts`].
    pub last_change: i64,
    pub cwd_exists: bool,
}

/// `GET /v1/sessions`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Sessions {
    pub sessions: Vec<WireSession>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum WireMode {
    Whole,
    Prefix,
}

impl From<WireMode> for TermMode {
    fn from(mode: WireMode) -> TermMode {
        match mode {
            WireMode::Whole => TermMode::Whole,
            WireMode::Prefix => TermMode::Prefix,
        }
    }
}

impl From<TermMode> for WireMode {
    fn from(mode: TermMode) -> WireMode {
        match mode {
            TermMode::Whole => WireMode::Whole,
            TermMode::Prefix => WireMode::Prefix,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TermSpec {
    pub term: String,
    pub mode: WireMode,
}

/// `POST /v1/terms` request body.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TermsRequest {
    pub terms: Vec<TermSpec>,
    pub include_subagents: bool,
}

/// One session's best match for a term.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireMatch {
    pub harness: String,
    pub id: String,
    pub score: f32,
    pub snippet: String,
    pub from_subagent: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TermMatches {
    pub term: String,
    pub matches: Vec<WireMatch>,
}

/// `POST /v1/terms` response: one entry per requested term, in request order.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TermsResponse {
    pub terms: Vec<TermMatches>,
}

/// What resuming a session needs to know from the filesystem it lives on;
/// also the body of `GET /v1/session`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SessionFacts {
    /// Unix seconds: the later of `updated_at` and the main file's
    /// modification time.
    pub last_change: i64,
    /// `resume_cwd` is non-empty and a directory.
    pub cwd_exists: bool,
}

/// Reads the session's facts fresh from the filesystem. The main file's
/// modification time is fresher than the index; `updated_at` alone is used if
/// the file cannot be read.
pub fn session_facts(doc: &SessionDoc) -> SessionFacts {
    let mtime = doc
        .files
        .first()
        .and_then(|f| fs::metadata(&f.path).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    let cwd = &doc.resume_cwd;
    SessionFacts {
        last_change: mtime.map_or(doc.updated_at, |m| m.max(doc.updated_at)),
        cwd_exists: !cwd.is_empty() && Path::new(cwd).is_dir(),
    }
}

impl WireSession {
    /// The session's metadata with facts read fresh from the filesystem.
    pub fn from_doc(doc: &SessionDoc) -> WireSession {
        let facts = session_facts(doc);
        WireSession {
            harness: doc.key.harness.as_str().to_string(),
            id: doc.key.id.clone(),
            title: doc.title.clone(),
            cwds: doc.cwds.clone(),
            resume_cwd: doc.resume_cwd.clone(),
            started_at: doc.started_at,
            updated_at: doc.updated_at,
            summary: doc.summary.clone(),
            category: doc.category.clone(),
            tags: doc.tags.clone(),
            main_file: doc
                .files
                .first()
                .map(|f| f.path.to_string_lossy().into_owned()),
            last_change: facts.last_change,
            cwd_exists: facts.cwd_exists,
        }
    }

    /// A [`SessionDoc`] standing in for the remote session, and its facts.
    /// `files` holds only the main file (with zero size, offset and hash), or
    /// nothing; `None` for a harness this build does not know or an id that
    /// is not [valid](valid_session_id).
    ///
    /// Strings from a remote are untrusted. The id is validated here, as it
    /// ends up on a harness command line; everything else (title, summary,
    /// tags, paths) must be sanitised before it reaches a terminal or a
    /// command line.
    pub fn into_doc(self) -> Option<(SessionDoc, SessionFacts)> {
        let harness = Harness::parse(&self.harness)?;
        if !valid_session_id(&self.id) {
            return None;
        }
        let files = self
            .main_file
            .map(|path| FileState {
                path: PathBuf::from(path),
                subagent: false,
                size: 0,
                offset: 0,
                head_hash: 0,
            })
            .into_iter()
            .collect();
        let doc = SessionDoc {
            key: SessionKey {
                harness,
                id: self.id,
            },
            title: self.title,
            cwds: self.cwds,
            resume_cwd: self.resume_cwd,
            started_at: self.started_at,
            updated_at: self.updated_at,
            summary: self.summary,
            category: self.category,
            tags: self.tags,
            files,
            enriched_at_bytes: 0,
        };
        let facts = SessionFacts {
            last_change: self.last_change,
            cwd_exists: self.cwd_exists,
        };
        Some((doc, facts))
    }
}

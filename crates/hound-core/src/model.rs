//! Core data types shared by the harness readers, the index and the frontends.

use std::path::PathBuf;

/// An agent harness whose sessions hound can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Harness {
    Claude,
    Pi,
    Opencode,
}

impl Harness {
    /// The harness executable name; hosts use it as the agent kind.
    pub fn as_str(&self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Pi => "pi",
            Harness::Opencode => "opencode",
        }
    }

    pub fn parse(s: &str) -> Option<Harness> {
        match s {
            "claude" => Some(Harness::Claude),
            "pi" => Some(Harness::Pi),
            "opencode" => Some(Harness::Opencode),
            _ => None,
        }
    }
}

/// Identifies one session across all harnesses.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub harness: Harness,
    pub id: String,
}

impl SessionKey {
    /// `"<harness>:<id>"`, e.g. `"claude:<id>"`.
    pub fn as_string(&self) -> String {
        format!("{}:{}", self.harness.as_str(), self.id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub subagent: bool,
    /// Unix seconds.
    pub ts: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub subagent: bool,
}

/// The transcript files making up one session.
#[derive(Clone, Debug)]
pub struct SessionFiles {
    pub key: SessionKey,
    /// `files[0]` is the main transcript.
    pub files: Vec<SourceFile>,
}

/// Result of parsing a transcript from a byte offset.
#[derive(Clone, Debug, Default)]
pub struct Parsed {
    /// Byte offset just past the last complete line consumed.
    pub new_offset: u64,
    pub messages: Vec<Message>,
    /// Harness-recorded title, latest wins.
    pub title: Option<String>,
    /// First user-typed text seen in this parse.
    pub first_prompt: Option<String>,
    /// Distinct working directories, in first-seen order.
    pub cwds: Vec<String>,
    /// Unix seconds.
    pub first_ts: Option<i64>,
    /// Unix seconds.
    pub last_ts: Option<i64>,
}

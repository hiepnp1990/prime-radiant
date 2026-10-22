//! `read` and `grep`: a session's messages rendered again from the exact raw bytes each one points to, cut into pages.

use std::collections::HashMap;
use std::collections::hash_map;
use std::ops::Range;

use regex::RegexBuilder;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::index::{self, Cuts, Identity, Lineage, Message, Timestamp};
use crate::raw::{RawRef, Sha256, Source, StreamId, View};
use crate::redact;
use crate::{Error, Result};

/// The most characters a page holds, counting each message's header line and the blank line between messages.
pub const PAGE_CHARS: usize = 12_000;
pub const GREP_MATCHES: usize = 20;
/// How much text `grep` shows on each side of a match.
const CONTEXT_CHARS: usize = 100;

#[derive(Debug, Serialize, Deserialize)]
pub struct Page {
    pub key: String,
    /// Counts from 1.
    pub page: usize,
    pub pages: usize,
    pub messages: Vec<Entry>,
}

impl Page {
    /// The page as `read` prints it, without the line that names the next page.
    pub fn text(&self) -> String {
        self.messages
            .iter()
            .map(Entry::block)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One message, or one part of a message that is larger than a page.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub position: i64,
    pub role: String,
    pub at: Option<Timestamp>,
    /// The session that owns this message, when a fork holds it as copied history.
    pub copied_from: Option<String>,
    pub part: Option<Part>,
    /// Secrets are removed, tool calls and tool results are cut, and each image is one line.
    pub text: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Part {
    pub number: usize,
    pub of: usize,
}

impl Entry {
    fn header(&self) -> String {
        self.header_with(self.part)
    }

    fn header_with(&self, part: Option<Part>) -> String {
        let at = self
            .at
            .map_or("time unknown".to_owned(), |at| at.to_string());
        let mut header = format!("[{}] {}, {at}", self.position, self.role);
        if let Some(owner) = &self.copied_from {
            header += &format!(", copied from {owner}");
        }
        if let Some(part) = part {
            header += &format!(", part {} of {}", part.number, part.of);
        }
        header
    }

    /// The header line, then the text.
    pub fn block(&self) -> String {
        if self.text.is_empty() {
            format!("{}\n", self.header())
        } else {
            format!("{}\n{}\n", self.header(), self.text)
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Matches {
    pub key: String,
    pub pattern: String,
    /// How many matches the session has. `matches` holds the first 20.
    pub total: usize,
    pub matches: Vec<Match>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Match {
    pub position: i64,
    pub role: String,
    /// The page of `read` that shows the match.
    pub page: usize,
    pub context: String,
}

/// Where each page's text comes from: a byte range of one message's text.
struct Slice {
    message: usize,
    bytes: Range<usize>,
    part: Option<Part>,
}

/// Every message of a session, rendered, and how they fall into pages.
pub struct Transcript {
    key: String,
    messages: Vec<Entry>,
    pages: Vec<Vec<Slice>>,
}

impl Transcript {
    pub fn render(db: &Connection, raw: &View, key: &str) -> Result<Self> {
        let messages = render_messages(db, raw, key)?;
        let pages = layout(&messages);
        Ok(Self {
            key: key.to_owned(),
            messages,
            pages,
        })
    }

    pub fn page(&self, number: usize) -> Result<Page> {
        let slices = number
            .checked_sub(1)
            .and_then(|index| self.pages.get(index))
            .ok_or_else(|| {
                let pages = self.pages.len();
                let noun = if pages == 1 { "page" } else { "pages" };
                Error::Usage(format!(
                    "{} has {pages} {noun}, so there is no page {number}",
                    self.key
                ))
            })?;
        let messages = slices
            .iter()
            .map(|slice| {
                let whole = &self.messages[slice.message];
                Entry {
                    text: whole.text[slice.bytes.clone()].to_owned(),
                    part: slice.part,
                    ..whole.clone()
                }
            })
            .collect();
        Ok(Page {
            key: self.key.clone(),
            page: number,
            pages: self.pages.len(),
            messages,
        })
    }

    /// Case-insensitive substring search over the rendered text of every message.
    pub fn grep(&self, pattern: &str) -> Result<Matches> {
        if pattern.trim().is_empty() {
            return Err(Error::Usage("grep needs a pattern".to_owned()));
        }
        let finder = RegexBuilder::new(&regex::escape(pattern))
            .case_insensitive(true)
            .build()
            .expect("an escaped pattern compiles");
        let mut total = 0;
        let mut matches = Vec::new();
        for (index, message) in self.messages.iter().enumerate() {
            for found in finder.find_iter(&message.text) {
                total += 1;
                if matches.len() < GREP_MATCHES {
                    matches.push(Match {
                        position: message.position,
                        role: message.role.clone(),
                        page: self.page_of(index, found.start()),
                        context: context(&message.text, found.range()),
                    });
                }
            }
        }
        Ok(Matches {
            key: self.key.clone(),
            pattern: redact::secrets(pattern).into_string(),
            total,
            matches,
        })
    }

    fn page_of(&self, message: usize, byte: usize) -> usize {
        self.pages
            .iter()
            .position(|slices| {
                slices
                    .iter()
                    .any(|slice| slice.message == message && slice.bytes.contains(&byte))
            })
            .map_or(1, |index| index + 1)
    }
}

/// Each message of the session in position order, rendered from the raw bytes its `raw_ref` points to.
fn render_messages(db: &Connection, raw: &View, key: &str) -> Result<Vec<Entry>> {
    let (source, identity) = identity(db, key)?;
    let parser = index::parser(source);
    let mut versions: HashMap<Sha256, HashMap<String, Message>> = HashMap::new();
    let mut query = db.prepare(
        "SELECT id, position, message_id, role, at, copied_from, searchable_text,
                raw_stream, raw_generation, raw_version, raw_start, raw_end
         FROM messages WHERE session_key = ?1 ORDER BY position",
    )?;
    let mut media =
        db.prepare("SELECT hash, mime, bytes FROM media WHERE message = ?1 ORDER BY rowid")?;
    let mut rows = query.query([key])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let message_id: String = row.get(2)?;
        let stream = StreamId(row.get(7)?);
        let generation: Option<i64> = row.get(8)?;
        let version: Option<String> = row.get(9)?;
        let (start, end) = (row.get::<_, i64>(10)? as u64, row.get::<_, i64>(11)? as u64);
        let reparsed = match (generation, version) {
            (Some(generation), _) => {
                let line = raw.read(&RawRef {
                    stream,
                    generation: generation as u64,
                    start,
                    end,
                })?;
                parser
                    .line(&identity, &line, Cuts::READ)
                    .messages
                    .into_iter()
                    .find(|message| message.id == message_id)
            }
            (None, Some(hex)) => {
                let sha256 = Sha256::from_hex(&hex)
                    .ok_or_else(|| Error::Corrupt(format!("message {id} names version {hex}")))?;
                let by_id = match versions.entry(sha256) {
                    hash_map::Entry::Occupied(parsed) => parsed.into_mut(),
                    hash_map::Entry::Vacant(slot) => {
                        let parsed = parser.version(&identity, &raw.version(sha256)?, Cuts::READ);
                        slot.insert(
                            parsed
                                .messages
                                .into_iter()
                                .map(|(message, _)| (message.id.clone(), message))
                                .collect(),
                        )
                    }
                };
                by_id.remove(&message_id)
            }
            (None, None) => None,
        };
        // A parser that no longer finds the message in its bytes still leaves the text the index kept.
        let mut text = match reparsed {
            Some(message) => message.searchable_text,
            None => row.get(6)?,
        };
        text = redact::secrets(&text).into_string();
        let attached = media
            .query_map([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (hash, mime, bytes) in attached {
            let kind = if mime.starts_with("image/") {
                "image"
            } else {
                "file"
            };
            if !text.is_empty() {
                text.push('\n');
            }
            text += &format!("[{kind} {mime}, {bytes} bytes, sha256 {hash}]");
        }
        entries.push(Entry {
            position: row.get(1)?,
            role: row.get(3)?,
            at: row.get::<_, Option<i64>>(4)?.map(Timestamp),
            copied_from: row.get(5)?,
            part: None,
            text,
        });
    }
    Ok(entries)
}

/// What a parser needs to name the messages of a session's streams again.
fn identity(db: &Connection, key: &str) -> Result<(Source, Identity)> {
    let (source, native_id, kind, parent): (String, String, String, Option<String>) = db
        .query_row(
            "SELECT source, native_id, kind, parent_key FROM sessions WHERE key = ?1",
            params![key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let source = Source::parse(&source)
        .ok_or_else(|| Error::Corrupt(format!("session {key} has source {source}")))?;
    let lineage = match (kind.as_str(), parent) {
        ("subagent", Some(parent)) => Lineage::Subagent {
            parent_native_id: parent
                .strip_prefix(&format!("{}/", source.as_str()))
                .unwrap_or(&parent)
                .to_owned(),
        },
        _ => Lineage::Main,
    };
    Ok((
        source,
        Identity {
            native_id,
            lineage,
            aliases: Vec::new(),
            project: None,
        },
    ))
}

/// Fills each page with whole messages in order. A message larger than a page gets pages of its own, one part each.
fn layout(messages: &[Entry]) -> Vec<Vec<Slice>> {
    let mut pages: Vec<Vec<Slice>> = Vec::new();
    let mut used = PAGE_CHARS;
    for (index, message) in messages.iter().enumerate() {
        let size = message.block().chars().count();
        if size <= PAGE_CHARS {
            // A blank line separates a message from the one before it on the same page.
            if used + 1 + size > PAGE_CHARS {
                pages.push(Vec::new());
                used = size;
            } else {
                used += 1 + size;
            }
            pages.last_mut().expect("a page was started").push(Slice {
                message: index,
                bytes: 0..message.text.len(),
                part: None,
            });
            continue;
        }
        let widest = message.header_with(Some(Part {
            number: 99_999,
            of: 99_999,
        }));
        // The header line and the newline after the text take room from each part.
        let parts = split(&message.text, PAGE_CHARS - widest.chars().count() - 2);
        let of = parts.len();
        for (number, bytes) in parts.into_iter().enumerate() {
            pages.push(vec![Slice {
                message: index,
                bytes,
                part: Some(Part {
                    number: number + 1,
                    of,
                }),
            }]);
        }
        used = PAGE_CHARS;
    }
    if pages.is_empty() {
        pages.push(Vec::new());
    }
    pages
}

/// Byte ranges of `text` of at most `chars` characters each, broken at a newline when one falls in a range's second half.
fn split(text: &str, chars: usize) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let rest = &text[start..];
        let Some((limit, _)) = rest.char_indices().nth(chars) else {
            ranges.push(start..text.len());
            break;
        };
        let end = match rest[..limit].rfind('\n') {
            Some(newline) if newline >= limit / 2 => newline + 1,
            _ => limit,
        };
        ranges.push(start..start + end);
        start += end;
    }
    ranges
}

/// About `CONTEXT_CHARS` characters on each side of a match, on one line.
fn context(text: &str, found: Range<usize>) -> String {
    let before = &text[..found.start];
    let from = before
        .char_indices()
        .rev()
        .nth(CONTEXT_CHARS - 1)
        .map_or(0, |(at, _)| at);
    let after = &text[found.end..];
    let to = found.end
        + after
            .char_indices()
            .nth(CONTEXT_CHARS)
            .map_or(after.len(), |(at, _)| at);
    let mut context = text[from..to]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if from > 0 {
        context.insert_str(0, "...");
    }
    if to < text.len() {
        context.push_str("...");
    }
    redact::secrets(&context).into_string()
}

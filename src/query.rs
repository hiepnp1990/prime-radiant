use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::index::{self, Timestamp};
use crate::raw::{Progress, Source, View};
use crate::redact;
use crate::transcript::{Matches, Page, Transcript};
use crate::{Error, Result};

const LIVE_WINDOW_MS: i64 = 10 * 60 * 1000;
const MIN_ID_PREFIX: usize = 8;
const TITLE_WEIGHT: f64 = 3.0;
/// How much a session's score grows when its own title has every query term, weighted by each term's rarity.
const TITLE_COVERAGE_BONUS: f64 = 2.0;
/// How much a long session's score shrinks, by the log of its length over the mean. A long session matches more words by chance.
const LENGTH_PENALTY: f64 = 0.5;
const SNIPPET_TOKENS: i64 = 24;

const FIRST_REQUEST_CHARS: usize = 600;
const LAST_ANSWER_CHARS: usize = 1200;
const CHANGED_FILES: usize = 20;
pub(crate) const BRIEF_METHOD: &str = "extractive-1";

const STOP_WORDS: &str = "\
    a about above after again against all am an and any are as at be because been before \
    being below between both but by can could did do does doing down during each few for \
    from further had has have having he her here hers herself him himself his how i if in \
    into is it its itself just me more most my myself no nor not now of off on once only or \
    other our ours ourselves out over own same she should so some such than that the their \
    theirs them themselves then there these they this those through to too under until up \
    very was we were what when where which while who whom why will with would you your yours \
    yourself yourselves";

static SYSTEM_REMINDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<system-reminder>.*?(?:</system-reminder>|\z)")
        .expect("system reminder pattern compiles")
});
/// The tags Claude Code wraps around a slash command, which a brief shows as `/name args`.
static COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)<command-message>.*?</command-message>\s*<command-name>(?P<name>.*?)</command-name>\s*(?:<command-args>(?P<args>.*?)</command-args>)?",
    )
    .expect("command pattern compiles")
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Live,
    Idle,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Live => "live",
            State::Idle => "idle",
        }
    }

    pub fn of(last_activity: Option<Timestamp>, now: Timestamp) -> Self {
        match last_activity {
            Some(at) if now.0 - at.0 < LIVE_WINDOW_MS => State::Live,
            _ => State::Idle,
        }
    }
}

/// One `find` candidate. `score` and `snippet` are `None` when the query was an ID.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    pub key: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub ran_on: String,
    pub last_activity: Option<Timestamp>,
    pub state: State,
    pub score: Option<f64>,
    pub snippet: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Brief {
    pub key: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub origin: String,
    pub parent: Option<Parent>,
    pub started: Option<Timestamp>,
    pub last_activity: Option<Timestamp>,
    pub state: State,
    pub archived_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
    /// Sessions that another harness ran for this one, such as the Claude Code session behind a T3 Code thread.
    pub underlying_sessions: Vec<String>,
    /// Sessions that name this one as an underlying session.
    pub front_end_sessions: Vec<String>,
    /// Message counts by role, plus `total`.
    pub messages: BTreeMap<String, u64>,
    #[serde(flatten)]
    pub body: BriefBody,
    pub freshness: Freshness,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
    pub key: String,
    pub kind: ParentKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParentKind {
    Fork,
    Subagent,
}

/// The extracted part of a brief, stored in `briefs` and rebuilt when the session has newer messages.
#[derive(Debug, Serialize, Deserialize)]
pub struct BriefBody {
    pub first_request: Option<String>,
    pub last_answer: Option<String>,
    pub pull_requests: Vec<String>,
    pub changed_files: Vec<String>,
    pub changed_files_total: usize,
    pub subagents: Vec<Subagent>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Subagent {
    pub key: String,
    pub title: Option<String>,
    pub message_count: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Freshness {
    /// The last message position the brief covers.
    pub built_through: Option<i64>,
    pub built_at: Timestamp,
    /// Whether this call rebuilt the brief because the session had newer messages.
    pub rebuilt: bool,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub counts: Counts,
    pub streams: Vec<StreamStats>,
    pub sessions: Vec<SessionStats>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Counts {
    pub streams: u64,
    pub segments: u64,
    pub versions: u64,
    pub sessions: u64,
    pub messages: u64,
    pub aliases: u64,
    pub media: u64,
    pub artifacts: u64,
}

/// `offset` is set for append streams and `checksum` for replace streams.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamStats {
    pub locator: String,
    pub generation: u64,
    pub offset: Option<u64>,
    pub checksum: Option<String>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    pub key: String,
    pub message_count: u64,
}

/// A `find` question. Every session it returns passes the filters and holds every phrase.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Find {
    /// Words about the topic, or a session key, an alias, or an ID prefix.
    pub query: String,
    /// Text that one message or the title of each session holds, with its words adjacent and in order.
    pub phrases: Vec<String>,
    pub filters: Filters,
    pub limit: usize,
}

/// What a session's metadata must match for `find` to return it. An empty list or `None` matches every session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    /// Any of these sources.
    pub sources: Vec<Source>,
    /// Any of these origins, the machines or accounts the sessions ran on.
    pub origins: Vec<String>,
    /// Text that the project path contains, ignoring case.
    pub project: Option<String>,
    /// The earliest last activity.
    pub since: Option<Timestamp>,
    /// The latest last activity.
    pub until: Option<Timestamp>,
}

impl Filters {
    fn admits(
        &self,
        source: &str,
        origin: &str,
        project: Option<&str>,
        last_activity: Option<Timestamp>,
    ) -> bool {
        (self.sources.is_empty() || self.sources.iter().any(|s| s.as_str() == source))
            && (self.origins.is_empty() || self.origins.iter().any(|o| o == origin))
            && self.project.as_ref().is_none_or(|part| {
                project.is_some_and(|p| p.to_lowercase().contains(&part.to_lowercase()))
            })
            && self
                .since
                .is_none_or(|since| last_activity.is_some_and(|at| at >= since))
            && self
                .until
                .is_none_or(|until| last_activity.is_some_and(|at| at <= until))
    }
}

/// A question that a local data folder and the hub answer the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    Find(Find),
    Brief { session: String },
    Stats,
    Read { session: String, page: usize },
    Grep { session: String, pattern: String },
}

#[derive(Debug)]
pub enum Answer {
    Found(Vec<Hit>),
    Brief(Box<Brief>),
    Stats(Stats),
    Page(Page),
    Matches(Matches),
}

/// Answers questions from a data directory, reading only. A brief it rebuilt is for the indexer to store.
pub struct Query {
    dir: std::path::PathBuf,
    db: Connection,
    now: Timestamp,
}

impl Query {
    pub fn open(dir: &Path, now: Timestamp) -> Result<Self> {
        if !dir.join("index.db").is_file() {
            return Err(Error::Usage(format!(
                "{} has no index; run import first",
                dir.display()
            )));
        }
        Ok(Self {
            dir: dir.to_owned(),
            db: index::open_reader(dir)?,
            now,
        })
    }

    pub fn answer(&self, ask: Ask) -> Result<Answer> {
        Ok(match ask {
            Ask::Find(find) => Answer::Found(self.find(&find)?),
            Ask::Brief { session } => Answer::Brief(Box::new(self.brief(&session)?)),
            Ask::Stats => Answer::Stats(self.stats()?),
            Ask::Read { session, page } => Answer::Page(self.read(&session, page)?),
            Ask::Grep { session, pattern } => Answer::Matches(self.grep(&session, &pattern)?),
        })
    }

    /// One page of the session's messages.
    pub fn read(&self, session: &str, page: usize) -> Result<Page> {
        self.transcript(session)?.page(page)
    }

    /// The first matches of `pattern` in the session's messages, with the page of `read` that shows each one.
    pub fn grep(&self, session: &str, pattern: &str) -> Result<Matches> {
        self.transcript(session)?.grep(pattern)
    }

    fn transcript(&self, session: &str) -> Result<Transcript> {
        let key = self.one_session(session)?;
        Transcript::render(&self.db, &View::open(&self.dir)?, &key)
    }

    /// An exact key or alias wins, then a unique ID prefix, then full-text search. An ID that names only sessions
    /// outside the filters or without the phrases finds nothing.
    pub fn find(&self, find: &Find) -> Result<Vec<Hit>> {
        let admitted = self.admitted(find)?;
        let keys = self.resolve(&find.query)?;
        if keys.is_empty() {
            return self.search(find, &admitted);
        }
        keys.iter()
            .filter(|key| admitted.contains(*key))
            .take(find.limit)
            .map(|key| self.hit(key, None, None))
            .collect()
    }

    /// The sessions that pass the filters and hold every phrase. A subagent's text counts for its main session too.
    fn admitted(&self, find: &Find) -> Result<HashSet<String>> {
        let mut holding = self.db.prepare(
            "SELECT key, IIF(kind = 'subagent', parent_key, NULL) FROM sessions
             WHERE key IN (SELECT session_key FROM message_search WHERE message_search MATCH ?1
                           UNION SELECT key FROM title_search WHERE title_search MATCH ?1)",
        )?;
        let mut holders = Vec::with_capacity(find.phrases.len());
        for text in &find.phrases {
            let mut held = HashSet::new();
            for row in holding.query_map([phrase(text)], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })? {
                let (key, main) = row?;
                held.extend(main);
                held.insert(key);
            }
            holders.push(held);
        }
        let mut sessions = self
            .db
            .prepare("SELECT key, source, ran_on, project, last_activity FROM sessions")?;
        let mut admitted = HashSet::new();
        for row in sessions.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?.map(Timestamp),
            ))
        })? {
            let (key, source, origin, project, last_activity) = row?;
            if find
                .filters
                .admits(&source, &origin, project.as_deref(), last_activity)
                && holders.iter().all(|held| held.contains(&key))
            {
                admitted.insert(key);
            }
        }
        Ok(admitted)
    }

    pub fn brief(&self, session: &str) -> Result<Brief> {
        let key = self.one_session(session)?;
        let (title, project, origin, parent, started, last_activity, archived_at, deleted_at) =
            self.db.query_row(
                "SELECT title, project, ran_on, parent_key, started_at, last_activity, archived_at, deleted_at, kind
                 FROM sessions WHERE key = ?1",
                [&key],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?
                            .zip(match row.get::<_, String>(8)?.as_str() {
                                "fork" => Some(ParentKind::Fork),
                                "subagent" => Some(ParentKind::Subagent),
                                _ => None,
                            })
                            .map(|(key, kind)| Parent { key, kind }),
                        row.get::<_, Option<i64>>(4)?.map(Timestamp),
                        row.get::<_, Option<i64>>(5)?.map(Timestamp),
                        row.get::<_, Option<i64>>(6)?.map(Timestamp),
                        row.get::<_, Option<i64>>(7)?.map(Timestamp),
                    ))
                },
            )?;
        let (body, freshness) = self.brief_body(&key)?;
        Ok(Brief {
            underlying_sessions: self.strings(
                "SELECT value FROM artifacts WHERE session_key = ?1 AND kind = 'underlying_session' ORDER BY value",
                &key,
            )?,
            front_end_sessions: self.strings(
                "SELECT session_key FROM artifacts WHERE value = ?1 AND kind = 'underlying_session'
                 ORDER BY session_key",
                &key,
            )?,
            archived_at,
            deleted_at,
            messages: self.role_counts(&key)?,
            state: State::of(last_activity, self.now),
            key,
            title,
            project,
            origin,
            parent,
            started,
            last_activity,
            body,
            freshness,
        })
    }

    fn one_session(&self, session: &str) -> Result<String> {
        match self.resolve(session)?.as_slice() {
            [key] => Ok(key.clone()),
            [] => Err(Error::Usage(format!("no session matches {session}"))),
            several => Err(Error::Usage(format!(
                "{session} matches {} sessions: {}",
                several.len(),
                several.join(", ")
            ))),
        }
    }

    /// The sessions an ID names, best first. Empty when the input is not an ID.
    fn resolve(&self, input: &str) -> Result<Vec<String>> {
        let input = input.trim();
        let exact: Option<String> = self
            .db
            .query_row(
                "SELECT key FROM sessions WHERE key = ?1
                 UNION ALL SELECT session_key FROM aliases WHERE value = ?1 LIMIT 1",
                [input],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(key) = exact {
            return Ok(vec![key]);
        }
        let Some(prefix) = id_prefix(input) else {
            return Ok(Vec::new());
        };
        let mut query = self.db.prepare(
            "SELECT DISTINCT s.key, IIF(s.kind = 'subagent', s.parent_key, NULL)
             FROM aliases a JOIN sessions s ON s.key = a.session_key
             WHERE substr(a.value, 1, length(?1)) = ?1
             ORDER BY s.last_activity DESC, s.key",
        )?;
        let matches = query
            .query_map([&prefix], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let keys: HashSet<&str> = matches.iter().map(|(key, _)| key.as_str()).collect();
        // A main session's ID prefix also prefixes its subagents' IDs, which should not make it ambiguous.
        Ok(matches
            .iter()
            .filter(|(_, parent)| !parent.as_deref().is_some_and(|p| keys.contains(p)))
            .map(|(key, _)| key.clone())
            .collect())
    }

    /// Ranks the admitted sessions by the query's words and phrases.
    fn search(&self, find: &Find, admitted: &HashSet<String>) -> Result<Vec<Hit>> {
        let mut terms: Vec<String> = terms(&find.query).iter().map(|t| phrase(t)).collect();
        for text in &find.phrases {
            let text = phrase(&text.to_lowercase());
            if !terms.contains(&text) {
                terms.push(text);
            }
        }
        if terms.is_empty() || find.limit == 0 {
            return Ok(Vec::new());
        }
        let main_of = self.main_sessions()?;
        let main = |key: String| main_of.get(&key).cloned().unwrap_or(key);
        let main_count: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM sessions WHERE kind <> 'subagent'",
            [],
            |row| row.get(0),
        )?;

        // Per main or fork session, the best score of each term in any of its messages or titles.
        let mut term_scores: HashMap<String, Vec<f64>> = HashMap::new();
        let mut idf = Vec::with_capacity(terms.len());
        let mut message_scores: HashMap<i64, (String, f64)> = HashMap::new();
        // Per session, which terms its own title has.
        let mut title_terms: HashMap<String, Vec<bool>> = HashMap::new();
        let mut messages = self.db.prepare(
            "SELECT rowid, session_key, -bm25(message_search) FROM message_search WHERE message_search MATCH ?1",
        )?;
        let mut titles = self.db.prepare(
            "SELECT key, -bm25(title_search) FROM title_search WHERE title_search MATCH ?1",
        )?;
        for (i, term) in terms.iter().enumerate() {
            let mut matched: HashSet<String> = HashSet::new();
            let rows = messages.query_map([term], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get(2)?))
            })?;
            for row in rows {
                let (rowid, session, score): (i64, String, f64) = row?;
                let session = main(session);
                let best = &mut term_scores
                    .entry(session.clone())
                    .or_insert_with(|| vec![0.0; terms.len()])[i];
                *best = best.max(score);
                message_scores
                    .entry(rowid)
                    .or_insert((session.clone(), 0.0))
                    .1 += score;
                matched.insert(session);
            }
            let rows = titles
                .query_map([term], |row| Ok((row.get::<_, String>(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<(String, f64)>>>()?;
            for (session, score) in rows {
                if !main_of.contains_key(&session) {
                    title_terms
                        .entry(session.clone())
                        .or_insert_with(|| vec![false; terms.len()])[i] = true;
                }
                let session = main(session);
                term_scores
                    .entry(session.clone())
                    .or_insert_with(|| vec![0.0; terms.len()])[i] += TITLE_WEIGHT * score;
                matched.insert(session);
            }
            idf.push(session_idf(main_count, matched.len()));
        }

        let idf_total: f64 = idf.iter().sum();
        let last_activity = self.last_activity()?;
        let sizes = self.sizes(&main_of)?;
        let mean_size = sizes.values().sum::<f64>() / sizes.len().max(1) as f64;
        let mut ranked: Vec<(String, f64)> = term_scores
            .into_iter()
            .map(|(session, scores)| {
                let score: f64 = scores.iter().zip(&idf).map(|(s, w)| s * w).sum();
                let coverage = title_terms.get(&session).map_or(0.0, |has| {
                    has.iter()
                        .zip(&idf)
                        .filter(|(has, _)| **has)
                        .map(|(_, w)| w)
                        .sum::<f64>()
                        / idf_total
                });
                let size = sizes.get(&session).copied().unwrap_or(0.0);
                let length = 1.0 + LENGTH_PENALTY * (1.0 + size / mean_size).ln();
                (
                    session,
                    score * (1.0 + TITLE_COVERAGE_BONUS * coverage) / length,
                )
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| last_activity.get(&b.0).cmp(&last_activity.get(&a.0)))
                .then_with(|| a.0.cmp(&b.0))
        });
        ranked.retain(|(session, _)| admitted.contains(session));
        ranked.truncate(find.limit);

        let mut best_message: HashMap<&str, (i64, f64)> = HashMap::new();
        for (rowid, (session, score)) in &message_scores {
            let best = best_message.entry(session).or_insert((*rowid, *score));
            if *score > best.1 || (*score == best.1 && *rowid < best.0) {
                *best = (*rowid, *score);
            }
        }
        let any_term = terms.join(" OR ");
        ranked
            .iter()
            .map(|(session, score)| {
                let snippet = match best_message.get(session.as_str()) {
                    Some((rowid, _)) => self.snippet(&any_term, *rowid)?,
                    None => None,
                };
                self.hit(session, Some(*score), snippet)
            })
            .collect()
    }

    /// How many searchable messages each main or fork session holds, counting its subagents' and not its copies.
    fn sizes(&self, main_of: &HashMap<String, String>) -> Result<HashMap<String, f64>> {
        let mut query = self
            .db
            .prepare("SELECT key, own_message_count FROM sessions WHERE own_message_count > 0")?;
        let mut sizes: HashMap<String, f64> = HashMap::new();
        for row in query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })? {
            let (key, count) = row?;
            let session = main_of.get(&key).cloned().unwrap_or(key);
            *sizes.entry(session).or_default() += count;
        }
        Ok(sizes)
    }

    /// Maps each subagent session to its main session.
    fn main_sessions(&self) -> Result<HashMap<String, String>> {
        let mut query = self
            .db
            .prepare("SELECT key, parent_key FROM sessions WHERE kind = 'subagent'")?;
        let rows = query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<HashMap<String, String>>>()?;
        Ok(rows)
    }

    fn last_activity(&self) -> Result<HashMap<String, i64>> {
        let mut query = self
            .db
            .prepare("SELECT key, last_activity FROM sessions WHERE last_activity IS NOT NULL")?;
        let rows = query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn snippet(&self, any_term: &str, rowid: i64) -> Result<Option<String>> {
        let snippet: Option<String> = self
            .db
            .query_row(
                "SELECT snippet(message_search, 0, '**', '**', '...', ?3) FROM message_search
                 WHERE message_search MATCH ?1 AND rowid = ?2",
                params![any_term, rowid, SNIPPET_TOKENS],
                |row| row.get(0),
            )
            .optional()?;
        Ok(snippet.map(|text| {
            let text = redact::secrets(&text).into_string();
            text.split_whitespace().collect::<Vec<_>>().join(" ")
        }))
    }

    fn hit(&self, key: &str, score: Option<f64>, snippet: Option<String>) -> Result<Hit> {
        let (title, project, ran_on, last_activity) = self.db.query_row(
            "SELECT title, project, ran_on, last_activity FROM sessions WHERE key = ?1",
            [key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get::<_, Option<i64>>(3)?.map(Timestamp),
                ))
            },
        )?;
        Ok(Hit {
            key: key.to_owned(),
            title,
            project,
            ran_on,
            last_activity,
            state: State::of(last_activity, self.now),
            score,
            snippet,
        })
    }

    fn strings(&self, sql: &str, key: &str) -> Result<Vec<String>> {
        let mut query = self.db.prepare(sql)?;
        let rows = query
            .query_map([key], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn role_counts(&self, key: &str) -> Result<BTreeMap<String, u64>> {
        let mut query = self.db.prepare(
            "SELECT role, COUNT(*) FROM messages WHERE session_key = ?1 GROUP BY role ORDER BY role",
        )?;
        let mut counts = query
            .query_map([key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        let total = counts.values().sum();
        counts.insert("total".to_owned(), total);
        Ok(counts)
    }

    fn brief_body(&self, key: &str) -> Result<(BriefBody, Freshness)> {
        let latest: Option<i64> = self.db.query_row(
            "SELECT MAX(position) FROM messages WHERE session_key = ?1",
            [key],
            |row| row.get(0),
        )?;
        let stored: Option<(String, Option<i64>, i64)> = self
            .db
            .query_row(
                "SELECT body, built_through, built_at FROM briefs WHERE session_key = ?1 AND method = ?2",
                params![key, BRIEF_METHOD],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((body, built_through, built_at)) = stored
            && built_through == latest
            && let Ok(body) = serde_json::from_str(&body)
        {
            let freshness = Freshness {
                built_through,
                built_at: Timestamp(built_at),
                rebuilt: false,
            };
            return Ok((body, freshness));
        }
        let body = self.extract(key)?;
        let freshness = Freshness {
            built_through: latest,
            built_at: self.now,
            rebuilt: true,
        };
        Ok((body, freshness))
    }

    fn extract(&self, key: &str) -> Result<BriefBody> {
        let first_request = self.first_text(
            "SELECT searchable_text FROM messages WHERE session_key = ?1 AND role = 'user' AND copied_from IS NULL
             ORDER BY position",
            key,
            FIRST_REQUEST_CHARS,
        )?;
        let last_answer = self.first_text(
            "SELECT searchable_text FROM messages WHERE session_key = ?1 AND role = 'assistant' ORDER BY position DESC",
            key,
            LAST_ANSWER_CHARS,
        )?;

        let mut query = self.db.prepare(
            "SELECT a.kind, a.value FROM artifacts a JOIN messages m ON m.id = a.message
             WHERE a.session_key = ?1
                OR a.session_key IN (SELECT key FROM sessions WHERE parent_key = ?1 AND kind = 'subagent')
             ORDER BY m.at, m.position",
        )?;
        let artifacts = query
            .query_map([key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut seen = HashSet::new();
        let (mut pull_requests, mut changed_files) = (Vec::new(), Vec::new());
        for (kind, value) in artifacts {
            if !seen.insert(value.clone()) {
                continue;
            }
            match kind.as_str() {
                "pull_request" => pull_requests.push(value),
                _ => changed_files.push(value),
            }
        }
        let changed_files_total = changed_files.len();
        changed_files.truncate(CHANGED_FILES);

        let mut query = self.db.prepare(
            "SELECT key, title, message_count FROM sessions WHERE parent_key = ?1 AND kind = 'subagent'
             ORDER BY started_at, key",
        )?;
        let subagents = query
            .query_map([key], |row| {
                Ok(Subagent {
                    key: row.get(0)?,
                    title: row.get(1)?,
                    message_count: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(BriefBody {
            first_request,
            last_answer,
            pull_requests,
            changed_files,
            changed_files_total,
            subagents,
        })
    }

    /// The first row of `sql` whose text has something left after removing injected reminders.
    fn first_text(&self, sql: &str, key: &str, chars: usize) -> Result<Option<String>> {
        let mut query = self.db.prepare(sql)?;
        let mut rows = query.query([key])?;
        while let Some(row) = rows.next()? {
            let text: String = row.get(0)?;
            let text = SYSTEM_REMINDER.replace_all(&text, "");
            let text = COMMAND.replace_all(&text, "$name $args");
            let text = text.trim();
            if !text.is_empty() {
                return Ok(Some(cut(text, chars)));
            }
        }
        Ok(None)
    }

    pub fn stats(&self) -> Result<Stats> {
        let raw = View::open(&self.dir)?;
        let [streams, segments, versions] = raw.counts()?.map(|(_, n)| n);
        let count = |table: &str| -> Result<u64> {
            let n: i64 =
                self.db
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
            Ok(n as u64)
        };
        let counts = Counts {
            streams,
            segments,
            versions,
            sessions: count("sessions")?,
            messages: count("messages")?,
            aliases: count("aliases")?,
            media: count("media")?,
            artifacts: count("artifacts")?,
        };
        let mut stream_stats: Vec<StreamStats> = raw
            .streams()?
            .into_iter()
            .map(|stream| {
                let (offset, checksum) = match stream.state.progress {
                    Progress::Append {
                        confirmed_offset, ..
                    } => (Some(confirmed_offset), None),
                    Progress::Replace { latest_checksum } => (None, Some(latest_checksum.to_hex())),
                };
                StreamStats {
                    locator: stream.key.locator,
                    generation: stream.state.generation,
                    offset,
                    checksum,
                }
            })
            .collect();
        stream_stats.sort_by(|a, b| {
            (&a.locator, a.generation, a.offset, &a.checksum).cmp(&(
                &b.locator,
                b.generation,
                b.offset,
                &b.checksum,
            ))
        });
        let mut query = self
            .db
            .prepare("SELECT key, message_count FROM sessions ORDER BY key")?;
        let sessions = query
            .query_map([], |row| {
                Ok(SessionStats {
                    key: row.get(0)?,
                    message_count: row.get::<_, i64>(1)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Stats {
            counts,
            streams: stream_stats,
            sessions,
        })
    }
}

/// The lowercase ID prefix in `input`, if it has at least 8 hex digits and nothing but hex digits and hyphens.
fn id_prefix(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let digits = lower.chars().filter(char::is_ascii_hexdigit).count();
    (digits >= MIN_ID_PREFIX && lower.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        .then_some(lower)
}

/// The distinct search words in a query, lowercased, without stop words unless nothing else is left.
pub fn terms(query: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in query.split(|c: char| !c.is_alphanumeric()) {
        let word = word.to_lowercase();
        if !word.is_empty() && !words.contains(&word) {
            words.push(word);
        }
    }
    let content: Vec<String> = words
        .iter()
        .filter(|word| {
            !STOP_WORDS
                .split_whitespace()
                .any(|stop| stop == word.as_str())
        })
        .cloned()
        .collect();
    if content.is_empty() { words } else { content }
}

/// BM25's inverse document frequency, with main and fork sessions as the documents.
fn session_idf(sessions: i64, matched: usize) -> f64 {
    let (n, df) = (sessions as f64, matched as f64);
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

fn phrase(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

fn cut(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_owned(),
    }
}

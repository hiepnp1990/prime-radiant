use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use base64::Engine as _;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};
use serde::Serialize;
use serde_json::Value;

use crate::{Error, Result};

const THREADS: &str = "SELECT * FROM orchestration_v2_projection_threads ORDER BY thread_id";
const THREAD_COUNT: &str = "SELECT count(*) FROM orchestration_v2_projection_threads";
const PROJECTS: &str = "SELECT * FROM projection_projects";
const MESSAGES: &str = "SELECT * FROM orchestration_v2_projection_messages
    ORDER BY thread_id, created_at, message_id";
const PROVIDER_THREADS: &str = "SELECT * FROM orchestration_v2_projection_provider_threads
    WHERE thread_id IS NOT NULL ORDER BY thread_id, provider_thread_id";

/// One database row, column name to value. Keys are sorted, so an unchanged row serializes to the same bytes.
type Columns = BTreeMap<String, Value>;

/// One thread as the raw layer keeps it. The T3 Code parser in the indexer reads this shape.
#[derive(Serialize)]
struct Snapshot<'a> {
    thread: &'a Columns,
    project: Option<&'a Columns>,
    provider_threads: &'a [Columns],
    messages: &'a [Columns],
}

pub struct ThreadSnapshot {
    pub locator: String,
    pub bytes: Vec<u8>,
}

/// Reads every thread in a T3 Code database as one JSON snapshot, from a single read transaction.
/// The connection is read-only, so T3 Code's database and its write-ahead log stay as they were.
pub fn snapshots(db_path: &Path) -> Result<Vec<ThreadSnapshot>> {
    let mut db = open_read_only(db_path)?;
    let tx = db.transaction()?;
    let threads = rows(&tx, THREADS)?;
    let projects = by_column(rows(&tx, PROJECTS)?, "project_id");
    let messages = by_column(rows(&tx, MESSAGES)?, "thread_id");
    let provider_threads = by_column(rows(&tx, PROVIDER_THREADS)?, "thread_id");
    drop(tx);

    let none = Vec::new();
    threads
        .iter()
        .map(|thread| {
            let id = text(thread, "thread_id")
                .ok_or_else(|| Error::Corrupt("a T3 Code thread has no thread_id".to_owned()))?;
            let snapshot = Snapshot {
                thread,
                project: text(thread, "project_id")
                    .and_then(|project| projects.get(project))
                    .and_then(|rows| rows.first()),
                provider_threads: provider_threads.get(id).unwrap_or(&none),
                messages: messages.get(id).unwrap_or(&none),
            };
            Ok(ThreadSnapshot {
                locator: format!("threads/{id}.json"),
                bytes: serde_json::to_vec(&snapshot).expect("a snapshot serializes"),
            })
        })
        .collect()
}

/// How many threads a T3 Code database holds, read without changing it.
pub fn thread_count(db_path: &Path) -> Result<u64> {
    let count: i64 = open_read_only(db_path)?.query_row(THREAD_COUNT, [], |row| row.get(0))?;
    Ok(count.unsigned_abs())
}

fn open_read_only(path: &Path) -> Result<Connection> {
    let path = path
        .to_str()
        .ok_or_else(|| Error::Usage(format!("{} is not a UTF-8 path", path.display())))?;
    let mut uri = String::from("file:");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push_str("?mode=ro");
    Ok(Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

fn rows(db: &Connection, sql: &str) -> Result<Vec<Columns>> {
    let mut query = db.prepare(sql)?;
    let names: Vec<String> = query
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let rows = query
        .query_map([], |row| columns(row, &names))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn columns(row: &Row, names: &[String]) -> rusqlite::Result<Columns> {
    let mut columns = Columns::new();
    for (i, name) in names.iter().enumerate() {
        let value = match row.get_ref(i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => n.into(),
            ValueRef::Real(x) => serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number),
            ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned().into(),
            ValueRef::Blob(bytes) => serde_json::json!({
                "base64": base64::engine::general_purpose::STANDARD.encode(bytes)
            }),
        };
        columns.insert(name.clone(), value);
    }
    Ok(columns)
}

fn by_column(rows: Vec<Columns>, column: &str) -> HashMap<String, Vec<Columns>> {
    let mut groups: HashMap<String, Vec<Columns>> = HashMap::new();
    for row in rows {
        if let Some(key) = text(&row, column) {
            groups.entry(key.to_owned()).or_default().push(row);
        }
    }
    groups
}

fn text<'a>(row: &'a Columns, column: &str) -> Option<&'a str> {
    row.get(column).and_then(Value::as_str)
}

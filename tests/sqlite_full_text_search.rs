use rusqlite::Connection;

#[test]
fn bundled_sqlite_supports_fts5_match() {
    let db = Connection::open_in_memory().expect("in-memory database opens");
    db.execute_batch(
        "CREATE VIRTUAL TABLE messages USING fts5(session, body);
         INSERT INTO messages VALUES ('first', 'add a json output flag to the list command');
         INSERT INTO messages VALUES ('second', 'fix the flaky file watcher test');",
    )
    .expect("the bundled SQLite build includes FTS5");

    let mut query = db
        .prepare("SELECT session FROM messages WHERE messages MATCH ?1")
        .expect("MATCH query prepares");
    let sessions: Vec<String> = query
        .query_map(["json flag"], |row| row.get(0))
        .expect("MATCH query runs")
        .collect::<Result<_, _>>()
        .expect("rows decode");

    assert_eq!(sessions, ["first"], "MATCH returns only the matching row");
}

use std::fs;
use std::path::Path;

use prime_radiant::collect::import_claude_code;
use prime_radiant::index::{Indexer, Timestamp};
use prime_radiant::query::{Find, Query};
use prime_radiant::raw::Store;
use serde_json::json;
use tempfile::TempDir;

const PROJECT: &str = "-home-dev-notes";

/// Writes one made-up main session: an optional title, then user messages one minute apart from `start`.
fn session(root: &Path, id: &str, title: Option<&str>, start: &str, messages: &[&str]) {
    let dir = root.join(PROJECT);
    fs::create_dir_all(&dir).expect("project folder is created");
    let mut lines = String::new();
    if let Some(title) = title {
        lines.push_str(&format!(
            "{}\n",
            json!({"type": "custom-title", "customTitle": title, "sessionId": id})
        ));
    }
    let mut parent: Option<String> = None;
    for (i, text) in messages.iter().enumerate() {
        let uuid = format!("{}-0000-4000-8000-{i:012}", &id[..8]);
        lines.push_str(&format!(
            "{}\n",
            json!({
                "parentUuid": parent,
                "isSidechain": false,
                "cwd": "/home/dev/notes",
                "sessionId": id,
                "type": "user",
                "message": {"role": "user", "content": text},
                "uuid": uuid,
                "timestamp": format!("{start}:{:02}.000Z", i % 60),
            })
        ));
        parent = Some(uuid);
    }
    fs::write(dir.join(format!("{id}.jsonl")), lines).expect("transcript is written");
}

fn find(root: &Path, words: &str) -> Vec<String> {
    let data = root.with_file_name("data");
    let mut store = Store::open(&data).expect("store opens");
    import_claude_code(root, "example-origin", &mut store).expect("import runs");
    drop(store);
    Indexer::open(&data)
        .expect("indexer opens")
        .sync()
        .expect("sync runs");
    Query::open(&data, Timestamp(0))
        .expect("query opens")
        .find(&Find {
            query: words.to_owned(),
            limit: 10,
            ..Find::default()
        })
        .expect("find runs")
        .into_iter()
        .map(|hit| hit.key)
        .collect()
}

fn projects() -> (TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("temp directory is created");
    let root = temp.path().join("projects");
    (temp, root)
}

const TITLED: &str = "1a2b3c4d-0000-4000-8000-00000000000a";
const UNTITLED: &str = "2b3c4d5e-0000-4000-8000-00000000000b";
const SHORT: &str = "3c4d5e6f-0000-4000-8000-00000000000c";
const LONG: &str = "4d5e6f70-0000-4000-8000-00000000000d";

#[test]
fn title_with_every_query_word_outranks_messages_that_repeat_them() {
    let (_temp, root) = projects();
    for (i, title) in [
        "Table layout fixes",
        "Report formatting",
        "Dark mode colors",
        "Release notes draft",
        "Staging deploy checklist",
        "Weekly planning",
    ]
    .iter()
    .enumerate()
    {
        session(
            &root,
            &format!("{i:08x}-0000-4000-8000-0000000000f{i}"),
            Some(title),
            &format!("2026-02-0{}T09:00", i + 1),
            &["Formatting tables in the report.", "Fixed the layout."],
        );
    }
    session(
        &root,
        TITLED,
        Some(
            "Notes on rotating the signing keys for the release pipeline, plus the other chores we planned for this week",
        ),
        "2026-03-01T09:00",
        &["Start with the staging environment."],
    );
    session(
        &root,
        UNTITLED,
        None,
        "2026-03-02T09:00",
        &[
            "Rotating signing keys: keep rotating, rotate signing keys, signing keys rotating keys signing",
            "Start with the staging environment.",
        ],
    );

    assert_eq!(
        find(&root, "rotating signing keys"),
        [
            format!("claude-code/{TITLED}"),
            format!("claude-code/{UNTITLED}")
        ],
        "a title that names the whole query beats a message that repeats it"
    );
}

#[test]
fn equal_evidence_in_a_shorter_session_outranks_a_much_longer_one() {
    let (_temp, root) = projects();
    let filler: Vec<String> = (0..120)
        .map(|i| format!("Unrelated step {i} about formatting tables."))
        .collect();
    let mut long: Vec<&str> = vec!["The cache eviction bug comes from a stale timer."];
    long.extend(filler.iter().map(String::as_str));
    session(
        &root,
        SHORT,
        None,
        "2026-03-01T09:00",
        &["The cache eviction bug comes from a stale timer.", "Fixed."],
    );
    session(&root, LONG, None, "2026-03-05T09:00", &long);

    assert_eq!(
        find(&root, "cache eviction timer"),
        [
            format!("claude-code/{SHORT}"),
            format!("claude-code/{LONG}")
        ],
        "the same matching message counts for more in the session that says less else"
    );
}

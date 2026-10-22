use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

const PROJECT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/claude-code/-home-dev-example-project"
);

type Line = Map<String, Value>;

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).expect("fixture directory is readable") {
        let path = entry.expect("directory entry is readable").path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn read_jsonl(path: &Path) -> Vec<Line> {
    fs::read_to_string(path)
        .expect("transcript is readable")
        .lines()
        .enumerate()
        .map(|(i, line)| match serde_json::from_str(line) {
            Ok(Value::Object(object)) => object,
            other => panic!(
                "{}:{} is not a JSON object: {other:?}",
                path.display(),
                i + 1
            ),
        })
        .collect()
}

fn read_json(path: &Path) -> Line {
    match serde_json::from_str(&fs::read_to_string(path).expect("JSON file is readable")) {
        Ok(Value::Object(object)) => object,
        other => panic!("{} is not a JSON object: {other:?}", path.display()),
    }
}

fn stem(path: &Path) -> &str {
    path.file_stem()
        .and_then(|s| s.to_str())
        .expect("UTF-8 file stem")
}

fn main_transcripts() -> Vec<PathBuf> {
    let transcripts: Vec<PathBuf> = fs::read_dir(PROJECT)
        .expect("project fixture exists")
        .map(|entry| entry.expect("directory entry is readable").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert!(!transcripts.is_empty(), "the project has a main transcript");
    transcripts
}

fn content_blocks(line: &Line) -> Vec<&Line> {
    line.get("message")
        .and_then(|message| message["content"].as_array())
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .collect()
}

#[test]
fn every_jsonl_line_is_an_object_with_a_type() {
    let jsonl: Vec<PathBuf> = files_under(Path::new(PROJECT))
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert_eq!(
        jsonl.len(),
        2,
        "one main transcript and one subagent transcript"
    );
    for path in jsonl {
        for (i, line) in read_jsonl(&path).iter().enumerate() {
            assert!(
                line.get("type").is_some_and(Value::is_string),
                "{}:{} has a string type",
                path.display(),
                i + 1
            );
        }
    }
}

#[test]
fn main_transcript_lines_belong_to_the_session_in_the_file_name() {
    for path in main_transcripts() {
        let session_id = stem(&path);
        let lines = read_jsonl(&path);
        let mut parent = Value::Null;
        for line in &lines {
            assert_eq!(
                line["sessionId"], session_id,
                "sessionId equals the file stem"
            );
            if line["type"] == "custom-title" {
                assert_eq!(line["customTitle"], "Add a JSON output flag");
                continue;
            }
            assert_eq!(line["isSidechain"], false, "main lines are not sidechains");
            assert_eq!(
                line["parentUuid"], parent,
                "parentUuid chains to the previous line"
            );
            parent = line["uuid"].clone();
        }
        for kind in ["user", "assistant", "attachment", "custom-title"] {
            assert!(
                lines.iter().any(|line| line["type"] == kind),
                "the main transcript has a {kind} line"
            );
        }
        let block_types: Vec<&Value> = lines
            .iter()
            .flat_map(content_blocks)
            .map(|block| &block["type"])
            .collect();
        for kind in ["text", "tool_use", "tool_result", "image"] {
            assert!(
                block_types.iter().any(|t| *t == kind),
                "the main transcript has a {kind} block"
            );
        }
    }
}

#[test]
fn subagent_transcripts_name_their_agent_and_parent_session() {
    for main in main_transcripts() {
        let session_id = stem(&main);
        let subagents = Path::new(PROJECT).join(session_id).join("subagents");
        let transcripts: Vec<PathBuf> = files_under(&subagents)
            .into_iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        assert!(
            !transcripts.is_empty(),
            "the session has a subagent transcript"
        );
        for path in transcripts {
            let agent_id = stem(&path)
                .strip_prefix("agent-")
                .expect("subagent file is named agent-<agentId>.jsonl");
            assert!(
                agent_id.len() == 17
                    && agent_id.starts_with('a')
                    && agent_id[1..]
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "agent id {agent_id} is a followed by 16 lowercase hex characters"
            );
            for line in read_jsonl(&path) {
                assert_eq!(line["isSidechain"], true, "subagent lines are sidechains");
                assert_eq!(line["agentId"], agent_id, "agentId equals the file name");
                assert_eq!(
                    line["sessionId"], session_id,
                    "sessionId is the parent session"
                );
            }
        }
    }
}

#[test]
fn subagent_meta_points_at_a_tool_use_in_the_main_transcript() {
    for main in main_transcripts() {
        let session_id = stem(&main);
        let tool_use_ids: Vec<Value> = read_jsonl(&main)
            .iter()
            .flat_map(content_blocks)
            .filter(|block| block["type"] == "tool_use")
            .map(|block| block["id"].clone())
            .collect();
        let metas: Vec<PathBuf> =
            files_under(&Path::new(PROJECT).join(session_id).join("subagents"))
                .into_iter()
                .filter(|path| path.to_string_lossy().ends_with(".meta.json"))
                .collect();
        assert!(!metas.is_empty(), "the session has a subagent meta file");
        for path in metas {
            let meta = read_json(&path);
            assert_eq!(meta["agentType"], "Explore");
            assert!(meta["description"].is_string(), "meta has a description");
            assert_eq!(meta["spawnDepth"], 1);
            assert_eq!(meta["model"], "opus");
            assert!(
                tool_use_ids.contains(&meta["toolUseId"]),
                "meta toolUseId {} is a tool_use id in the main transcript",
                meta["toolUseId"]
            );
        }
    }
}

#[test]
fn custom_title_file_holds_the_title() {
    for main in main_transcripts() {
        let title = read_json(
            &Path::new(PROJECT)
                .join(stem(&main))
                .join("custom-title.json"),
        );
        assert_eq!(title["customTitle"], "Add a JSON output flag");
    }
}

#[test]
fn persisted_output_stub_points_at_a_saved_file_in_the_fixture_tree() {
    let stubs: Vec<String> = main_transcripts()
        .iter()
        .flat_map(|path| read_jsonl(path))
        .flat_map(|line| {
            content_blocks(&line)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        })
        .filter(|block| block["type"] == "tool_result")
        .filter_map(|block| block["content"].as_str().map(str::to_owned))
        .filter(|content| content.starts_with("<persisted-output>\n"))
        .collect();
    assert_eq!(
        stubs.len(),
        1,
        "the main transcript has one persisted-output stub"
    );

    let stub = &stubs[0];
    let saved_path = stub
        .split_once("Full output saved to: ")
        .and_then(|(_, rest)| rest.lines().next())
        .expect("the stub names the saved file");
    let marker = saved_path
        .rfind("/tool-results/")
        .expect("the saved file is under tool-results/");
    let session_start = saved_path[..marker]
        .rfind('/')
        .expect("tool-results/ sits inside a session folder");
    let local = Path::new(PROJECT).join(&saved_path[session_start + 1..]);
    assert!(
        local.is_file(),
        "{} exists in the fixture tree",
        local.display()
    );

    let preview = stub
        .split_once("\n\nPreview (first 2KB):\n")
        .map(|(_, preview)| preview)
        .expect("the stub has a preview");
    let saved = fs::read_to_string(&local).expect("saved output is readable");
    assert!(
        saved.starts_with(preview),
        "the preview is the start of the saved file"
    );
}

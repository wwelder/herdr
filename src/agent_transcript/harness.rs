//! `agent.transcript` for Claude Code and Codex: their session JSONL read into
//! the same entries as omp's, paged by turns from the newest end.
//!
//! Unlike omp's indexed tree, a file is parsed whole once and afterwards only
//! its appended bytes; the cache keeps what each record contributes, not the
//! lines. Claude's file is a tree (`uuid`/`parentUuid`): only the chain of the
//! newest record is shown, so a rewound branch drops out. Codex 0.166+ writes
//! each finished thread item as an `item_completed` event, read here; older
//! rollouts without those events show nothing.

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use base64::Engine as _;
use serde_json::Value;

use super::normalize::{
    parse_iso8601_ms, truncate_chars, user_author, DETAIL_MAX_CHARS, SUMMARY_TEXT,
};
use super::reply::{claude_prompt, claude_text};
use super::{Page, PageRequest, TranscriptError, DEFAULT_TURNS, MAX_TURNS};
use crate::api::schema::{
    TranscriptEntry, TranscriptStepStatus, TranscriptTurnStatus, TranscriptWorking,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Harness {
    Claude,
    Codex,
}

/// Rendered entries one page holds at most; a longer turn is split.
const PAGE_ENTRY_BUDGET: usize = 1500;
/// Entries before an `after` cursor sent again, so a step that settled since
/// (Claude writes a tool's result after its call) is updated by id.
// ponytail: a step still running this many entries back stays shown as running until a reload.
const RESEND: usize = 64;
const CACHED_SESSIONS: usize = 8;
const INTENT_MAX_CHARS: usize = 200;

/// What one record contributes, in file order.
#[derive(Debug, Clone)]
enum Piece {
    Prompt {
        id: String,
        ts: u64,
        text: String,
    },
    Reply {
        id: String,
        ts: u64,
        text: String,
        last: bool,
    },
    Thinking {
        id: String,
        ts: u64,
        text: String,
    },
    Step {
        id: String,
        ts: u64,
        tool: String,
        intent: String,
        detail: Option<String>,
        status: Option<TranscriptStepStatus>,
    },
    /// A Claude tool result: settles the step with that call id.
    Result {
        call: String,
        error: bool,
    },
    End {
        id: String,
        ts: u64,
        status: TranscriptTurnStatus,
    },
    Summary {
        id: String,
        ts: u64,
        text: String,
    },
}

/// One parsed record: Claude's tree link (Codex has none) and its pieces.
#[derive(Debug)]
struct Record {
    uuid: Option<String>,
    parent: Option<String>,
    pieces: Vec<Piece>,
}

struct Session {
    path: PathBuf,
    harness: Harness,
    /// Bytes parsed so far, and an unfinished last line kept for the next read.
    read: u64,
    partial: Vec<u8>,
    records: Vec<Record>,
}

impl Session {
    fn new(path: &Path, harness: Harness) -> Self {
        Self {
            path: path.to_owned(),
            harness,
            read: 0,
            partial: Vec::new(),
            records: Vec::new(),
        }
    }

    /// Parse what was appended since the last read; a shorter file is a new one.
    fn refresh(&mut self) -> std::io::Result<()> {
        let mut file = std::fs::File::open(&self.path)?;
        let len = file.metadata()?.len();
        if len < self.read {
            *self = Self::new(&self.path, self.harness);
        }
        if len == self.read {
            return Ok(());
        }
        file.seek(SeekFrom::Start(self.read))?;
        let mut bytes = std::mem::take(&mut self.partial);
        file.take(len - self.read).read_to_end(&mut bytes)?;
        self.read = len;
        let complete = bytes
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |end| end + 1);
        self.partial = bytes.split_off(complete);
        for line in bytes.split(|&b| b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let record = match self.harness {
                Harness::Claude => claude_record(&value),
                Harness::Codex => codex_record(&value),
            };
            if let Some(record) = record {
                self.records.push(record);
            }
        }
        Ok(())
    }

    fn session_id(&self) -> String {
        self.path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The records on the shown branch, oldest first.
    fn visible(&self) -> Vec<&Record> {
        if self.harness == Harness::Codex {
            return self.records.iter().collect();
        }
        let parents: HashMap<&str, Option<&str>> = self
            .records
            .iter()
            .filter_map(|record| Some((record.uuid.as_deref()?, record.parent.as_deref())))
            .collect();
        let mut chain = HashSet::new();
        let mut next = self
            .records
            .iter()
            .rev()
            .find(|record| !record.pieces.is_empty())
            .and_then(|record| record.uuid.as_deref());
        while let Some(uuid) = next {
            if !chain.insert(uuid) {
                break;
            }
            next = parents.get(uuid).copied().flatten();
        }
        self.records
            .iter()
            .filter(|record| {
                record
                    .uuid
                    .as_deref()
                    .is_some_and(|uuid| chain.contains(uuid))
            })
            .collect()
    }

    fn entries(&self) -> Vec<TranscriptEntry> {
        let mut entries = Vec::new();
        let mut steps: HashMap<String, usize> = HashMap::new();
        let mut open: Vec<usize> = Vec::new();
        let settle = |entries: &mut Vec<TranscriptEntry>, at: usize, to: TranscriptStepStatus| {
            if let Some(TranscriptEntry::Step { status, .. }) = entries.get_mut(at) {
                *status = to;
            }
        };
        for piece in self.visible().into_iter().flat_map(|record| &record.pieces) {
            match piece.clone() {
                Piece::Prompt { id, ts, text } => {
                    let (author, author_name, text) = user_author(&text);
                    entries.push(TranscriptEntry::User {
                        id,
                        ts_ms: ts,
                        text,
                        author,
                        author_name,
                    });
                }
                Piece::Reply { id, ts, text, last } => entries.push(TranscriptEntry::Reply {
                    id,
                    ts_ms: ts,
                    text,
                    is_final: last,
                }),
                Piece::Thinking { id, ts, text } => entries.push(TranscriptEntry::Thinking {
                    id,
                    ts_ms: ts,
                    text,
                }),
                Piece::Step {
                    id,
                    ts,
                    tool,
                    intent,
                    detail,
                    status,
                } => {
                    if status.is_none() {
                        steps.insert(id.clone(), entries.len());
                        open.push(entries.len());
                    }
                    entries.push(TranscriptEntry::Step {
                        id,
                        ts_ms: ts,
                        tool,
                        intent,
                        status: status.unwrap_or(TranscriptStepStatus::Running),
                        detail,
                    });
                }
                Piece::Result { call, error } => {
                    if let Some(at) = steps.remove(&call) {
                        open.retain(|&step| step != at);
                        let to = if error {
                            TranscriptStepStatus::Error
                        } else {
                            TranscriptStepStatus::Ok
                        };
                        settle(&mut entries, at, to);
                    }
                }
                Piece::End { id, ts, status } => {
                    // A turn that ended leaves nothing running; an interrupted call failed.
                    for at in open.drain(..) {
                        let to = if status == TranscriptTurnStatus::Done {
                            TranscriptStepStatus::Ok
                        } else {
                            TranscriptStepStatus::Error
                        };
                        settle(&mut entries, at, to);
                    }
                    steps.clear();
                    entries.push(TranscriptEntry::TurnEnd {
                        id,
                        ts_ms: ts,
                        status,
                        error: None,
                    });
                }
                Piece::Summary { id, ts, text } => entries.push(TranscriptEntry::Summary {
                    id,
                    ts_ms: ts,
                    text,
                }),
            }
        }
        entries
    }
}

static CACHE: LazyLock<Mutex<Vec<Arc<Mutex<Session>>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn cached(path: &Path, harness: Harness) -> Arc<Mutex<Session>> {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let found = cache.iter().position(|session| {
        let session = session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        session.path == path && session.harness == harness
    });
    let session = match found {
        Some(position) => cache.remove(position),
        None => Arc::new(Mutex::new(Session::new(path, harness))),
    };
    cache.push(session.clone());
    if cache.len() > CACHED_SESSIONS {
        cache.remove(0);
    }
    session
}

/// One page of a Claude Code or Codex conversation.
pub(crate) fn read_page(
    path: &Path,
    harness: Harness,
    request: &PageRequest,
) -> Result<Page, TranscriptError> {
    if request.before.is_some() && request.after.is_some() {
        return Err(TranscriptError::InvalidRequest(
            "pass either before or after, not both",
        ));
    }
    if request.subagent.is_some() {
        return Err(TranscriptError::SubagentNotFound);
    }
    let limit = request.limit.unwrap_or(DEFAULT_TURNS).clamp(1, MAX_TURNS) as usize;
    let session = cached(path, harness);
    let mut session = session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Err(error) = session.refresh() {
        *session = Session::new(path, harness);
        return Err(error.into());
    }
    let session_id = session.session_id();
    let entries = session.entries();
    drop(session);
    page(&session_id, entries, request, limit)
}

fn page(
    session_id: &str,
    mut entries: Vec<TranscriptEntry>,
    request: &PageRequest,
    limit: usize,
) -> Result<Page, TranscriptError> {
    let len = entries.len();
    let (start, end, live) = match (&request.before, &request.after) {
        (Some(token), _) => {
            let (kind, at, id) = decode(token, session_id)?;
            if kind != 'b' || id_at(&entries, at) != Some(id.as_str()) {
                return Err(TranscriptError::CursorExpired);
            }
            (page_start(&entries, at, limit), at, false)
        }
        (None, Some(token)) => {
            let (kind, seen, tip) = decode(token, session_id)?;
            let tip_ok = match seen.checked_sub(1) {
                None => tip.is_empty(),
                Some(at) => id_at(&entries, at) == Some(tip.as_str()),
            };
            if kind != 'a' || seen > len || !tip_ok {
                return Err(TranscriptError::CursorExpired);
            }
            let start = turn_start(&entries, seen.saturating_sub(1))
                .max(seen.saturating_sub(RESEND))
                .min(seen);
            (start, len, true)
        }
        (None, None) => (page_start(&entries, len, limit), len, false),
    };
    let working = working(&entries);
    let before =
        (!live && start > 0).then(|| encode('b', session_id, start, entry_id(&entries[start])));
    let tip = len
        .checked_sub(1)
        .map_or("", |at| entry_id(&entries[at]))
        .to_owned();
    let after = encode('a', session_id, len, &tip);
    entries.truncate(end);
    entries.drain(..start);
    Ok(Page {
        session_id: session_id.to_owned(),
        entries,
        before,
        after,
        pending_ask: None,
        working,
        model: None,
        plan: None,
        recap: None,
        subagents: None,
    })
}

/// Where a page of `limit` turns ending before `end` starts, at most a budget back.
fn page_start(entries: &[TranscriptEntry], end: usize, limit: usize) -> usize {
    let mut turns = 0;
    let mut start = end;
    while start > 0 && end - start < PAGE_ENTRY_BUDGET {
        start -= 1;
        if matches!(entries[start], TranscriptEntry::User { .. }) {
            turns += 1;
            if turns == limit {
                break;
            }
        }
    }
    start
}

/// The prompt that began the turn holding `at`, else the start.
fn turn_start(entries: &[TranscriptEntry], at: usize) -> usize {
    entries[..=at.min(entries.len().saturating_sub(1))]
        .iter()
        .rposition(|entry| matches!(entry, TranscriptEntry::User { .. }))
        .unwrap_or(0)
}

/// The step running now: an unsettled one since the newest prompt or turn end.
fn working(entries: &[TranscriptEntry]) -> Option<TranscriptWorking> {
    for entry in entries.iter().rev() {
        match entry {
            TranscriptEntry::User { .. } | TranscriptEntry::TurnEnd { .. } => return None,
            TranscriptEntry::Step {
                ts_ms,
                tool,
                intent,
                status: TranscriptStepStatus::Running,
                ..
            } => {
                return Some(TranscriptWorking {
                    intent: intent.clone(),
                    tool: tool.clone(),
                    since_ms: *ts_ms,
                })
            }
            _ => {}
        }
    }
    None
}

fn id_at(entries: &[TranscriptEntry], at: usize) -> Option<&str> {
    entries.get(at).map(entry_id)
}

fn entry_id(entry: &TranscriptEntry) -> &str {
    match entry {
        TranscriptEntry::User { id, .. }
        | TranscriptEntry::Reply { id, .. }
        | TranscriptEntry::Thinking { id, .. }
        | TranscriptEntry::Step { id, .. }
        | TranscriptEntry::Note { id, .. }
        | TranscriptEntry::Plan { id, .. }
        | TranscriptEntry::Ask { id, .. }
        | TranscriptEntry::TurnEnd { id, .. }
        | TranscriptEntry::Summary { id, .. } => id,
    }
}

/// `h`, the kind (`b`efore / `a`fter), the session, a position and the id
/// there (`after`: the newest entry seen). Any other token is expired.
fn encode(kind: char, session_id: &str, at: usize, id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!("h\n{kind}\n{session_id}\n{at}\n{id}"))
}

fn decode(token: &str, session_id: &str) -> Result<(char, usize, String), TranscriptError> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .ok()
        .and_then(|raw| String::from_utf8(raw).ok())
        .ok_or(TranscriptError::CursorExpired)?;
    let mut fields = raw.splitn(5, '\n');
    match (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next().and_then(|at| at.parse().ok()),
        fields.next(),
    ) {
        (Some("h"), Some(kind @ ("a" | "b")), Some(session), Some(at), Some(id))
            if session == session_id =>
        {
            Ok((kind.chars().next().unwrap_or('a'), at, id.to_owned()))
        }
        _ => Err(TranscriptError::CursorExpired),
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn ts_of(record: &Value) -> u64 {
    record["timestamp"]
        .as_str()
        .and_then(parse_iso8601_ms)
        .unwrap_or(0)
}

fn intent(text: &str) -> String {
    truncate_chars(
        text.lines().next().unwrap_or_default().trim(),
        INTENT_MAX_CHARS,
    )
}

fn base_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// A Claude Code record. Records without a `uuid` are not part of the tree.
fn claude_record(record: &Value) -> Option<Record> {
    let uuid = record["uuid"].as_str()?.to_owned();
    let parent = record["parentUuid"]
        .as_str()
        .or_else(|| record["logicalParentUuid"].as_str())
        .map(str::to_owned);
    let mut pieces = Vec::new();
    if record["isSidechain"] != true && record["isMeta"] != true {
        let ts = ts_of(record);
        let message = &record["message"];
        let content = &message["content"];
        match record["type"].as_str() {
            // The summary itself is Claude's own context, not the owner's chat; the
            // divider only marks where it was compacted, as omp's does.
            Some("user") if record["isCompactSummary"] == true => pieces.push(Piece::Summary {
                id: uuid.clone(),
                ts,
                text: SUMMARY_TEXT.to_owned(),
            }),
            Some("user") => {
                let results: Vec<&Value> = content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|block| block["type"] == "tool_result")
                    .collect();
                if !results.is_empty() {
                    pieces.extend(results.into_iter().map(|block| Piece::Result {
                        call: block["tool_use_id"].as_str().unwrap_or_default().to_owned(),
                        error: block["is_error"] == true,
                    }));
                } else if claude_text(content).starts_with("[Request interrupted") {
                    pieces.push(Piece::End {
                        id: uuid.clone(),
                        ts,
                        status: TranscriptTurnStatus::Aborted,
                    });
                } else if let Some(text) = claude_prompt(content) {
                    pieces.push(Piece::Prompt {
                        id: uuid.clone(),
                        ts,
                        text,
                    });
                }
            }
            Some("assistant") => {
                let last = message["stop_reason"] == "end_turn";
                for (index, block) in content.as_array().into_iter().flatten().enumerate() {
                    let id = match index {
                        0 => uuid.clone(),
                        _ => format!("{uuid}:{index}"),
                    };
                    match block["type"].as_str() {
                        Some("text") => {
                            if let Some(text) = str_of(block, "text") {
                                pieces.push(Piece::Reply {
                                    id,
                                    ts,
                                    text: text.to_owned(),
                                    last,
                                });
                            }
                        }
                        Some("thinking") => {
                            if let Some(text) = str_of(block, "thinking") {
                                pieces.push(Piece::Thinking {
                                    id,
                                    ts,
                                    text: text.to_owned(),
                                });
                            }
                        }
                        Some("tool_use") => {
                            let name = block["name"].as_str().unwrap_or_default();
                            // `mcp__server__tool` reads as its tool.
                            let tool = name.rsplit("__").next().unwrap_or(name).to_owned();
                            let (intent, detail) = claude_step(&tool, &block["input"]);
                            pieces.push(Piece::Step {
                                id: block["id"].as_str().map_or(id, str::to_owned),
                                ts,
                                tool,
                                intent,
                                detail,
                                status: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
            Some("system") if record["subtype"] == "turn_duration" => pieces.push(Piece::End {
                id: uuid.clone(),
                ts,
                status: TranscriptTurnStatus::Done,
            }),
            _ => {}
        }
    }
    Some(Record {
        uuid: Some(uuid),
        parent,
        pieces,
    })
}

/// A Claude tool call's one-line intent and its command or path.
fn claude_step(tool: &str, input: &Value) -> (String, Option<String>) {
    let path = str_of(input, "file_path").or_else(|| str_of(input, "notebook_path"));
    let (intent_text, detail) = match tool {
        "Bash" => (
            str_of(input, "description").or_else(|| str_of(input, "command")),
            str_of(input, "command"),
        ),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => (path.map(base_name), path),
        "Grep" | "Glob" => (str_of(input, "pattern"), str_of(input, "path")),
        "WebFetch" => (str_of(input, "url"), None),
        "WebSearch" => (str_of(input, "query"), None),
        "Task" | "Agent" => (str_of(input, "description"), None),
        "TodoWrite" => (Some("Update the to-do list"), None),
        _ => (
            input
                .as_object()
                .and_then(|fields| fields.values().find_map(Value::as_str)),
            None,
        ),
    };
    (
        intent(intent_text.unwrap_or_default()),
        detail.map(|detail| truncate_chars(detail, DETAIL_MAX_CHARS)),
    )
}

/// A Codex rollout record: finished thread items and turn ends.
fn codex_record(record: &Value) -> Option<Record> {
    let payload = &record["payload"];
    let ts = ts_of(record);
    let piece = match (record["type"].as_str()?, payload["type"].as_str()?) {
        ("event_msg", "item_completed") => codex_item(&payload["item"], ts)?,
        ("event_msg", kind @ ("task_complete" | "turn_aborted")) => Piece::End {
            id: format!(
                "end:{}",
                payload["turn_id"]
                    .as_str()
                    .map_or_else(|| ts.to_string(), str::to_owned)
            ),
            ts,
            status: if kind == "task_complete" {
                TranscriptTurnStatus::Done
            } else {
                TranscriptTurnStatus::Aborted
            },
        },
        _ => return None,
    };
    Some(Record {
        uuid: None,
        parent: None,
        pieces: vec![piece],
    })
}

fn codex_item(item: &Value, ts: u64) -> Option<Piece> {
    let id = item["id"].as_str()?.to_owned();
    let texts = |key: &str| {
        item[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| part.as_str().or_else(|| part["text"].as_str()))
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let status = |ok: bool| {
        Some(if ok {
            TranscriptStepStatus::Ok
        } else {
            TranscriptStepStatus::Error
        })
    };
    let piece = match item["type"].as_str()? {
        "UserMessage" => Piece::Prompt {
            id,
            ts,
            text: texts("content"),
        },
        "AgentMessage" => Piece::Reply {
            id,
            ts,
            text: texts("content"),
            last: item["phase"] == "final_answer",
        },
        "Reasoning" => Piece::Thinking {
            id,
            ts,
            text: texts("summary_text"),
        },
        "CommandExecution" => {
            let command = item["command"]
                .as_array()
                .and_then(|argv| argv.last())
                .and_then(Value::as_str)
                .or_else(|| item["command"].as_str())
                .unwrap_or_default();
            Piece::Step {
                id,
                ts,
                tool: "exec".into(),
                intent: intent(command),
                detail: Some(truncate_chars(command, DETAIL_MAX_CHARS)),
                status: status(item["status"] == "completed" && item["exit_code"] == 0),
            }
        }
        "FileChange" => {
            let paths: Vec<&str> = item["changes"]
                .as_object()
                .map(|changes| changes.keys().map(String::as_str).collect())
                .unwrap_or_default();
            Piece::Step {
                id,
                ts,
                tool: "edit".into(),
                intent: intent(
                    &paths
                        .iter()
                        .map(|path| base_name(path))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                detail: Some(truncate_chars(&paths.join("\n"), DETAIL_MAX_CHARS)),
                status: status(item["status"] == "completed"),
            }
        }
        "ImageView" => {
            let path = item["path"].as_str().unwrap_or_default();
            Piece::Step {
                id,
                ts,
                tool: "view".into(),
                intent: intent(base_name(path)),
                detail: Some(truncate_chars(
                    path.trim_start_matches("file://"),
                    DETAIL_MAX_CHARS,
                )),
                status: status(true),
            }
        }
        _ => return None,
    };
    match &piece {
        Piece::Prompt { text, .. } | Piece::Reply { text, .. } | Piece::Thinking { text, .. }
            if text.is_empty() =>
        {
            None
        }
        _ => Some(piece),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn file(tag: &str, records: &[Value]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("herdr-harness-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let lines: String = records.iter().map(|record| format!("{record}\n")).collect();
        std::fs::write(&path, lines).unwrap();
        path
    }

    fn claude(
        uuid: &str,
        parent: Option<&str>,
        kind: &str,
        content: Value,
        stop: Option<&str>,
    ) -> Value {
        json!({
            "type": kind, "uuid": uuid, "parentUuid": parent,
            "timestamp": "2026-11-06T01:00:00.000Z",
            "message": {"role": kind, "content": content, "stop_reason": stop},
        })
    }

    fn kinds(entries: &[TranscriptEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                TranscriptEntry::User { text, .. } => format!("user {text}"),
                TranscriptEntry::Reply { text, is_final, .. } => format!("reply {text} {is_final}"),
                TranscriptEntry::Thinking { text, .. } => format!("thinking {text}"),
                TranscriptEntry::Step {
                    tool,
                    intent,
                    status,
                    ..
                } => format!("step {tool} {intent} {status:?}"),
                TranscriptEntry::TurnEnd { status, .. } => format!("end {status:?}"),
                TranscriptEntry::Summary { text, .. } => format!("summary {text}"),
                _ => "other".into(),
            })
            .collect()
    }

    #[test]
    fn a_compacted_claude_session_shows_a_divider_and_short_mcp_names() {
        let mcp = json!([{"type": "tool_use", "id": "t1", "name": "mcp__claude_ai_Gmail__search_threads",
            "input": {"query": "rclone"}}]);
        let mut summary = claude(
            "c1",
            None,
            "user",
            json!("This session is being continued from a previous conversation..."),
            None,
        );
        summary["isCompactSummary"] = json!(true);
        summary["logicalParentUuid"] = json!("old");
        let path = file(
            "compacted",
            &[
                summary,
                claude("u1", Some("c1"), "user", json!("go on"), None),
                claude("a1", Some("u1"), "assistant", mcp, Some("tool_use")),
            ],
        );
        let page = read_page(&path, Harness::Claude, &PageRequest::default()).unwrap();
        assert_eq!(
            kinds(&page.entries),
            [
                "summary Earlier messages were summarised",
                "user go on",
                "step search_threads rclone Running"
            ]
        );
    }

    #[test]
    fn a_claude_session_reads_as_turns_and_follows_appends() {
        let tool = json!([{"type": "tool_use", "id": "t1", "name": "Bash",
            "input": {"command": "cargo test", "description": "Run the tests"}}]);
        let mut records = vec![
            claude("u1", None, "user", json!("fix it"), None),
            claude(
                "a1",
                Some("u1"),
                "assistant",
                json!([{"type": "thinking", "thinking": "hm"}]),
                Some("tool_use"),
            ),
            claude("a2", Some("a1"), "assistant", tool, Some("tool_use")),
        ];
        let path = file("claude", &records);
        let first = read_page(&path, Harness::Claude, &PageRequest::default()).unwrap();
        assert_eq!(
            kinds(&first.entries),
            [
                "user fix it",
                "thinking hm",
                "step Bash Run the tests Running"
            ]
        );
        assert_eq!(
            first.working.as_ref().map(|w| w.intent.as_str()),
            Some("Run the tests")
        );
        assert!(first.before.is_none());

        records.push(claude(
            "r1",
            Some("a2"),
            "user",
            json!([{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]),
            None,
        ));
        records.push(claude(
            "a3",
            Some("r1"),
            "assistant",
            json!([{"type": "text", "text": "Fixed."}]),
            Some("end_turn"),
        ));
        records.push(
            json!({"type": "system", "subtype": "turn_duration", "uuid": "s1", "parentUuid": "a3"}),
        );
        let lines: String = records.iter().map(|record| format!("{record}\n")).collect();
        std::fs::write(&path, lines).unwrap();
        let next = read_page(
            &path,
            Harness::Claude,
            &PageRequest {
                after: Some(first.after.clone()),
                ..PageRequest::default()
            },
        )
        .unwrap();
        // The turn is sent again from its prompt, so the settled step updates by id.
        assert_eq!(
            kinds(&next.entries),
            [
                "user fix it",
                "thinking hm",
                "step Bash Run the tests Ok",
                "reply Fixed. true",
                "end Done"
            ]
        );
        assert!(next.working.is_none());
    }

    #[test]
    fn a_rewound_claude_branch_drops_out_and_old_cursors_expire() {
        let records = vec![
            claude("u1", None, "user", json!("one"), None),
            claude(
                "a1",
                Some("u1"),
                "assistant",
                json!([{"type": "text", "text": "first"}]),
                Some("end_turn"),
            ),
            claude("u2", Some("a1"), "user", json!("two"), None),
            // Rewound to after "one": a new prompt hangs off a1.
            claude("u3", Some("a1"), "user", json!("again"), None),
        ];
        let path = file("rewind", &records);
        let page = read_page(&path, Harness::Claude, &PageRequest::default()).unwrap();
        assert_eq!(
            kinds(&page.entries),
            ["user one", "reply first true", "user again"]
        );
        let stale = encode('a', &page.session_id, 4, "u2");
        assert!(matches!(
            read_page(
                &path,
                Harness::Claude,
                &PageRequest {
                    after: Some(stale),
                    ..PageRequest::default()
                }
            ),
            Err(TranscriptError::CursorExpired)
        ));
    }

    #[test]
    fn claude_pages_older_turns_by_cursor_and_skips_what_the_owner_did_not_type() {
        let mut records = Vec::new();
        let mut parent: Option<String> = None;
        for turn in 0..5 {
            let (u, a) = (format!("u{turn}"), format!("a{turn}"));
            records.push(claude(
                &u,
                parent.as_deref(),
                "user",
                json!(format!("prompt {turn}")),
                None,
            ));
            records.push(claude(
                &a,
                Some(&u),
                "assistant",
                json!([{"type": "text", "text": "done"}]),
                Some("end_turn"),
            ));
            parent = Some(a);
        }
        records.push(claude(
            "n",
            parent.as_deref(),
            "user",
            json!("<task-notification>x</task-notification>"),
            None,
        ));
        let path = file("pages", &records);
        let newest = read_page(
            &path,
            Harness::Claude,
            &PageRequest {
                limit: Some(2),
                ..PageRequest::default()
            },
        )
        .unwrap();
        assert_eq!(
            kinds(&newest.entries),
            [
                "user prompt 3",
                "reply done true",
                "user prompt 4",
                "reply done true"
            ]
        );
        let older = read_page(
            &path,
            Harness::Claude,
            &PageRequest {
                before: newest.before.clone(),
                limit: Some(2),
                ..PageRequest::default()
            },
        )
        .unwrap();
        assert_eq!(
            kinds(&older.entries),
            [
                "user prompt 1",
                "reply done true",
                "user prompt 2",
                "reply done true"
            ]
        );
    }

    #[test]
    fn a_codex_rollout_reads_its_finished_items() {
        let item = |item: Value| {
            json!({"timestamp": "2026-11-06T01:00:00.000Z", "type": "event_msg",
            "payload": {"type": "item_completed", "turn_id": "t", "item": item}})
        };
        let path = file(
            "codex",
            &[
                json!({"type": "session_meta", "payload": {"id": "x"}}),
                item(
                    json!({"type": "UserMessage", "id": "m1", "content": [{"type": "text", "text": "ship it"}]}),
                ),
                item(
                    json!({"type": "Reasoning", "id": "r1", "summary_text": [], "raw_content": []}),
                ),
                item(
                    json!({"type": "AgentMessage", "id": "m2", "phase": "commentary",
                "content": [{"type": "Text", "text": "Looking."}]}),
                ),
                item(
                    json!({"type": "CommandExecution", "id": "c1", "command": ["/bin/bash", "-lc", "git status"],
                "status": "completed", "exit_code": 1}),
                ),
                item(
                    json!({"type": "FileChange", "id": "f1", "status": "completed",
                "changes": {"/repo/src/main.rs": {"type": "update"}}}),
                ),
                item(
                    json!({"type": "AgentMessage", "id": "m3", "phase": "final_answer",
                "content": [{"type": "Text", "text": "Shipped."}]}),
                ),
                json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t"}}),
            ],
        );
        let page = read_page(&path, Harness::Codex, &PageRequest::default()).unwrap();
        assert_eq!(
            kinds(&page.entries),
            [
                "user ship it",
                "reply Looking. false",
                "step exec git status Error",
                "step edit main.rs Ok",
                "reply Shipped. true",
                "end Done",
            ]
        );
    }
}

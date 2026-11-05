//! The final reply of a Claude Code or Codex turn, for the owner's Gram recap.
//!
//! Both harnesses append their session as JSONL; a turn's reply is near the end,
//! so only the file's tail is read and walked backward to the turn's start. The
//! omp index in this module's parent parses omp's tree format, not these.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// How much of the session file's end is read. A turn's last records (the
/// reply, its stop record, a few metadata lines) are far smaller.
const TAIL_BYTES: u64 = 512 * 1024;

/// The newest reply of the session's current turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalReply {
    pub(crate) text: String,
    /// Identifies the record or turn the text came from, so one turn is
    /// recapped once.
    pub(crate) key: String,
    /// The harness has recorded the turn as finished. While false the text may
    /// be an interim message the turn's real reply has not followed yet.
    pub(crate) complete: bool,
    /// The owner's prompt that started the turn, when one did.
    pub(crate) prompt: Option<String>,
}

/// The current turn's final reply in a Claude Code transcript: the newest
/// assistant text since the owner's last prompt, joined with the other text
/// blocks of the same message. Complete once the newest assistant record ended
/// the turn (`end_turn`) and no tool result followed it.
pub(crate) fn claude(path: &Path) -> std::io::Result<Option<FinalReply>> {
    let tail = read_tail(path)?;
    let mut reply: Option<(String, String, Vec<String>)> = None;
    let mut sealed = false;
    let mut complete = None;
    let mut prompt = None;
    for record in records_newest_first(&tail) {
        if record["isSidechain"] == true || record["isMeta"] == true {
            continue;
        }
        let message = &record["message"];
        match record["type"].as_str() {
            Some("assistant") => {
                complete.get_or_insert(message["stop_reason"] == "end_turn");
                if sealed {
                    continue;
                }
                let id = message["id"].as_str().unwrap_or_default();
                let text = claude_text(&message["content"]);
                match &mut reply {
                    None if !text.is_empty() => {
                        let uuid = record["uuid"].as_str().unwrap_or(id).to_owned();
                        reply = Some((uuid, id.to_owned(), vec![text]));
                    }
                    Some((_, reply_id, texts)) if !id.is_empty() && reply_id == id => {
                        if !text.is_empty() {
                            texts.push(text);
                        }
                    }
                    // An earlier message of the turn: the reply is whole; walk on to the prompt.
                    Some(_) => sealed = true,
                    None => {}
                }
            }
            Some("user") => {
                let content = &message["content"];
                let tool_result = content.as_array().is_some_and(|blocks| {
                    blocks.iter().any(|block| block["type"] == "tool_result")
                });
                if !tool_result {
                    // The owner's prompt: the turn started here.
                    prompt = claude_prompt(content);
                    break;
                }
                complete.get_or_insert(false);
            }
            _ => {}
        }
    }
    Ok(reply.map(|(key, _, mut texts)| {
        texts.reverse();
        FinalReply {
            text: texts.join("\n\n"),
            key: format!("claude:{key}"),
            complete: complete.unwrap_or(false),
            prompt,
        }
    }))
}

/// The current turn's final reply in a Codex rollout: `task_complete`'s
/// `last_agent_message`, else (before the turn completes, or a turn that ended
/// without one) the newest assistant message since the turn started.
pub(crate) fn codex(path: &Path) -> std::io::Result<Option<FinalReply>> {
    let tail = read_tail(path)?;
    let mut completed_turn: Option<String> = None;
    for record in records_newest_first(&tail) {
        let payload = &record["payload"];
        match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("task_complete")) if completed_turn.is_none() => {
                let turn = payload["turn_id"].as_str().unwrap_or_default().to_owned();
                if let Some(text) = payload["last_agent_message"]
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    return Ok(Some(FinalReply {
                        text: text.to_owned(),
                        key: format!("codex:{turn}"),
                        complete: true,
                        // ponytail: codex's prompt is not read; newer rollouts mix it with injected context.
                        prompt: None,
                    }));
                }
                completed_turn = Some(turn);
            }
            (Some("event_msg"), Some("task_started" | "user_message")) => break,
            (Some("response_item"), Some("message")) if payload["role"] == "assistant" => {
                let text = payload["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|block| block["type"] == "output_text")
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let key = match &completed_turn {
                    Some(turn) => turn.clone(),
                    None => payload["id"].as_str().unwrap_or_default().to_owned(),
                };
                return Ok(Some(FinalReply {
                    text: text.to_owned(),
                    key: format!("codex:{key}"),
                    complete: completed_turn.is_some(),
                    prompt: None,
                }));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// What the owner typed in a turn-starting user record: its text, or a slash command as
/// `/name args`. None for what Claude Code wrote there itself (a background task's
/// result, a local command's output, an interrupt notice).
fn claude_prompt(content: &Value) -> Option<String> {
    let text = claude_text(content);
    if let Some(name) = tagged(&text, "command-name") {
        let args = tagged(&text, "command-args").unwrap_or_default();
        return Some(format!("{name} {args}").trim_end().to_owned());
    }
    (!text.is_empty() && !text.starts_with('<') && !text.starts_with("[Request interrupted"))
        .then_some(text)
}

/// The trimmed text between `<name>` and `</name>`.
fn tagged<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = text.find(&format!("<{name}>"))? + name.len() + 2;
    let end = start + text[start..].find(&format!("</{name}>"))?;
    Some(text[start..end].trim())
}

/// The text blocks of one Claude message's content.
fn claude_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.trim().to_owned(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn read_tail(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    // A tail read starts mid-line; that partial first line is not a record.
    if start > 0 {
        let first_line = text.find('\n').map_or(text.len(), |end| end + 1);
        text.drain(..first_line);
    }
    Ok(text)
}

fn records_newest_first(tail: &str) -> impl Iterator<Item = Value> + '_ {
    tail.lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn session(tag: &str, records: &[Value]) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herdr-final-reply-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let lines: String = records.iter().map(|record| format!("{record}\n")).collect();
        std::fs::write(&path, lines).unwrap();
        path
    }

    fn claude_assistant(uuid: &str, id: &str, content: Value, stop: &str) -> Value {
        json!({
            "type": "assistant",
            "uuid": uuid,
            "isSidechain": false,
            "message": {"id": id, "role": "assistant", "content": content, "stop_reason": stop},
        })
    }

    fn claude_user(content: Value) -> Value {
        json!({"type": "user", "isSidechain": false, "message": {"role": "user", "content": content}})
    }

    #[test]
    fn claude_reply_is_the_turns_final_text_once_it_ends_the_turn() {
        let path = session(
            "claude-done",
            &[
                claude_user(json!("old prompt")),
                claude_assistant(
                    "u0",
                    "m0",
                    json!([{"type": "text", "text": "old answer"}]),
                    "end_turn",
                ),
                claude_user(json!("fix the build")),
                claude_assistant(
                    "u1",
                    "m1",
                    json!([{"type": "text", "text": "Let me look."}]),
                    "tool_use",
                ),
                claude_assistant(
                    "u2",
                    "m1",
                    json!([{"type": "tool_use", "name": "Bash"}]),
                    "tool_use",
                ),
                claude_user(json!([{"type": "tool_result", "content": "ok"}])),
                claude_assistant(
                    "u3",
                    "m2",
                    json!([{"type": "thinking", "thinking": "..."}]),
                    "end_turn",
                ),
                claude_assistant(
                    "u4",
                    "m2",
                    json!([{"type": "text", "text": "Fixed the build."}]),
                    "end_turn",
                ),
                claude_assistant(
                    "u5",
                    "m2",
                    json!([{"type": "text", "text": "Tests pass."}]),
                    "end_turn",
                ),
                json!({"type": "assistant", "isSidechain": true, "message": {"id": "s", "content": [{"type": "text", "text": "subagent chatter"}], "stop_reason": "end_turn"}}),
                json!({"type": "system", "subtype": "stop_hook_summary"}),
                json!({"type": "last-prompt"}),
            ],
        );
        assert_eq!(
            claude(&path).unwrap(),
            Some(FinalReply {
                text: "Fixed the build.\n\nTests pass.".into(),
                key: "claude:u5".into(),
                complete: true,
                prompt: Some("fix the build".into()),
            })
        );
    }

    #[test]
    fn a_claude_prompt_is_what_the_owner_typed() {
        assert_eq!(
            claude_prompt(&json!("fix the build")),
            Some("fix the build".into())
        );
        assert_eq!(
            claude_prompt(
                &json!([{"type": "image"}, {"type": "text", "text": "[Image #1] this one"}])
            ),
            Some("[Image #1] this one".into())
        );
        assert_eq!(
            claude_prompt(&json!("<command-message>loop</command-message>\n<command-name>/loop</command-name>\n<command-args>10m check</command-args>")),
            Some("/loop 10m check".into())
        );
        assert_eq!(
            claude_prompt(&json!(
                "<command-name>/model</command-name>\n<command-args></command-args>"
            )),
            Some("/model".into())
        );
        assert_eq!(
            claude_prompt(&json!("<task-notification>\n<task-id>b1</task-id>")),
            None
        );
        assert_eq!(
            claude_prompt(&json!([{"type": "text", "text": "[Request interrupted by user]"}])),
            None
        );
    }

    #[test]
    fn claude_reply_mid_turn_is_incomplete_and_never_crosses_the_prompt() {
        let path = session(
            "claude-mid",
            &[
                claude_assistant(
                    "u0",
                    "m0",
                    json!([{"type": "text", "text": "old answer"}]),
                    "end_turn",
                ),
                claude_user(json!([{"type": "text", "text": "new prompt"}])),
                claude_assistant(
                    "u1",
                    "m1",
                    json!([{"type": "text", "text": "Checking."}]),
                    "tool_use",
                ),
                claude_assistant(
                    "u2",
                    "m1",
                    json!([{"type": "tool_use", "name": "Bash"}]),
                    "tool_use",
                ),
                claude_user(json!([{"type": "tool_result", "content": "ok"}])),
            ],
        );
        let reply = claude(&path).unwrap().unwrap();
        assert_eq!(reply.text, "Checking.");
        assert!(!reply.complete);

        let no_reply = session(
            "claude-none",
            &[
                claude_assistant(
                    "u0",
                    "m0",
                    json!([{"type": "text", "text": "old"}]),
                    "end_turn",
                ),
                claude_user(json!("new prompt")),
            ],
        );
        assert_eq!(claude(&no_reply).unwrap(), None);
    }

    fn codex_message(id: &str, text: &str) -> Value {
        json!({"type": "response_item", "payload": {"type": "message", "id": id, "role": "assistant", "content": [{"type": "output_text", "text": text}]}})
    }

    #[test]
    fn codex_reply_is_the_completed_turns_last_agent_message() {
        let path = session(
            "codex-done",
            &[
                json!({"type": "event_msg", "payload": {"type": "task_started"}}),
                codex_message("msg_1", "Looking."),
                codex_message("msg_2", "Done: two icons."),
                json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t1", "last_agent_message": "Done: two icons."}}),
                json!({"type": "token_usage_record"}),
            ],
        );
        assert_eq!(
            codex(&path).unwrap(),
            Some(FinalReply {
                text: "Done: two icons.".into(),
                key: "codex:t1".into(),
                complete: true,
                prompt: None,
            })
        );
    }

    #[test]
    fn codex_reply_before_task_complete_is_incomplete() {
        let path = session(
            "codex-mid",
            &[
                json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t0", "last_agent_message": "old"}}),
                json!({"type": "event_msg", "payload": {"type": "task_started"}}),
                codex_message("msg_9", "Working on it."),
            ],
        );
        let reply = codex(&path).unwrap().unwrap();
        assert_eq!(reply.text, "Working on it.");
        assert_eq!(reply.key, "codex:msg_9");
        assert!(!reply.complete);

        let started = session(
            "codex-started",
            &[
                json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t0", "last_agent_message": "old"}}),
                json!({"type": "event_msg", "payload": {"type": "user_message", "message": "go"}}),
            ],
        );
        assert_eq!(codex(&started).unwrap(), None);
    }

    #[test]
    fn a_tail_read_drops_the_partial_first_line() {
        let filler = "x".repeat(TAIL_BYTES as usize);
        let path = session(
            "tail",
            &[
                json!({"type": "event_msg", "payload": {"type": "agent_message", "message": filler}}),
                json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t2", "last_agent_message": "tail ok"}}),
            ],
        );
        assert_eq!(codex(&path).unwrap().unwrap().text, "tail ok");
    }
}

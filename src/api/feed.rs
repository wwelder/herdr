//! HerdrUp's feed: cards scripts write to `<config>/feeds/<source>/<id>.json`, and
//! the owner's answers to them. `feed.list` serves the cards unparsed (the app reads
//! them); `feed.act` records an answer in `<id>.reply.json` and runs the source's
//! `on-reply` handler. The script owns the card and herdr owns the reply, so the two
//! never write the same file.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::api::schema::{
    ErrorBody, ErrorResponse, FeedActParams, FeedCardInfo, ResponseResult, SuccessResponse,
};

/// A card is served whole, up to this size; a larger one is listed with an error.
const CARD_MAX_BYTES: u64 = 256 * 1024;
// ponytail: newest-written first with a flat cap, so a flood of info cards could push an
// old unanswered one out; rank answerable cards first if that ever happens.
const MAX_CARDS: usize = 300;
/// How long `on-reply` may run before it is stopped and the answer withdrawn.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(60);
/// The most of the handler's output kept, from its end.
const OUTPUT_MAX_CHARS: usize = 2000;
const REPLY_SUFFIX: &str = ".reply.json";

pub(crate) fn feeds_dir() -> PathBuf {
    crate::config::config_dir().join("feeds")
}

/// `feed.list`: every `<source>/<id>.json` under `dir`, most recently written first, with
/// its reply. Hidden files (a script's `.id.json.tmp` before its rename) are skipped, so a
/// card is never read half written. A missing folder is an empty list.
pub(crate) fn list(dir: &Path) -> Vec<FeedCardInfo> {
    let mut found: Vec<(SystemTime, FeedCardInfo)> = Vec::new();
    for source in names(dir) {
        let source_dir = dir.join(&source);
        if !source_dir.is_dir() {
            continue;
        }
        for file in names(&source_dir) {
            let Some(id) = file.strip_suffix(".json") else {
                continue;
            };
            if file.ends_with(REPLY_SUFFIX) {
                continue;
            }
            let path = source_dir.join(&file);
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let (card, error) = match read_capped(&path) {
                Ok(text) => (Some(text), None),
                Err(error) => (None, Some(error)),
            };
            let reply =
                std::fs::read_to_string(source_dir.join(format!("{id}{REPLY_SUFFIX}"))).ok();
            found.push((
                modified,
                FeedCardInfo {
                    source: source.clone(),
                    id: id.to_owned(),
                    card,
                    reply,
                    error,
                },
            ));
        }
    }
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.truncate(MAX_CARDS);
    found.into_iter().map(|(_, card)| card).collect()
}

/// `feed.act`, answered on the API connection thread so a slow handler never holds up
/// the app loop.
pub(crate) fn act_response(request_id: String, params: &FeedActParams) -> String {
    match act(
        &feeds_dir(),
        params,
        HANDLER_TIMEOUT,
        OffsetDateTime::now_utc(),
    ) {
        Ok(output) => serde_json::to_string(&SuccessResponse {
            id: request_id,
            result: ResponseResult::FeedAct { output },
        }),
        Err(error) => serde_json::to_string(&ErrorResponse {
            id: request_id,
            error,
        }),
    }
    .expect("feed.act response serializes")
}

/// What `feed.act` checks of a card; the app reads the rest.
#[derive(Deserialize)]
struct Card {
    #[serde(default)]
    actions: Vec<CardAction>,
    /// The text field's placeholder: present when the card takes text.
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    expires: Option<String>,
}

#[derive(Deserialize)]
struct CardAction {
    id: String,
}

/// Checks the answer against the card, writes `<id>.reply.json` once, then runs
/// `<source>/on-reply <card path>` with the reply on stdin. A handler that fails or
/// overruns `timeout` withdraws the reply, so the card is still open to a retry.
fn act(
    dir: &Path,
    params: &FeedActParams,
    timeout: Duration,
    now: OffsetDateTime,
) -> Result<Option<String>, ErrorBody> {
    if !is_name(&params.source) || !is_name(&params.id) || params.id.ends_with(".reply") {
        return Err(error(
            "invalid_params",
            "source and id must be plain file names",
        ));
    }
    let source_dir = dir.join(&params.source);
    let card_path = source_dir.join(format!("{}.json", params.id));
    let text = read_capped(&card_path).map_err(|message| {
        error(
            "feed_card_not_found",
            format!("{}/{}: {message}", params.source, params.id),
        )
    })?;
    let card: Card = serde_json::from_str(&text)
        .map_err(|parse| error("feed_card_invalid", parse.to_string()))?;
    let text = params
        .text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    match &params.action {
        Some(action) if !card.actions.iter().any(|offered| &offered.id == action) => {
            return Err(error(
                "feed_invalid_action",
                format!("the card has no action {action:?}"),
            ));
        }
        None if text.is_none() => {
            return Err(error("invalid_params", "an action or text is required"));
        }
        _ => {}
    }
    if text.is_some() && card.input.is_none() {
        return Err(error("invalid_params", "the card takes no text"));
    }
    if let Some(expires) = &card.expires {
        let expires = OffsetDateTime::parse(expires, &Rfc3339)
            .map_err(|parse| error("feed_card_invalid", format!("expires: {parse}")))?;
        if now >= expires {
            return Err(error("feed_expired", "the card has expired"));
        }
    }

    let reply = serde_json::to_vec(&serde_json::json!({
        "action": params.action,
        "text": text,
        // Whole seconds: every ISO 8601 reader takes them.
        "at": now.replace_nanosecond(0).unwrap_or(now).format(&Rfc3339).unwrap_or_default(),
    }))
    .expect("reply serializes");
    let reply_path = source_dir.join(format!("{}{REPLY_SUFFIX}", params.id));
    write_once(&reply_path, &reply)?;

    let handler = source_dir.join("on-reply");
    if !is_executable(&handler) {
        return Ok(None);
    }
    run_handler(&handler, &card_path, &reply, timeout).map_err(|message| {
        let _ = std::fs::remove_file(&reply_path);
        error("feed_handler_failed", message)
    })
}

/// Writes `bytes` to `path` only if nothing is there yet, atomically: a temp file is
/// hard-linked into place, which fails when `path` exists. A double tap can't answer
/// twice, and a script never reads half a reply.
fn write_once(path: &Path, bytes: &[u8]) -> Result<(), ErrorBody> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("reply");
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let temp = path.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    std::fs::write(&temp, bytes).map_err(|write| error("feed_write_failed", write.to_string()))?;
    let linked = std::fs::hard_link(&temp, path);
    let _ = std::fs::remove_file(&temp);
    match linked {
        Ok(()) => Ok(()),
        Err(link) if link.kind() == std::io::ErrorKind::AlreadyExists => Err(error(
            "feed_already_answered",
            "this card was already answered",
        )),
        Err(link) => Err(error("feed_write_failed", link.to_string())),
    }
}

fn run_handler(
    handler: &Path,
    card: &Path,
    reply: &[u8],
    timeout: Duration,
) -> Result<Option<String>, String> {
    let mut child = Command::new(handler)
        .arg(card)
        .current_dir(handler.parent().unwrap_or(Path::new("/")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|spawn| format!("on-reply didn't start: {spawn}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A few hundred bytes: the pipe holds them even if the handler never reads.
        let _ = stdin.write_all(reply);
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "on-reply ran longer than {} s and was stopped",
                    timeout.as_secs()
                ));
            }
            Err(wait) => return Err(wait.to_string()),
        }
    };
    // Something the handler left running in the background may still hold its pipes.
    let collect = |pipe: mpsc::Receiver<String>| {
        pipe.recv_timeout(Duration::from_secs(2))
            .unwrap_or_default()
    };
    let (stdout, stderr) = (collect(stdout), collect(stderr));
    if status.success() {
        let output = tail(&stdout);
        return Ok((!output.is_empty()).then_some(output));
    }
    let said = if stderr.trim().is_empty() {
        &stdout
    } else {
        &stderr
    };
    Err(format!("on-reply failed ({status}): {}", tail(said)))
}

/// Reads a pipe to its end (at most 1 MiB) on its own thread, so a chatty handler never
/// blocks on a full pipe while we wait for it.
fn drain(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe {
            let _ = pipe.take(1024 * 1024).read_to_end(&mut bytes);
        }
        let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    rx
}

fn tail(text: &str) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= OUTPUT_MAX_CHARS {
        return text.to_owned();
    }
    format!(
        "…{}",
        text.chars()
            .skip(count - OUTPUT_MAX_CHARS)
            .collect::<String>()
    )
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// A plain file name: not empty, not hidden, no separators.
fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
}

/// Visible entry names of `dir`, sorted; none when it can't be read.
fn names(dir: &Path) -> Vec<String> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read_dir
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

fn read_capped(path: &Path) -> Result<String, String> {
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => Err("not a file".to_owned()),
        Ok(meta) if meta.len() > CARD_MAX_BYTES => {
            Err(format!("larger than {} KiB", CARD_MAX_BYTES / 1024))
        }
        Ok(_) => std::fs::read_to_string(path).map_err(|read| read.to_string()),
        Err(missing) => Err(missing.to_string()),
    }
}

fn error(code: &str, message: impl Into<String>) -> ErrorBody {
    ErrorBody {
        code: code.to_owned(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("herdr-feed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn params(id: &str, action: Option<&str>, text: Option<&str>) -> FeedActParams {
        FeedActParams {
            source: "kg".to_owned(),
            id: id.to_owned(),
            action: action.map(str::to_owned),
            text: text.map(str::to_owned),
        }
    }

    fn at(rfc3339: &str) -> OffsetDateTime {
        OffsetDateTime::parse(rfc3339, &Rfc3339).unwrap()
    }

    const NOW: &str = "2026-11-05T12:00:00Z";
    const CARD: &str = r#"{"title":"Add?","actions":[{"id":"add","label":"Add"},{"id":"skip","label":"Skip"}],"input":"Why?","expires":"2026-11-06T00:00:00Z"}"#;

    #[test]
    fn list_serves_cards_with_their_replies_newest_first() {
        let dir = temp_dir("list");
        let kg = dir.join("kg");
        std::fs::create_dir_all(&kg).unwrap();
        std::fs::create_dir_all(dir.join("oura")).unwrap();
        std::fs::write(kg.join("a.json"), CARD).unwrap();
        std::fs::write(kg.join("a.reply.json"), r#"{"action":"add"}"#).unwrap();
        std::fs::write(kg.join(".b.json.tmp"), "{").unwrap();
        std::fs::write(kg.join("on-reply"), "#!/bin/sh").unwrap();
        std::fs::write(dir.join("oura").join("night.json"), "{}").unwrap();
        std::fs::write(
            dir.join("oura").join("big.json"),
            vec![b' '; CARD_MAX_BYTES as usize + 1],
        )
        .unwrap();
        std::fs::write(dir.join("stray.json"), "{}").unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        std::fs::File::options()
            .write(true)
            .open(kg.join("a.json"))
            .unwrap()
            .set_modified(old)
            .unwrap();

        let cards = list(&dir);
        let keys: Vec<String> = cards
            .iter()
            .map(|card| format!("{}/{}", card.source, card.id))
            .collect();
        assert_eq!(keys, vec!["oura/big", "oura/night", "kg/a"]);
        assert_eq!(cards[0].error.as_deref(), Some("larger than 256 KiB"));
        assert_eq!(cards[1].card.as_deref(), Some("{}"));
        assert_eq!(cards[1].reply, None);
        assert_eq!(cards[2].reply.as_deref(), Some(r#"{"action":"add"}"#));
        assert!(list(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn act_records_one_answer_and_refuses_what_the_card_does_not_offer() {
        let dir = temp_dir("act");
        let kg = dir.join("kg");
        std::fs::create_dir_all(&kg).unwrap();
        std::fs::write(kg.join("a.json"), CARD).unwrap();
        std::fs::write(kg.join("info.json"), r#"{"title":"Sleep"}"#).unwrap();
        let act = |p: FeedActParams, now: &str| act(&dir, &p, HANDLER_TIMEOUT, at(now));
        let code = |result: Result<Option<String>, ErrorBody>| result.unwrap_err().code;

        assert_eq!(
            code(act(params("../a", Some("add"), None), NOW)),
            "invalid_params"
        );
        assert_eq!(
            code(act(params("nope", Some("add"), None), NOW)),
            "feed_card_not_found"
        );
        assert_eq!(
            code(act(params("a", Some("buy"), None), NOW)),
            "feed_invalid_action"
        );
        assert_eq!(
            code(act(params("a", None, Some("  ")), NOW)),
            "invalid_params"
        );
        assert_eq!(
            code(act(params("info", None, Some("hi")), NOW)),
            "invalid_params"
        );
        assert_eq!(
            code(act(params("a", Some("add"), None), "2026-11-06T00:00:00Z")),
            "feed_expired"
        );

        assert_eq!(
            act(
                params("a", Some("skip"), Some(" dup ")),
                "2026-11-05T12:00:00.25Z"
            )
            .unwrap(),
            None
        );
        let reply: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(kg.join("a.reply.json")).unwrap())
                .unwrap();
        assert_eq!(reply["action"], "skip");
        assert_eq!(reply["text"], "dup");
        assert_eq!(reply["at"], "2026-11-05T12:00:00Z");
        assert_eq!(
            code(act(params("a", Some("add"), None), NOW)),
            "feed_already_answered"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_or_slow_handler_withdraws_the_answer() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("handler");
        let kg = dir.join("kg");
        std::fs::create_dir_all(&kg).unwrap();
        std::fs::write(kg.join("a.json"), CARD).unwrap();
        let handler = kg.join("on-reply");
        let install = |script: &str| {
            std::fs::write(&handler, script).unwrap();
            std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let run = |timeout| act(&dir, &params("a", Some("add"), None), timeout, at(NOW));

        install("#!/bin/sh\necho broker down >&2\nexit 3\n");
        let failed = run(HANDLER_TIMEOUT).unwrap_err();
        assert_eq!(failed.code, "feed_handler_failed");
        assert!(
            failed.message.ends_with("broker down"),
            "{}",
            failed.message
        );
        assert!(!kg.join("a.reply.json").exists());

        install("#!/bin/sh\nsleep 5\n");
        assert_eq!(
            run(Duration::from_millis(200)).unwrap_err().code,
            "feed_handler_failed"
        );
        assert!(!kg.join("a.reply.json").exists());

        install("#!/bin/sh\nread reply\necho \"$1\" >/dev/null\necho \"ordered: $reply\"\n");
        let output = run(HANDLER_TIMEOUT).unwrap().unwrap();
        assert!(
            output.starts_with("ordered: {\"action\":\"add\""),
            "{output}"
        );
        assert!(kg.join("a.reply.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

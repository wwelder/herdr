//! One Live Activity for the whole machine. Each herdr session is its own server, and all of
//! them push to the same registered activities, so a server alone would show only its own
//! session. Instead every server records its own aggregate as `live-activity/<pid>.json`
//! under the config dir and pushes the merge of every live server's file.
//!
//! The lowest live pid also measures host APM every [`TICK`]: Claude + Codex tool calls per
//! minute over the last five minutes, summed over every user's home on the machine, counted
//! from their transcripts with the same rules as the wezterm status bar's `ai-usage --apm`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const TICK: Duration = Duration::from_secs(20);
const APM_WINDOW_SECS: u64 = 5 * 60;
// ponytail: reads only the last 8 MiB of each transcript touched in the window; an agent
// that writes more than that in five minutes is undercounted.
const TAIL_BYTES: u64 = 8 << 20;

fn dir() -> PathBuf {
    crate::config::config_dir().join("live-activity")
}

/// Record this server's own content-state, then return the merge of every live server's
/// with the host APM (`null` when no server has measured it in the last minute).
// Tests push this session alone; see `emit_live_activity`.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn merge_with_peers(own: &Value) -> Value {
    let dir = dir();
    let me = std::process::id();
    write_atomic(&dir.join(format!("{me}.json")), &own.to_string());
    let mut states = vec![own.clone()];
    for (pid, path) in peers(&dir) {
        if pid == me {
            continue;
        }
        if !alive(pid) {
            let _ = std::fs::remove_file(path);
        } else if let Some(state) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(Value::is_object)
        {
            states.push(state);
        }
    }
    let mut merged = merge(&states);
    merged["apm"] = read_apm(&dir).map_or(Value::Null, Value::from);
    merged
}

/// Counts add up; the headline, status and timer come from the session whose lead ranks
/// highest (needs you, then working, then idle), as one server already picks its own.
fn merge(states: &[Value]) -> Value {
    fn rank(state: &Value) -> u8 {
        match state["status"].as_str() {
            Some("needsYou") => 0,
            Some("working") => 1,
            _ => 2,
        }
    }
    let sum = |key: &str| states.iter().filter_map(|s| s[key].as_u64()).sum::<u64>();
    let mut merged = states
        .iter()
        .filter(|s| s["totalCount"].as_u64().unwrap_or(0) > 0)
        .min_by_key(|s| rank(s))
        .or(states.first())
        .cloned()
        .unwrap_or_else(
            || json!({ "headline": "No agents", "status": "idle", "workingSince": null }),
        );
    for key in ["needsYouCount", "workingCount", "totalCount"] {
        merged[key] = sum(key).into();
    }
    merged
}

/// Start the ticker that measures APM (on the lowest live pid only) and asks the app loop
/// to push whenever it changes. Every server also pushes once at start, so a session that
/// stays quiet after a restart or live handoff is still in the merge.
pub(crate) fn start(events: tokio::sync::mpsc::Sender<crate::events::AppEvent>) {
    let spawned = crate::thread_spawn::spawn_named("herdr-fleet", move || {
        let mut last: Option<String> = None;
        let mut first = true;
        let mut sudo = true;
        loop {
            let mut changed = std::mem::take(&mut first);
            if !crate::persist::activities::load().is_empty() && is_leader(&dir()) {
                if let Some(home) = std::env::var_os("HOME") {
                    let apm = format!("{:.1}", host_apm(Path::new(&home), &mut sudo));
                    // Rewritten every tick: its age is how peers know a leader is alive.
                    write_atomic(&dir().join("apm"), &apm);
                    changed |= last.as_ref() != Some(&apm);
                    last = Some(apm);
                }
            }
            if changed
                && events
                    .blocking_send(crate::events::AppEvent::LiveActivityTick)
                    .is_err()
            {
                return;
            }
            std::thread::sleep(TICK);
        }
    });
    if let Err(err) = spawned {
        tracing::warn!(error = %err, "failed to start the live-activity ticker");
    }
}

/// Our own APM plus every other home beside ours. Other users' transcripts are 0600, so
/// they are counted by `herdr fleet-apm` through `sudo -n`; on a machine without
/// passwordless sudo the first refusal leaves only our own.
// ponytail: one sudo per tick (an auth-log line every 20 s); a long-lived helper if that matters.
fn host_apm(home: &Path, sudo: &mut bool) -> f64 {
    let own = apm(home, unix_secs());
    let others: Vec<PathBuf> = home
        .parent()
        .and_then(|parent| std::fs::read_dir(parent).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path != home && path.is_dir())
        .collect();
    if !*sudo || others.is_empty() {
        return own;
    }
    let counted = std::env::current_exe().ok().and_then(|exe| {
        let out = std::process::Command::new("sudo")
            .arg("-n")
            .arg(exe)
            .arg("fleet-apm")
            .args(&others)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status.success().then_some(())?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<f64>()
            .ok()
    });
    *sudo = counted.is_some();
    own + counted.unwrap_or(0.0)
}

/// `herdr fleet-apm <home>...`: the APM of these homes together, printed for a leader that
/// cannot read them itself.
pub(crate) fn print_apm(homes: &[String]) {
    let now = unix_secs();
    let total: f64 = homes.iter().map(|home| apm(Path::new(home), now)).sum();
    println!("{total:.1}");
}

fn is_leader(dir: &Path) -> bool {
    let me = std::process::id();
    peers(dir)
        .into_iter()
        .all(|(pid, _)| pid >= me || !alive(pid))
}

// Tests push this session alone; see `emit_live_activity`.
#[cfg_attr(test, allow(dead_code))]
fn read_apm(dir: &Path) -> Option<f64> {
    let path = dir.join("apm");
    let age = std::fs::metadata(&path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?;
    if age > TICK * 3 {
        return None;
    }
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn peers(dir: &Path) -> Vec<(u32, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let pid = path
                .file_name()?
                .to_str()?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            Some((pid, path))
        })
        .collect()
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    crate::update::state::process_alive(pid)
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    true
}

fn write_atomic(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(tmp);
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Tool calls per minute over the last [`APM_WINDOW_SECS`] across `~/.claude/projects` and
/// `~/.codex/sessions`.
fn apm(home: &Path, now_secs: u64) -> f64 {
    let since = now_secs.saturating_sub(APM_WINDOW_SECS);
    // Transcripts stamp UTC as "2026-11-02T10:11:12.345Z"; the second-resolution prefix
    // compares correctly as a string.
    let Ok(cutoff) = time::OffsetDateTime::from_unix_timestamp(since as i64)
        .map_err(|_| ())
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| ())
        })
    else {
        return 0.0;
    };
    let cutoff = &cutoff[..19.min(cutoff.len())];
    let since = UNIX_EPOCH + Duration::from_secs(since);
    let mut calls = 0;
    for root in [".claude/projects", ".codex/sessions"] {
        count_tree(&home.join(root), since, cutoff, &mut calls);
    }
    calls as f64 / (APM_WINDOW_SECS as f64 / 60.0)
}

fn count_tree(dir: &Path, since: SystemTime, cutoff: &str, calls: &mut usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            count_tree(&path, since, cutoff, calls);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.modified().is_ok_and(|modified| modified >= since) {
                *calls += count_file(&path, meta.len(), cutoff);
            }
        }
    }
}

fn count_file(path: &Path, len: u64, cutoff: &str) -> usize {
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let start = len.saturating_sub(TAIL_BYTES);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return 0;
    }
    let text = String::from_utf8_lossy(&bytes);
    // A tail read starts mid-line; that partial first line is not JSON.
    text.lines()
        .skip(usize::from(start > 0))
        .filter(|line| line.contains("\"tool_use\"") || line.contains("_call\""))
        .map(|line| calls_in(line, cutoff))
        .sum()
}

fn calls_in(line: &str, cutoff: &str) -> usize {
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return 0;
    };
    if record["timestamp"].as_str().unwrap_or("") < cutoff {
        return 0;
    }
    if record["type"] == "response_item" {
        // Codex
        return usize::from(matches!(
            record["payload"]["type"].as_str(),
            Some("function_call" | "custom_tool_call")
        ));
    }
    // Claude Code
    record["message"]["content"]
        .as_array()
        .map_or(0, |content| {
            content.iter().filter(|c| c["type"] == "tool_use").count()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apm_counts_recent_claude_and_codex_tool_calls() {
        let home = std::env::temp_dir().join(format!("herdr-fleet-{}", std::process::id()));
        let claude = home.join(".claude/projects/p");
        let codex = home.join(".codex/sessions/2026/11/02");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        let now = 1_793_613_600; // 2026-11-02T10:00:00Z
        std::fs::write(
            claude.join("a.jsonl"),
            concat!(
                r#"{"timestamp":"2026-11-02T09:58:00.100Z","message":{"content":[{"type":"tool_use"},{"type":"tool_use"},{"type":"text"}]}}"#, "\n",
                r#"{"timestamp":"2026-11-02T09:50:00.000Z","message":{"content":[{"type":"tool_use"}]}}"#, "\n",
                "not json \"tool_use\"\n",
            ),
        )
        .unwrap();
        std::fs::write(
            codex.join("b.jsonl"),
            concat!(
                r#"{"timestamp":"2026-11-02T09:59:00.000Z","type":"response_item","payload":{"type":"function_call"}}"#, "\n",
                r#"{"timestamp":"2026-11-02T09:59:01.000Z","type":"response_item","payload":{"type":"function_call_output"}}"#, "\n",
            ),
        )
        .unwrap();
        // Two Claude calls and one Codex call inside the window: 3 calls / 5 min.
        assert_eq!(apm(&home, now), 0.6);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn merge_sums_counts_and_leads_with_the_most_urgent_session() {
        let quiet = json!({"headline": "No agents", "status": "idle", "needsYouCount": 0,
                           "workingCount": 0, "totalCount": 0, "workingSince": null});
        let busy = json!({"headline": "api", "status": "working", "needsYouCount": 0,
                          "workingCount": 2, "totalCount": 3, "workingSince": 5.0});
        let blocked = json!({"headline": "deploy", "status": "needsYou", "needsYouCount": 1,
                             "workingCount": 1, "totalCount": 4, "workingSince": null,
                             "question": "Do you want to proceed?", "agentID": "w2-1",
                             "session": "bot-r"});
        let merged = merge(&[quiet.clone(), busy.clone(), blocked]);
        assert_eq!(merged["headline"], "deploy");
        assert_eq!(merged["question"], "Do you want to proceed?");
        assert_eq!(merged["agentID"], "w2-1");
        assert_eq!(merged["session"], "bot-r");
        assert_eq!(merged["needsYouCount"], 1);
        assert_eq!(merged["workingCount"], 3);
        assert_eq!(merged["totalCount"], 7);
        assert_eq!(merge(&[quiet.clone(), busy])["headline"], "api");
        assert_eq!(merge(&[quiet])["headline"], "No agents");
    }
}

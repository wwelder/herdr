use std::io::Read;

use crate::api::schema::{FeedAskParams, Method, Request};

const USAGE: &str = "usage: herdr feed ask [--source NAME] < card.json";

/// `herdr feed ask` — ask the owner something on their phone. The card is JSON on stdin
/// (`title`, markdown `body`, `actions` [{id, label, role?: "destructive"}] and/or
/// `input` as the reply field's placeholder, optional `confirm` and RFC 3339 `expires`).
/// The owner gets a time-sensitive alert that opens the card; their answer comes back to
/// this pane (`HERDR_PANE_ID`) as a prompt.
pub(super) fn run_feed_command(args: &[String]) -> std::io::Result<i32> {
    let source = match args {
        [ask] if ask == "ask" => None,
        [ask, flag, source] if ask == "ask" && flag == "--source" => Some(source.clone()),
        _ => {
            eprintln!("{USAGE}");
            return Ok(2);
        }
    };
    let mut text = String::new();
    std::io::stdin()
        .take(256 * 1024 + 1)
        .read_to_string(&mut text)?;
    let card: serde_json::Value = match serde_json::from_str(&text) {
        Ok(card) => card,
        Err(parse) => {
            eprintln!("the card on stdin is not JSON: {parse}\n{USAGE}");
            return Ok(2);
        }
    };
    super::print_response(&super::send_request(&Request {
        id: format!("cli:feed:ask:{}", crate::persist::gram::new_id()),
        method: Method::FeedAsk(FeedAskParams {
            card,
            source,
            caller_pane_id: super::gram::env_pane_id(),
            caller_pids: Vec::new(),
        }),
    })?)
}

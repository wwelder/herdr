use serde::{Deserialize, Serialize};

/// One card returned by `feed.list`: `<config>/feeds/<source>/<id>.json`, a JSON
/// object a script wrote for HerdrUp's feed. `card` is the file's text, unparsed,
/// or absent with an `error` when it can't be served. `reply` is the text of the
/// owner's answer (`<id>.reply.json`, written by `feed.act`), when there is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeedCardInfo {
    pub source: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Parameters for `feed.ask` — an agent asks the owner something: `card` is a feed card
/// (`title`, `body`, `actions` and/or `input`, ...) that herdr writes as
/// `<source>/<id>.json` (`source` defaults to `agents`) and pushes to the owner's phone
/// as a time-sensitive alert. The owner's answer is typed back into the asking pane
/// (`caller_pane_id`) as a prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeedAskParams {
    pub card: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pane_id: Option<String>,
    /// Set by the server, never the client: the calling process and its ancestors. The
    /// answer goes back to `caller_pane_id` only if that pane's process is among them,
    /// since an agent harness can run commands with another pane's environment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caller_pids: Vec<u32>,
}

/// Parameters for `feed.act` — the owner answers card `id` of `source`: one of the
/// card's `actions` by id, and/or `text` when the card takes `input`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeedActParams {
    pub source: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

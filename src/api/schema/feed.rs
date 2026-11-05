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

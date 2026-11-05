//! What a Claude Code agent still runs in the background while it waits at its prompt,
//! read off the task pill in its footer, below the prompt box: "1 shell", "3 local
//! agents" (subagents), "2 monitors", "2 background tasks" for a mix. Carried to the phone
//! in `agent.list` so an agent busy in the background counts as running.

use std::sync::LazyLock;

/// Claude Code's pill wordings (its background-task summary), one or a ", " list of them.
static PILL: LazyLock<regex::Regex> = LazyLock::new(|| {
    let kind = r"\d+ (?:shells?|monitors?|teams?|local agents?|cloud sessions?|MCP tasks?|background tasks?|(?:background|remote) dynamic workflows?|Artifact comment monitors?)";
    regex::Regex::new(&format!(r"(?:^|·)\s*({kind}(?:, {kind})*)\s*(?:·|$)")).expect("valid regex")
});

/// The pill's text from the footer under the last rule on `visible`, `None` without one.
// ponytail: a screen heuristic over Claude Code's footer; a renamed pill reads as nothing.
pub(crate) fn background_work(visible: &str) -> Option<String> {
    let lines: Vec<&str> = visible.lines().collect();
    let footer = match lines
        .iter()
        .rposition(|line| super::pending_request::is_rule(line))
    {
        Some(rule) => &lines[rule + 1..],
        None => return None,
    };
    footer
        .iter()
        .find_map(|line| PILL.captures(line.trim()).map(|found| found[1].to_string()))
}

#[cfg(test)]
mod tests {
    use super::background_work;

    #[test]
    fn reads_the_footer_pill_under_the_prompt_box() {
        let screen = "● Done. 20 workers are running.\n\n❯ \n────────────\n  -- INSERT -- ⏵⏵ bypass permissions on · 1 shell\n";
        assert_eq!(background_work(screen).as_deref(), Some("1 shell"));
        let mixed = "❯ \n────────────\n  ⏵⏵ accept edits on · 2 shells, 1 monitor · ↓ to manage\n";
        assert_eq!(
            background_work(mixed).as_deref(),
            Some("2 shells, 1 monitor")
        );
        let agents = "❯ \n────────────\n  3 local agents\n";
        assert_eq!(background_work(agents).as_deref(), Some("3 local agents"));
    }

    #[test]
    fn nothing_without_a_pill_or_above_the_prompt() {
        let idle =
            "❯ \n────────────\n  -- INSERT -- ⏵⏵ bypass permissions on (shift+tab to cycle)\n";
        assert_eq!(background_work(idle), None);
        // Words in the agent's answer above the prompt box are not the pill.
        let answer = "● It started 2 shells earlier.\n❯ \n────────────\n  ? for shortcuts\n";
        assert_eq!(background_work(answer), None);
    }
}

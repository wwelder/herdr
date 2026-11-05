//! The question or request a blocked agent is showing, read off its visible screen: the
//! lines above a numbered choice menu (Claude Code permission prompts and AskUserQuestion,
//! Codex approvals, omp menus). Carried to the phone in `agent.list`, the push alert body
//! and the Live Activity.

use std::sync::LazyLock;

const MAX_LINES: usize = 5;
const MAX_CHARS: usize = 280;

/// Box sides and left bars that frame a prompt's lines.
const BORDERS: &[char] = &['│', '┃', '║', '|', '▌', '▎', '▍'];

const HINTS: &[&str] = &[
    "esc to cancel",
    "enter to select",
    "enter to confirm",
    "tab to amend",
    "ctrl+e to explain",
    "to navigate",
    "esc to interrupt",
];

static FIRST_OPTION: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^(?:[❯›>▶➤]\s*)?1[.)]\s+\S").expect("valid regex"));

/// The lines above the LAST "1." row of a menu on `visible`, top to bottom, joined with
/// `\n` and capped at [`MAX_CHARS`]. `None` when the screen shows no numbered menu or
/// nothing readable sits above it.
// ponytail: a screen heuristic; a menu whose question is wider than five lines, or a
// numbered list printed in an agent's answer while it is blocked, reads wrong.
pub(crate) fn pending_request(visible: &str) -> Option<String> {
    let lines: Vec<&str> = visible.lines().collect();
    let first_option = lines
        .iter()
        .rposition(|line| FIRST_OPTION.is_match(clean(line)))?;
    let mut collected = Vec::new();
    for line in lines[..first_option].iter().rev() {
        if is_rule(line) {
            break;
        }
        let text = clean(line);
        let lowered = text.to_lowercase();
        if text.is_empty() || HINTS.iter().any(|hint| lowered.contains(hint)) {
            continue;
        }
        collected.push(text);
        if collected.len() == MAX_LINES {
            break;
        }
    }
    collected.reverse();
    let joined = collected.join("\n");
    match joined.char_indices().nth(MAX_CHARS) {
        _ if joined.is_empty() => None,
        Some((cut, _)) => Some(format!("{}…", joined[..cut].trim_end())),
        None => Some(joined),
    }
}

fn clean(line: &str) -> &str {
    line.trim()
        .trim_start_matches(BORDERS)
        .trim_end_matches(BORDERS)
        .trim()
}

/// A horizontal rule or a box's top or bottom edge: only box-drawing characters.
pub(super) fn is_rule(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= 3
        && line
            .chars()
            .all(|c| ('\u{2500}'..='\u{257F}').contains(&c) || matches!(c, '-' | '=' | '▔' | '▁'))
}

#[cfg(test)]
mod tests {
    use super::pending_request;

    #[test]
    fn boxed_claude_permission_prompt() {
        let screen = "\
● I'll push the branch now.

╭───────────────────────────────────────────────────────────╮
│ Bash command                                              │
│                                                           │
│   git push origin main                                    │
│   Push the branch to origin                               │
│                                                           │
│ Do you want to proceed?                                   │
│ ❯ 1. Yes                                                  │
│   2. Yes, and don't ask again for git push commands       │
│   3. No, and tell Claude what to do differently (esc)     │
╰───────────────────────────────────────────────────────────╯
";
        assert_eq!(
            pending_request(screen).as_deref(),
            Some("Bash command\ngit push origin main\nPush the branch to origin\nDo you want to proceed?")
        );
    }

    #[test]
    fn borderless_claude_prompt_under_a_rule() {
        let screen = "\
● Reading the config first.
────────────────────────────────────────────────────────────
 Bash command

   rm -rf build
   Clean the build directory

 Do you want to proceed?
 ❯ 1. Yes
   2. No, and tell Claude what to do differently (esc)

 Esc to cancel · Tab to amend · ctrl+e to explain
";
        assert_eq!(
            pending_request(screen).as_deref(),
            Some("Bash command\nrm -rf build\nClean the build directory\nDo you want to proceed?")
        );
    }

    #[test]
    fn claude_ask_user_question() {
        let screen = "\
────────────────────────────────────────────────────────────
 ☐ Approach

Which approach should I take?

❯ 1. Option A
     Rewrite the parser from scratch
  2. Option B
     Patch the existing parser
  3. Type something.

Enter to select · Tab/Arrow keys to navigate · Esc to cancel
";
        assert_eq!(
            pending_request(screen).as_deref(),
            Some("☐ Approach\nWhich approach should I take?")
        );
    }

    #[test]
    fn codex_approval() {
        let screen = "\
Would you like to run the following command?

  $ git push

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for this command (a)
  3. No, and tell Codex what to do differently (esc)
";
        assert_eq!(
            pending_request(screen).as_deref(),
            Some("Would you like to run the following command?\n$ git push")
        );
    }

    #[test]
    fn no_menu_and_long_questions() {
        assert_eq!(pending_request("● Done. All tests pass.\n\n> \n"), None);
        assert_eq!(pending_request("❯ 1. Yes\n  2. No\n"), None);
        let long = format!("{}\n❯ 1. Yes\n", "é".repeat(400));
        let request = pending_request(&long).unwrap();
        assert_eq!(request.chars().count(), 281);
        assert!(request.ends_with('…'));
    }
}

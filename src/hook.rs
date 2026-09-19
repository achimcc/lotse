//! `lotse hook claude`: a PreToolUse hook for Claude Code that puts
//! `lotse run --class=… --` in front of the heavy commands of a Bash call.
//!
//! A rule in a CLAUDE.md is a thing to remember, and a dozen sessions forget
//! it a dozen times. The hook makes it a property of the tool call.
//!
//! The rewrite is conservative: whatever this scanner does not understand
//! stays exactly as it was. A command that runs unqueued is observed anyway;
//! a command that runs mangled is a bug somebody has to find.

use serde_json::{Value, json};

use crate::config::Config;

/// Where in the text a wrapper goes, and for which class.
#[derive(Debug, PartialEq)]
struct Insert {
    at: usize,
    class: String,
}

const KEYWORDS: &[&str] = &[
    "then", "do", "else", "elif", "if", "while", "until", "!", "time", "{",
];

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The simple command that starts at `from`, cut at the first character that
/// could end it. Cutting inside a quoted string only shortens the text the
/// patterns see; it never lets them read into the NEXT command.
fn segment(text: &str, from: usize) -> &str {
    let rest = &text[from..];
    let end = rest.find([';', '|', '&', ')', '\n']).unwrap_or(rest.len());
    &rest[..end]
}

fn class_of(cfg: &Config, segment: &str) -> Option<String> {
    cfg.classes
        .iter()
        .filter(|(_, c)| c.wrap)
        .find(|(_, c)| {
            c.observe.iter().any(|re| re.is_match(segment))
                && !c.ignore.iter().any(|re| re.is_match(segment))
        })
        .map(|(name, _)| name.clone())
}

/// The command positions that match a wrapped class. `None`: the text holds
/// something this scanner does not follow, so nothing may be touched.
fn inserts(cfg: &Config, text: &str) -> Option<Vec<Insert>> {
    #[derive(PartialEq)]
    enum Ctx {
        Paren,
        DQuote,
    }
    let bytes = text.as_bytes();
    let mut stack: Vec<Ctx> = Vec::new();
    let mut out = Vec::new();
    // True while the next word would be a command name.
    let mut command_position = true;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if stack.last() == Some(&Ctx::DQuote) {
            match c {
                b'\\' => i += 1,
                b'"' => {
                    stack.pop();
                }
                b'`' => return None,
                b'$' if bytes.get(i + 1) == Some(&b'(') => {
                    if bytes.get(i + 2) == Some(&b'(') {
                        return None;
                    }
                    stack.push(Ctx::Paren);
                    command_position = true;
                    i += 1;
                }
                _ => {}
            }
            i += 1;
            continue;
        }
        match c {
            b' ' | b'\t' => i += 1,
            b'\n' | b';' => {
                command_position = true;
                i += 1;
            }
            b'|' | b'&' => {
                // `2>&1` and `>&2` are redirections, not the end of a command.
                let redirection = c == b'&' && i > 0 && bytes[i - 1] == b'>';
                if !redirection {
                    command_position = true;
                }
                i += 1;
            }
            b'(' => {
                stack.push(Ctx::Paren);
                command_position = true;
                i += 1;
            }
            b')' => {
                if stack.pop() != Some(Ctx::Paren) {
                    return None;
                }
                command_position = false;
                i += 1;
            }
            b'#' if command_position || i == 0 || bytes[i - 1].is_ascii_whitespace() => {
                // A comment runs to the end of the line.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'`' => return None,
            b'<' if bytes.get(i + 1) == Some(&b'<') => {
                // A here-document: its body is data, and a line in it that
                // starts with `nix build` must not be rewritten.
                return None;
            }
            _ => {
                // A word. Find its end, honouring quotes and substitutions.
                let start = i;
                if command_position {
                    let seg = segment(text, start);
                    let word = seg.split_whitespace().next().unwrap_or("");
                    if KEYWORDS.contains(&word) || is_assignment(word) {
                        // Still in front of the command name.
                    } else {
                        if let Some(class) = class_of(cfg, seg) {
                            out.push(Insert { at: start, class });
                        }
                        command_position = false;
                    }
                }
                while i < bytes.len() {
                    match bytes[i] {
                        b' ' | b'\t' | b'\n' | b';' | b'|' | b'&' | b'(' | b')' => break,
                        b'\\' => i += 2,
                        b'\'' => {
                            i += 1;
                            while i < bytes.len() && bytes[i] != b'\'' {
                                i += 1;
                            }
                            if i >= bytes.len() {
                                return None;
                            }
                            i += 1;
                        }
                        b'"' => {
                            stack.push(Ctx::DQuote);
                            i += 1;
                            break;
                        }
                        b'`' => return None,
                        b'$' if bytes.get(i + 1) == Some(&b'(') => {
                            if bytes.get(i + 2) == Some(&b'(') {
                                return None;
                            }
                            stack.push(Ctx::Paren);
                            command_position = true;
                            i += 2;
                            break;
                        }
                        b'<' if bytes.get(i + 1) == Some(&b'<') => return None,
                        _ => i += 1,
                    }
                }
                if i > bytes.len() {
                    return None;
                }
            }
        }
    }
    stack.is_empty().then_some(out)
}

/// The command with the wrappers in place, and the classes that were wrapped.
/// `None`: nothing to do, or nothing that may safely be done.
pub fn rewrite(cfg: &Config, command: &str) -> Option<(String, Vec<String>)> {
    let found = inserts(cfg, command)?;
    if found.is_empty() {
        return None;
    }
    let mut text = command.to_string();
    // From the back, so that the earlier offsets stay valid.
    for ins in found.iter().rev() {
        // `--class=x`, one word: some sandboxes refuse a bare `eval` word.
        text.insert_str(ins.at, &format!("lotse run --class={} -- ", ins.class));
    }
    let mut classes: Vec<String> = found.into_iter().map(|i| i.class).collect();
    classes.dedup();
    Some((text, classes))
}

/// What Claude Code gets back for a PreToolUse event, or `None` to stay
/// silent (which leaves the call exactly as it was).
pub fn claude_pre_tool_use(cfg: &Config, event: &Value) -> Option<Value> {
    if event["tool_name"] != "Bash" {
        return None;
    }
    let input = event["tool_input"].as_object()?;
    let command = input.get("command")?.as_str()?;
    let (rewritten, classes) = rewrite(cfg, command)?;
    let mut updated = input.clone();
    updated.insert("command".into(), Value::String(rewritten));
    // No permissionDecision: the rewritten command goes through the same
    // permission flow as any other. This hook queues, it does not approve.
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": updated,
            "additionalContext": format!(
                "lotse: this call was put behind the other sessions' runs \
                 (lotse run --class={}). It may wait for memory; `lotse status` shows for whom. \
                 Exit code 200 means the wait limit passed, 201 means every attempt died of the \
                 network: neither is a verdict of the command. The last line of the output \
                 (`lotse: exit=…`) carries the command's real exit code.",
                classes.join(", ")
            ),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::parse(
            r#"
            [class.eval]
            wrap = true
            observe = ['^(\S*/)?nix (build|eval)\b.*nixosConfigurations', '^(\S*/)?nix flake check\b']

            [class.deploy]
            observe = ['^(\S*/)?colmena apply\b']
            "#,
        )
        .unwrap()
    }

    fn rw(cmd: &str) -> Option<String> {
        rewrite(&cfg(), cmd).map(|(text, _)| text)
    }

    const W: &str = "lotse run --class=eval -- ";

    #[test]
    fn a_plain_build_is_wrapped() {
        assert_eq!(
            rw("nix build .#nixosConfigurations.server.x --no-link"),
            Some(format!(
                "{W}nix build .#nixosConfigurations.server.x --no-link"
            ))
        );
        assert_eq!(rw("nix flake check"), Some(format!("{W}nix flake check")));
    }

    #[test]
    fn other_commands_are_left_alone() {
        assert_eq!(rw("nix build .#gast-pruefung"), None);
        assert_eq!(rw("ls -la"), None);
        // Not a wrapped class.
        assert_eq!(rw("colmena apply --on server"), None);
    }

    #[test]
    fn inside_a_compound_command() {
        assert_eq!(
            rw("cd /x && nix build .#nixosConfigurations.vps.y 2>&1 | tail -5; echo done"),
            Some(format!(
                "cd /x && {W}nix build .#nixosConfigurations.vps.y 2>&1 | tail -5; echo done"
            ))
        );
    }

    #[test]
    fn after_assignments_and_keywords() {
        assert_eq!(
            rw("FOO=1 BAR=2 nix flake check"),
            Some(format!("FOO=1 BAR=2 {W}nix flake check"))
        );
        assert_eq!(
            rw("if nix flake check; then echo ok; fi"),
            Some(format!("if {W}nix flake check; then echo ok; fi"))
        );
    }

    #[test]
    fn in_a_command_substitution_and_a_subshell() {
        assert_eq!(
            rw("out=$(nix build .#nixosConfigurations.server.x --print-out-paths)"),
            Some(format!(
                "out=$({W}nix build .#nixosConfigurations.server.x --print-out-paths)"
            ))
        );
        assert_eq!(
            rw("( nix flake check ) &"),
            Some(format!("( {W}nix flake check ) &"))
        );
        assert_eq!(
            rw("echo \"path: $(nix eval --raw .#nixosConfigurations.server.p)\""),
            Some(format!(
                "echo \"path: $({W}nix eval --raw .#nixosConfigurations.server.p)\""
            ))
        );
    }

    #[test]
    fn text_that_only_mentions_a_build_is_not_a_command() {
        assert_eq!(rw("echo nix flake check"), None);
        assert_eq!(rw("echo 'nix flake check'"), None);
        assert_eq!(rw("echo \"nix flake check\""), None);
        assert_eq!(
            rw("pgrep -f 'nix eval --raw .#nixosConfigurations.server'"),
            None
        );
        assert_eq!(
            rw("ssh server 'nix build .#nixosConfigurations.server.x'"),
            None
        );
        assert_eq!(rw("true # nix flake check"), None);
        assert_eq!(rw("git commit -m 'x; nix flake check'"), None);
    }

    #[test]
    fn what_is_wrapped_already_stays() {
        let once = format!("{W}nix flake check");
        assert_eq!(rw(&once), None);
        assert_eq!(rw("nix develop --command nix flake check"), None);
    }

    #[test]
    fn a_match_does_not_reach_into_the_next_command() {
        assert_eq!(rw("nix build .#foo; echo nixosConfigurations"), None);
    }

    #[test]
    fn what_the_scanner_does_not_follow_is_left_alone() {
        assert_eq!(rw("cat > f <<EOF\nnix flake check\nEOF"), None);
        assert_eq!(rw("echo `nix flake check`"), None);
        assert_eq!(rw("nix flake check; echo 'unbalanced"), None);
        assert_eq!(rw("nix flake check; echo $((1+2))"), None);
    }

    #[test]
    fn text_beyond_ascii_is_walked_without_harm() {
        assert_eq!(
            rw("echo „Prüfung läuft“ && nix flake check # ärgerlich"),
            Some(format!(
                "echo „Prüfung läuft“ && {W}nix flake check # ärgerlich"
            ))
        );
        assert_eq!(rw("größe=1 ü"), None);
        assert_eq!(rw("\\"), None);
        assert_eq!(rw("echo \\"), None);
    }

    #[test]
    fn several_commands_in_one_call() {
        assert_eq!(
            rw("nix flake check && nix build .#nixosConfigurations.vps.y"),
            Some(format!(
                "{W}nix flake check && {W}nix build .#nixosConfigurations.vps.y"
            ))
        );
    }

    #[test]
    fn the_event_keeps_every_other_field_and_approves_nothing() {
        let event = json!({
            "tool_name": "Bash",
            "tool_input": {"command": "nix flake check", "timeout": 600000, "description": "gate"},
        });
        let out = claude_pre_tool_use(&cfg(), &event).unwrap();
        let hook = &out["hookSpecificOutput"];
        assert_eq!(
            hook["updatedInput"]["command"],
            format!("{W}nix flake check")
        );
        assert_eq!(hook["updatedInput"]["timeout"], 600000);
        assert_eq!(hook["updatedInput"]["description"], "gate");
        assert!(hook.get("permissionDecision").is_none());
        assert!(hook["additionalContext"].as_str().unwrap().contains("201"));
    }

    #[test]
    fn other_tools_and_quiet_calls_produce_nothing() {
        let edit = json!({"tool_name": "Edit", "tool_input": {"command": "nix flake check"}});
        assert!(claude_pre_tool_use(&cfg(), &edit).is_none());
        let quiet = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
        assert!(claude_pre_tool_use(&cfg(), &quiet).is_none());
    }
}

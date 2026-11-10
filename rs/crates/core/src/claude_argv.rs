//! Classify the tokens after `claude` the way claude's own CLI parser does: which are flags
//! (with their values), which are positional (the initial prompt), and whether anything is
//! unrecognised. Shared by the resume path (drop the positional prompt) and the shell-pane
//! restore path (replay the flags a user typed).

/// How many values a known flag takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arity {
    None,
    Required,
    /// Consume the next token only when it does not start with `-`.
    Optional,
    /// Consume following tokens until one starts with `-`.
    Variadic,
}

const FLAGS: &[(&str, Arity)] = &[
    ("--allow-dangerously-skip-permissions", Arity::None),
    ("--ax-screen-reader", Arity::None),
    ("--bg", Arity::None),
    ("--background", Arity::None),
    ("--bare", Arity::None),
    ("--brief", Arity::None),
    ("--chrome", Arity::None),
    ("-c", Arity::None),
    ("--continue", Arity::None),
    ("--dangerously-skip-permissions", Arity::None),
    ("--desktop", Arity::None),
    ("--disable-slash-commands", Arity::None),
    ("--exclude-dynamic-system-prompt-sections", Arity::None),
    ("--fork-session", Arity::None),
    ("--forward-subagent-text", Arity::None),
    ("-h", Arity::None),
    ("--help", Arity::None),
    ("--ide", Arity::None),
    ("--include-hook-events", Arity::None),
    ("--include-partial-messages", Arity::None),
    ("--no-chrome", Arity::None),
    ("--no-session-persistence", Arity::None),
    ("-p", Arity::None),
    ("--print", Arity::None),
    ("--replay-user-messages", Arity::None),
    ("--restricted", Arity::None),
    ("--safe-mode", Arity::None),
    ("--strict-mcp-config", Arity::None),
    ("--tmux", Arity::None),
    ("--verbose", Arity::None),
    ("-v", Arity::None),
    ("--version", Arity::None),
    ("--agent", Arity::Required),
    ("--agents", Arity::Required),
    ("--append-system-prompt", Arity::Required),
    ("--append-system-prompt-file", Arity::Required),
    ("--autocompact", Arity::Required),
    ("--debug-file", Arity::Required),
    ("--effort", Arity::Required),
    ("--environment", Arity::Required),
    ("--fallback-model", Arity::Required),
    ("--input-format", Arity::Required),
    ("--json-schema", Arity::Required),
    ("--max-budget-usd", Arity::Required),
    ("--model", Arity::Required),
    ("-n", Arity::Required),
    ("--name", Arity::Required),
    ("--output-format", Arity::Required),
    ("--permission-mode", Arity::Required),
    ("--permission-prompt-tool", Arity::Required),
    ("--permission-prompts", Arity::Required),
    ("--plugin-dir", Arity::Required),
    ("--plugin-url", Arity::Required),
    ("--remote-control-session-name-prefix", Arity::Required),
    ("--session-id", Arity::Required),
    ("--setting-sources", Arity::Required),
    ("--settings", Arity::Required),
    ("--system-prompt", Arity::Required),
    ("--system-prompt-file", Arity::Required),
    ("--system-prompt-snapshot", Arity::Required),
    ("--cloud", Arity::Optional),
    ("-d", Arity::Optional),
    ("--debug", Arity::Optional),
    ("--from-pr", Arity::Optional),
    ("--prompt-suggestions", Arity::Optional),
    ("--remote-control", Arity::Optional),
    ("-r", Arity::Optional),
    ("--resume", Arity::Optional),
    ("--teleport", Arity::Optional),
    ("-w", Arity::Optional),
    ("--worktree", Arity::Optional),
    ("--add-dir", Arity::Variadic),
    ("--allowedTools", Arity::Variadic),
    ("--allowed-tools", Arity::Variadic),
    ("--betas", Arity::Variadic),
    ("--disallowedTools", Arity::Variadic),
    ("--disallowed-tools", Arity::Variadic),
    ("--file", Arity::Variadic),
    ("--mcp-config", Arity::Variadic),
    ("--tools", Arity::Variadic),
];

fn arity(flag: &str) -> Option<Arity> {
    FLAGS.iter().find(|(f, _)| *f == flag).map(|(_, a)| *a)
}

/// What one token is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenKind {
    /// A known flag, or a value belonging to one.
    Flag,
    /// A positional token (the initial prompt).
    Positional,
    /// The `--` options terminator.
    Separator,
    /// An unrecognised `-…` token.
    Unknown,
}

/// Per-token classification of the tokens after the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub kinds: Vec<TokenKind>,
    /// `-p` / `--print` was seen (a one-shot run).
    pub print_mode: bool,
    pub has_unknown: bool,
}

/// Classify `tokens` (everything after the program).
pub fn classify<S: AsRef<str>>(tokens: &[S]) -> Classified {
    let n = tokens.len();
    let mut kinds = vec![TokenKind::Positional; n];
    let mut print_mode = false;
    let mut has_unknown = false;
    let mut i = 0;
    while i < n {
        let tok = tokens[i].as_ref();
        if tok == "--" {
            kinds[i] = TokenKind::Separator;
            // Everything after is positional (already the default).
            break;
        }
        if !tok.starts_with('-') || tok == "-" {
            i += 1;
            continue;
        }
        // `--flag=value`.
        if let Some((flag, _)) = tok.split_once('=').filter(|(f, _)| f.starts_with("--")) {
            match arity(flag) {
                Some(Arity::None) | None => {
                    kinds[i] = TokenKind::Unknown;
                    has_unknown = true;
                }
                Some(_) => kinds[i] = TokenKind::Flag,
            }
            i += 1;
            continue;
        }
        let Some(a) = arity(tok) else {
            kinds[i] = TokenKind::Unknown;
            has_unknown = true;
            i += 1;
            continue;
        };
        kinds[i] = TokenKind::Flag;
        if tok == "-p" || tok == "--print" {
            print_mode = true;
        }
        match a {
            Arity::None => {}
            Arity::Required => {
                if i + 1 < n {
                    kinds[i + 1] = TokenKind::Flag;
                    i += 1;
                }
            }
            Arity::Optional => {
                if i + 1 < n && !tokens[i + 1].as_ref().starts_with('-') {
                    kinds[i + 1] = TokenKind::Flag;
                    i += 1;
                }
            }
            Arity::Variadic => {
                while i + 1 < n && !tokens[i + 1].as_ref().starts_with('-') {
                    kinds[i + 1] = TokenKind::Flag;
                    i += 1;
                }
            }
        }
        i += 1;
    }
    Classified {
        kinds,
        print_mode,
        has_unknown,
    }
}

/// The classified argv: kept flag tokens, positional tokens, and the two summary bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeArgv {
    pub flags: Vec<String>,
    pub positional: Vec<String>,
    pub print_mode: bool,
    pub has_unknown: bool,
}

/// Split `tokens` (everything after the program) into flags and positionals.
pub fn parse<S: AsRef<str>>(tokens: &[S]) -> ClaudeArgv {
    let c = classify(tokens);
    let mut flags = Vec::new();
    let mut positional = Vec::new();
    for (t, k) in tokens.iter().zip(&c.kinds) {
        match k {
            TokenKind::Flag | TokenKind::Unknown => flags.push(t.as_ref().to_string()),
            TokenKind::Positional => positional.push(t.as_ref().to_string()),
            TokenKind::Separator => {}
        }
    }
    ClaudeArgv {
        flags,
        positional,
        print_mode: c.print_mode,
        has_unknown: c.has_unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(t: &[&str]) -> ClaudeArgv {
        parse(t)
    }
    fn v(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_value_flags() {
        let a = p(&["--dangerously-skip-permissions", "--verbose", "fix it"]);
        assert_eq!(a.flags, v(&["--dangerously-skip-permissions", "--verbose"]));
        assert_eq!(a.positional, v(&["fix it"]));
        assert!(!a.has_unknown && !a.print_mode);
    }

    #[test]
    fn required_value_takes_next_even_dashed() {
        let a = p(&["--model", "opus", "--system-prompt", "-weird", "hello"]);
        assert_eq!(
            a.flags,
            v(&["--model", "opus", "--system-prompt", "-weird"])
        );
        assert_eq!(a.positional, v(&["hello"]));
    }

    #[test]
    fn optional_value_only_when_not_a_flag() {
        let a = p(&["--resume", "abc", "-w", "--model", "m"]);
        assert_eq!(a.flags, v(&["--resume", "abc", "-w", "--model", "m"]));
        assert!(a.positional.is_empty());
        let a = p(&["--debug", "--verbose"]);
        assert_eq!(a.flags, v(&["--debug", "--verbose"]));
    }

    #[test]
    fn variadic_stops_at_next_flag() {
        let a = p(&["--add-dir", "/a", "/b", "--model", "m", "prompt"]);
        assert_eq!(a.flags, v(&["--add-dir", "/a", "/b", "--model", "m"]));
        assert_eq!(a.positional, v(&["prompt"]));
    }

    #[test]
    fn equals_form() {
        let a = p(&["--model=opus", "--mcp-config=/x.json", "go"]);
        assert_eq!(a.flags, v(&["--model=opus", "--mcp-config=/x.json"]));
        assert_eq!(a.positional, v(&["go"]));
        assert!(!a.has_unknown);
    }

    #[test]
    fn double_dash_ends_options() {
        let a = p(&["--model", "m", "--", "--not-a-flag", "x"]);
        assert_eq!(a.flags, v(&["--model", "m"]));
        assert_eq!(a.positional, v(&["--not-a-flag", "x"]));
        assert!(!a.has_unknown);
    }

    #[test]
    fn positional_before_and_after_flags() {
        let a = p(&["first", "--verbose", "second"]);
        assert_eq!(a.flags, v(&["--verbose"]));
        assert_eq!(a.positional, v(&["first", "second"]));
    }

    #[test]
    fn print_mode_detected() {
        assert!(p(&["-p", "hi"]).print_mode);
        assert!(p(&["--print", "hi"]).print_mode);
        assert!(!p(&["--model", "-p"]).print_mode);
    }

    #[test]
    fn unknown_flag_reported() {
        let a = p(&["--frobnicate", "--model", "m"]);
        assert!(a.has_unknown);
        assert!(p(&["--verbose=1"]).has_unknown);
        assert!(p(&["-xyz"]).has_unknown);
    }
}

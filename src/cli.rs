//! Make sense of the cargo command line we're forwarding.

/// Where a cargo command should run.
#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    /// Build in a slot, output through the path rewriter.
    Slot,
    /// Build in a slot, with the user's terminal handed straight through.
    Interactive,
    /// Run plain cargo in the worktree: the command edits source files, or
    /// doesn't build anything worth sharing.
    Worktree,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Parsed<'a> {
    /// `+nightly` style toolchain override, without the `+`.
    pub toolchain: Option<&'a str>,
    pub subcommand: Option<&'a str>,
    pub mode: Mode,
}

/// Global cargo flags that take a separate value.
const VALUE_FLAGS: &[&str] = &["--color", "--config", "-Z", "-C", "--explain"];

/// Subcommands that write to the source tree (or registry/network only).
/// Running these in a slot would apply their edits to the mirror, and the
/// next sync would silently throw them away.
const WORKTREE_COMMANDS: &[&str] = &[
    "add", "fix", "fmt", "generate-lockfile", "init", "insta", "install", "login", "logout", "new",
    "owner", "publish", "remove", "rm", "search", "set-version", "uninstall", "update", "upgrade",
    "vendor", "yank",
];

pub fn parse(args: &[String]) -> Parsed<'_> {
    let mut toolchain = None;
    let mut i = 0;
    if let Some(tc) = args.first().and_then(|a| a.strip_prefix('+')) {
        toolchain = Some(tc);
        i = 1;
    }
    let mut subcommand = None;
    let mut sub_at = args.len();
    while i < args.len() {
        let a = args[i].as_str();
        if VALUE_FLAGS.contains(&a) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        subcommand = Some(a);
        sub_at = i;
        break;
    }
    let rest = &args[(sub_at + 1).min(args.len())..];
    // Stop at `--`: anything after belongs to the program or test harness.
    let rest = rest.split(|a| a == "--").next().unwrap_or(&[]);
    let mode = match subcommand {
        None => Mode::Worktree,
        Some("run" | "r") => Mode::Interactive,
        Some("clippy") if rest.iter().any(|a| a == "--fix") => Mode::Worktree,
        Some(s) if WORKTREE_COMMANDS.contains(&s) => Mode::Worktree,
        Some(_) => Mode::Slot,
    };
    Parsed { toolchain, subcommand, mode }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> (Option<String>, Option<String>, Mode) {
        let args: Vec<String> = s.split_whitespace().map(String::from).collect();
        let r = parse(&args);
        (r.toolchain.map(String::from), r.subcommand.map(String::from), r.mode)
    }

    #[test]
    fn finds_subcommand_past_toolchain_and_global_flags() {
        assert_eq!(p("build"), (None, Some("build".into()), Mode::Slot));
        assert_eq!(p("+nightly run"), (Some("nightly".into()), Some("run".into()), Mode::Interactive));
        assert_eq!(p("+nightly -v --color always --config a=1 -Z x test").1.as_deref(), Some("test"));
        assert_eq!(p("--locked -C sub r").2, Mode::Interactive);
    }

    #[test]
    fn source_editing_commands_run_in_worktree() {
        for c in ["fmt", "fix", "add serde", "remove x", "update", "+nightly fmt", "clippy --fix", "-q update -p x"] {
            assert_eq!(p(c).2, Mode::Worktree, "{c}");
        }
        assert_eq!(p("clippy -- -D warnings").2, Mode::Slot);
        assert_eq!(p("clippy -- --fix").2, Mode::Slot, "after `--` is for clippy-driver, not cargo");
        assert_eq!(p("--version").2, Mode::Worktree);
        assert_eq!(p("").2, Mode::Worktree);
    }
}

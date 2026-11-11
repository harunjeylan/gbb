//! Which cargo commands are cheap (no link) and which are heavy (link / run tests).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Light,
    Heavy,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Light => "light",
            Class::Heavy => "heavy",
        }
    }

    pub fn parse(s: &str) -> Option<Class> {
        match s {
            "light" => Some(Class::Light),
            "heavy" => Some(Class::Heavy),
            _ => None,
        }
    }
}

impl std::fmt::Display for Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Cargo subcommands that never link the final artifact.
const LIGHT_COMMANDS: &[&str] = &[
    "check",
    "clippy",
    "fmt",
    "metadata",
    "tree",
    "pkgid",
    "locate-project",
    "verify-project",
    "read-manifest",
    "fetch",
    "search",
    "info",
];

/// First positional argument is the cargo subcommand.
pub fn subcommand(args: &[String]) -> Option<&str> {
    args.iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| s.as_str())
}

pub fn classify(args: &[String]) -> Class {
    match subcommand(args) {
        Some(cmd) if LIGHT_COMMANDS.contains(&cmd) => Class::Light,
        _ => Class::Heavy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn check_is_light() {
        assert_eq!(classify(&v(&["check", "-p", "goldenbee"])), Class::Light);
        assert_eq!(classify(&v(&["clippy", "--all-targets"])), Class::Light);
    }

    #[test]
    fn test_and_build_are_heavy() {
        assert_eq!(classify(&v(&["test", "-p", "goldenbee"])), Class::Heavy);
        assert_eq!(classify(&v(&["build"])), Class::Heavy);
        assert_eq!(classify(&v(&["run", "--bin", "goldenbee"])), Class::Heavy);
    }

    #[test]
    fn leading_flags_are_skipped() {
        assert_eq!(
            classify(&v(&["--locked", "check", "-p", "x"])),
            Class::Light
        );
    }

    #[test]
    fn unknown_command_defaults_to_heavy() {
        assert_eq!(classify(&v(&["nextest", "run"])), Class::Heavy);
    }
}

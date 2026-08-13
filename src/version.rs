use std::fmt;

pub struct Version;

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} (commit {}, built {})",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            short_commit(env!("AZC_COMMIT_SHA")),
            env!("AZC_BUILD_DATE")
        )
    }
}

fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

#[cfg(test)]
mod tests {
    use super::{Version, short_commit};

    #[test]
    fn version_line_contains_release_and_reproducible_metadata() {
        let line = Version.to_string();

        assert!(line.starts_with(concat!("azc ", env!("CARGO_PKG_VERSION"), " (commit ")));
        assert!(line.contains(", built "));
        assert!(line.ends_with(')'));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn commit_is_shortened_without_breaking_fallbacks() {
        assert_eq!(short_commit("0123456789abcdef"), "0123456789ab");
        assert_eq!(short_commit("unknown"), "unknown");
    }
}

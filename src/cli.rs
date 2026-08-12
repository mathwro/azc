use std::{error::Error, ffi::OsString, fmt};

#[derive(Debug, Eq, PartialEq)]
pub enum Command {
    Interactive { refresh: bool },
    Switch { query: String, refresh: bool },
    Help,
    Version,
}

pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let mut refresh = false;
    let mut positional_only = false;
    let mut query_parts = Vec::new();

    for argument in arguments {
        let argument = argument
            .into_string()
            .map_err(|_| CliError("arguments must be valid Unicode".to_owned()))?;

        if positional_only {
            query_parts.push(argument);
            continue;
        }

        match argument.as_str() {
            "--" => positional_only = true,
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "-r" | "--refresh" => refresh = true,
            value if value.starts_with('-') => {
                return Err(CliError(format!(
                    "unknown option '{value}'; run 'azc --help' for usage"
                )));
            }
            _ => query_parts.push(argument),
        }
    }

    if query_parts.is_empty() {
        Ok(Command::Interactive { refresh })
    } else {
        Ok(Command::Switch {
            query: query_parts.join(" "),
            refresh,
        })
    }
}

pub fn help() -> &'static str {
    "azc — fast Azure CLI subscription switching

USAGE:
    azc [OPTIONS] [QUERY...]

MODES:
    azc                 Open the searchable subscription list
    azc hr              Switch to the unique best match for 'hr'
    azc human resources Switch using a multi-word query

OPTIONS:
    -r, --refresh       Refresh subscriptions from Azure before listing
    -h, --help          Print help
    -V, --version       Print version
    --                  Treat remaining arguments as the query

INTERACTIVE KEYS:
    Type                Filter immediately
    Up/Down, Ctrl+P/N   Move selection
    Home/End            Jump to first/last match
    Enter               Switch to selected subscription
    Esc, Ctrl+C         Cancel
    Ctrl+U              Clear search

ENVIRONMENT:
    AZC_AZ_BIN           Azure CLI executable override (default: az)
"
}

#[derive(Debug, Eq, PartialEq)]
pub struct CliError(String);

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for CliError {}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{Command, parse};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_query_opens_interactive_mode() {
        assert_eq!(
            parse(args(&[])).unwrap(),
            Command::Interactive { refresh: false }
        );
    }

    #[test]
    fn joins_query_words_and_accepts_refresh_anywhere() {
        assert_eq!(
            parse(args(&["human", "--refresh", "resources"])).unwrap(),
            Command::Switch {
                query: "human resources".to_owned(),
                refresh: true,
            }
        );
    }

    #[test]
    fn separator_allows_a_query_starting_with_a_dash() {
        assert_eq!(
            parse(args(&["--", "-sandbox"])).unwrap(),
            Command::Switch {
                query: "-sandbox".to_owned(),
                refresh: false,
            }
        );
    }

    #[test]
    fn rejects_unknown_options() {
        assert!(
            parse(args(&["--wat"]))
                .unwrap_err()
                .to_string()
                .contains("unknown option")
        );
    }
}

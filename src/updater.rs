#[cfg(not(target_os = "windows"))]
use std::process::ExitStatus;
use std::{error::Error, ffi::OsStr, fmt, io, process::Command};

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateOutcome {
    #[cfg(not(target_os = "windows"))]
    Completed,
    #[cfg(target_os = "windows")]
    Started,
}

pub fn update() -> Result<UpdateOutcome, UpdateError> {
    let mut command = cargo_install_command(OsStr::new("cargo"));

    #[cfg(target_os = "windows")]
    {
        command.spawn().map_err(map_start_error)?;
        Ok(UpdateOutcome::Started)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let status = command.status().map_err(map_start_error)?;
        ensure_success(status)?;
        Ok(UpdateOutcome::Completed)
    }
}

fn cargo_install_command(cargo: &OsStr) -> Command {
    let mut command = Command::new(cargo);
    command.args(["install", "--git", REPOSITORY, "--locked", "--force"]);
    command
}

fn map_start_error(source: io::Error) -> UpdateError {
    if source.kind() == io::ErrorKind::NotFound {
        UpdateError::CargoNotFound
    } else {
        UpdateError::Start(source)
    }
}

#[cfg(not(target_os = "windows"))]
fn ensure_success(status: ExitStatus) -> Result<(), UpdateError> {
    if status.success() {
        Ok(())
    } else {
        Err(UpdateError::Failed(status.code()))
    }
}

#[derive(Debug)]
pub enum UpdateError {
    CargoNotFound,
    Start(io::Error),
    #[cfg(not(target_os = "windows"))]
    Failed(Option<i32>),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CargoNotFound => write!(
                f,
                "Cargo was not found; install Rust and Cargo, then retry 'azc update'"
            ),
            Self::Start(source) => write!(f, "could not start Cargo updater: {source}"),
            #[cfg(not(target_os = "windows"))]
            Self::Failed(Some(code)) => {
                write!(f, "Cargo updater failed with exit code {code}")
            }
            #[cfg(not(target_os = "windows"))]
            Self::Failed(None) => f.write_str("Cargo updater was terminated"),
        }
    }
}

impl Error for UpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Start(source) => Some(source),
            Self::CargoNotFound => None,
            #[cfg(not(target_os = "windows"))]
            Self::Failed(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{REPOSITORY, cargo_install_command};

    #[test]
    fn updater_reinstalls_locked_main_branch_from_project_repository() {
        let command = cargo_install_command(OsStr::new("cargo-test"));
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(command.get_program(), "cargo-test");
        assert_eq!(
            arguments,
            ["install", "--git", REPOSITORY, "--locked", "--force"]
        );
        assert_eq!(REPOSITORY, "https://github.com/mathwro/azc");
    }
}

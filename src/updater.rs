#[cfg(not(target_os = "windows"))]
use std::process::ExitStatus;
use std::{
    env,
    error::Error,
    ffi::OsStr,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const LATEST_RELEASE_URL: &str = "https://github.com/mathwro/azc/releases/latest";
const PREBUILT_RELEASE: bool = matches!(env!("AZC_PREBUILT_RELEASE").as_bytes(), b"1");
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateOutcome {
    #[cfg(not(target_os = "windows"))]
    Completed,
    #[cfg(target_os = "windows")]
    Started,
}

pub fn update() -> Result<UpdateOutcome, UpdateError> {
    let current_exe = env::current_exe()
        .ok()
        .map(|path| fs::canonicalize(&path).unwrap_or(path));
    check_install_source(current_exe.as_deref(), PREBUILT_RELEASE)?;

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

fn check_install_source(exe: Option<&Path>, prebuilt_release: bool) -> Result<(), UpdateError> {
    if let Some(exe) = exe {
        let normalized = exe
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        if normalized.contains("/cellar/azc/") || normalized.contains("/linuxbrew/") {
            return Err(UpdateError::ManagedByHomebrew(exe.to_path_buf()));
        }
        if normalized.contains("/scoop/apps/azc/") {
            return Err(UpdateError::ManagedByScoop(exe.to_path_buf()));
        }
    }
    if prebuilt_release {
        return Err(UpdateError::PrebuiltRelease);
    }
    Ok(())
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
    ManagedByHomebrew(PathBuf),
    ManagedByScoop(PathBuf),
    PrebuiltRelease,
    CargoNotFound,
    Start(io::Error),
    #[cfg(not(target_os = "windows"))]
    Failed(Option<i32>),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManagedByHomebrew(path) => write!(
                f,
                "azc is managed by Homebrew ({}); run 'brew upgrade mathwro/tools/azc' instead",
                path.display()
            ),
            Self::ManagedByScoop(path) => write!(
                f,
                "azc is managed by Scoop ({}); run 'scoop update azc' instead",
                path.display()
            ),
            Self::PrebuiltRelease => write!(
                f,
                "this binary was installed from a prebuilt release archive rather than Cargo; update via your package manager (brew/scoop) or {LATEST_RELEASE_URL}"
            ),
            Self::CargoNotFound => write!(
                f,
                "Cargo was not found; install Rust and Cargo, or update azc with your package manager (brew/scoop)"
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
            Self::ManagedByHomebrew(_)
            | Self::ManagedByScoop(_)
            | Self::PrebuiltRelease
            | Self::CargoNotFound => None,
            #[cfg(not(target_os = "windows"))]
            Self::Failed(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, path::Path};

    use super::{REPOSITORY, UpdateError, cargo_install_command, check_install_source};

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

    #[test]
    fn rejects_homebrew_managed_install_paths() {
        for exe in [
            "/opt/homebrew/Cellar/azc/0.6.0/bin/azc",
            "/usr/local/Cellar/azc/0.6.0/bin/azc",
            "/home/linuxbrew/.linuxbrew/Cellar/azc/0.6.0/bin/azc",
        ] {
            let err = check_install_source(Some(Path::new(exe)), false).unwrap_err();
            assert!(matches!(err, UpdateError::ManagedByHomebrew(_)));
            assert!(err.to_string().contains("brew upgrade mathwro/tools/azc"));
        }
    }

    #[test]
    fn rejects_scoop_managed_install_paths() {
        for exe in [
            r"C:\Users\alice\scoop\apps\azc\0.6.0\azc.exe",
            r"C:\ProgramData\scoop\apps\azc\current\azc.exe",
        ] {
            let err = check_install_source(Some(Path::new(exe)), false).unwrap_err();
            assert!(matches!(err, UpdateError::ManagedByScoop(_)));
            assert!(err.to_string().contains("scoop update azc"));
        }
    }

    #[test]
    fn rejects_prebuilt_release_binary_outside_package_manager_paths() {
        let err = check_install_source(Some(Path::new("/usr/local/bin/azc")), true).unwrap_err();
        assert!(matches!(err, UpdateError::PrebuiltRelease));
        let message = err.to_string();
        assert!(message.contains("prebuilt release"));
        assert!(message.contains("https://github.com/mathwro/azc/releases/latest"));
    }

    #[test]
    fn allows_cargo_installed_and_local_development_binaries() {
        assert!(check_install_source(Some(Path::new("/home/alice/.cargo/bin/azc")), false).is_ok());
        assert!(
            check_install_source(Some(Path::new(r"C:\Users\alice\.cargo\bin\azc.exe")), false)
                .is_ok()
        );
        assert!(check_install_source(None, false).is_ok());
    }
}

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, io,
    process::{Command, Output},
};

use crate::model::Subscription;

const AZURE_CLI_ENV: &str = "AZC_AZ_BIN";

#[derive(Clone, Debug)]
pub struct AzureCli {
    executable: OsString,
}

impl AzureCli {
    pub fn from_env() -> Self {
        Self {
            executable: env::var_os(AZURE_CLI_ENV).unwrap_or_else(default_executable),
        }
    }

    pub fn list_subscriptions(&self, refresh: bool) -> Result<Vec<Subscription>, AzureError> {
        let mut command = self.command();
        command.args(["account", "list", "--output", "json", "--only-show-errors"]);
        if refresh {
            command.arg("--refresh");
        }

        let output = self.run(command, "az account list")?;
        serde_json::from_slice(&output.stdout).map_err(AzureError::InvalidAccountJson)
    }

    pub fn set_subscription(&self, subscription_id: &str) -> Result<(), AzureError> {
        let mut command = self.command();
        command.args([
            "account",
            "set",
            "--subscription",
            subscription_id,
            "--only-show-errors",
        ]);
        self.run(command, "az account set")?;
        Ok(())
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.env("AZURE_CORE_NO_COLOR", "true");
        command
    }

    fn run(&self, mut command: Command, operation: &'static str) -> Result<Output, AzureError> {
        let output = command.output().map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                AzureError::NotFound {
                    executable: self.executable.clone(),
                }
            } else {
                AzureError::Start { operation, source }
            }
        })?;

        if output.status.success() {
            return Ok(output);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if stderr.is_empty() { stdout } else { stderr };

        Err(AzureError::CommandFailed {
            operation,
            status: output.status.code(),
            detail: if detail.is_empty() {
                "Azure CLI returned no error details".to_owned()
            } else {
                detail
            },
        })
    }
}

fn default_executable() -> OsString {
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = env::var_os("PATH") {
            for directory in env::split_paths(&path) {
                for file_name in ["az.exe", "az.cmd", "az.bat"] {
                    let candidate = directory.join(file_name);
                    if candidate.is_file() {
                        return candidate.into_os_string();
                    }
                }
            }
        }

        // The official Windows installer exposes Azure CLI as az.cmd.
        OsString::from("az.cmd")
    }

    #[cfg(not(target_os = "windows"))]
    {
        OsString::from("az")
    }
}

#[derive(Debug)]
pub enum AzureError {
    NotFound {
        executable: OsString,
    },
    Start {
        operation: &'static str,
        source: io::Error,
    },
    CommandFailed {
        operation: &'static str,
        status: Option<i32>,
        detail: String,
    },
    InvalidAccountJson(serde_json::Error),
}

impl fmt::Display for AzureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { executable } => write!(
                f,
                "Azure CLI executable '{}' was not found; install Azure CLI or set {AZURE_CLI_ENV}",
                executable.to_string_lossy()
            ),
            Self::Start { operation, source } => {
                write!(f, "could not start {operation}: {source}")
            }
            Self::CommandFailed {
                operation,
                status,
                detail,
            } => match status {
                Some(code) => write!(f, "{operation} failed with exit code {code}: {detail}"),
                None => write!(f, "{operation} was terminated: {detail}"),
            },
            Self::InvalidAccountJson(source) => {
                write!(f, "az account list returned invalid JSON: {source}")
            }
        }
    }
}

impl Error for AzureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Start { source, .. } => Some(source),
            Self::InvalidAccountJson(source) => Some(source),
            Self::NotFound { .. } | Self::CommandFailed { .. } => None,
        }
    }
}

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, io,
    process::{Command, Output},
};

use crate::model::{Subscription, TenantDescription};

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

    pub fn list_tenants(&self) -> Result<Vec<TenantDescription>, AzureError> {
        let mut command = self.command();
        command.args([
            "rest",
            "--method",
            "get",
            "--url",
            "https://management.azure.com/tenants?api-version=2022-12-01",
            "--query",
            "value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain}",
            "--output",
            "json",
        ]);

        let output = self.run(command, "az rest tenant list")?;
        let mut descriptions: Vec<TenantDescription> =
            serde_json::from_slice(&output.stdout).map_err(AzureError::InvalidTenantJson)?;

        let mut metadata_command = self.command();
        metadata_command.args([
            "account",
            "list",
            "--all",
            "--query",
            "[].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain}",
            "--output",
            "json",
            "--only-show-errors",
        ]);
        let metadata_output = self.run(metadata_command, "az account list tenant metadata")?;
        let metadata: Vec<TenantDescription> = serde_json::from_slice(&metadata_output.stdout)
            .map_err(AzureError::InvalidTenantMetadataJson)?;

        Self::merge_tenant_metadata(&mut descriptions, &metadata);

        Ok(descriptions)
    }

    pub(crate) fn merge_tenant_metadata(
        descriptions: &mut Vec<TenantDescription>,
        metadata: &[TenantDescription],
    ) {
        for metadata in metadata {
            if let Some(description) = descriptions.iter_mut().find(|description| {
                description
                    .tenant_id
                    .trim()
                    .eq_ignore_ascii_case(metadata.tenant_id.trim())
            }) {
                if (description.display_name.is_empty()
                    || description
                        .display_name
                        .eq_ignore_ascii_case(description.tenant_id.trim()))
                    && !metadata.display_name.is_empty()
                {
                    description.display_name = metadata.display_name.clone();
                }
                if description.default_domain.is_empty() && !metadata.default_domain.is_empty() {
                    description.default_domain = metadata.default_domain.clone();
                }
            } else if !metadata.tenant_id.trim().is_empty() {
                descriptions.push(metadata.clone());
            }
        }
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
    InvalidTenantJson(serde_json::Error),
    InvalidTenantMetadataJson(serde_json::Error),
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
            Self::InvalidTenantJson(source) => {
                write!(f, "az rest tenant list returned invalid JSON: {source}")
            }
            Self::InvalidTenantMetadataJson(source) => {
                write!(
                    f,
                    "az account list tenant metadata returned invalid JSON: {source}"
                )
            }
        }
    }
}

impl Error for AzureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Start { source, .. } => Some(source),
            Self::InvalidAccountJson(source) => Some(source),
            Self::InvalidTenantJson(source) => Some(source),
            Self::InvalidTenantMetadataJson(source) => Some(source),
            Self::NotFound { .. } | Self::CommandFailed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AzureCli;
    use crate::model::TenantDescription;

    fn tenant(tenant_id: &str, display_name: &str, default_domain: &str) -> TenantDescription {
        TenantDescription {
            tenant_id: tenant_id.to_owned(),
            display_name: display_name.to_owned(),
            default_domain: default_domain.to_owned(),
        }
    }

    #[test]
    fn merges_names_from_later_duplicate_account_records() {
        let mut descriptions = vec![tenant("tenant-a", "", "")];
        let metadata = vec![
            tenant("tenant-a", "", ""),
            tenant("TENANT-A", "Contoso", "contoso.onmicrosoft.com"),
        ];

        AzureCli::merge_tenant_metadata(&mut descriptions, &metadata);

        assert_eq!(descriptions[0].display_name, "Contoso");
        assert_eq!(descriptions[0].default_domain, "contoso.onmicrosoft.com");
    }

    #[test]
    fn preserves_existing_names_and_does_not_cross_tenants() {
        let mut descriptions = vec![
            tenant("tenant-a", "Existing", "existing.example"),
            tenant("tenant-b", "", ""),
        ];
        let metadata = vec![
            tenant("tenant-a", "Wrong", "wrong.example"),
            tenant("tenant-b", "Fabrikam", "fabrikam.onmicrosoft.com"),
        ];

        AzureCli::merge_tenant_metadata(&mut descriptions, &metadata);

        assert_eq!(descriptions[0].display_name, "Existing");
        assert_eq!(descriptions[0].default_domain, "existing.example");
        assert_eq!(descriptions[1].display_name, "Fabrikam");
    }
    #[test]
    fn appends_tenants_missing_from_resource_manager_response() {
        let mut descriptions = vec![tenant("tenant-a", "Contoso", "contoso.example")];
        let metadata = vec![tenant("tenant-b", "Fabrikam", "fabrikam.example")];

        AzureCli::merge_tenant_metadata(&mut descriptions, &metadata);

        assert_eq!(descriptions.len(), 2);
        assert_eq!(descriptions[1], metadata[0]);
    }
}

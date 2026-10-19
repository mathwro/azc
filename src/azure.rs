use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, io,
    process::{Command, Output},
};

use serde::de::DeserializeOwned;

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
        parse_json(&output.stdout).map_err(AzureError::InvalidAccountJson)
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
            parse_json(&output.stdout).map_err(AzureError::InvalidTenantJson)?;

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
        let metadata: Vec<TenantDescription> =
            parse_json(&metadata_output.stdout).map_err(AzureError::InvalidTenantMetadataJson)?;

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
                if description.meaningful_name().is_none()
                    && let Some(name) = metadata.meaningful_name()
                {
                    description.display_name = name.to_owned();
                }
                if description.default_domain.trim().is_empty()
                    && !metadata.default_domain.trim().is_empty()
                {
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

    pub fn login(&self, tenant_id: &str) -> Result<(), AzureError> {
        let mut command = self.command();
        command.arg("login");
        if !tenant_id.trim().is_empty() {
            command.args(["--tenant", tenant_id]);
        }
        command.args(["--output", "none"]);
        // azc already has a switch target; do not ask Azure CLI to select another.
        command.env("AZURE_CORE_LOGIN_EXPERIENCE_V2", "off");
        let status = command
            .status()
            .map_err(|source| self.start_error("az login", source))?;
        if !status.success() {
            return Err(AzureError::CommandFailed {
                operation: "az login",
                status: status.code(),
                detail: "sign-in did not complete; no subscription switch was retried".to_owned(),
            });
        }
        Ok(())
    }

    fn start_error(&self, operation: &'static str, source: io::Error) -> AzureError {
        if source.kind() == io::ErrorKind::NotFound {
            AzureError::NotFound {
                executable: self.executable.clone(),
            }
        } else {
            AzureError::Start { operation, source }
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.env("AZURE_CORE_NO_COLOR", "true");
        command.env("PYTHONIOENCODING", "utf-8");
        command
    }

    fn run(&self, mut command: Command, operation: &'static str) -> Result<Output, AzureError> {
        let output = command
            .output()
            .map_err(|source| self.start_error(operation, source))?;

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

fn parse_json<T: DeserializeOwned>(output: &[u8]) -> Result<T, serde_json::Error> {
    let result = serde_json::from_slice(output);
    if result.is_err()
        && std::str::from_utf8(output).is_err()
        && let Some(code_page) = windows_ansi_code_page()
        && let Some(decoded) = decode_windows_output(output, code_page)
    {
        return serde_json::from_str(&decoded);
    }
    result
}

fn decode_windows_output(output: &[u8], code_page: u16) -> Option<std::borrow::Cow<'_, str>> {
    codepage::to_encoding_no_replacement(code_page)?
        .decode_without_bom_handling_and_without_replacement(output)
}

#[cfg(target_os = "windows")]
fn windows_ansi_code_page() -> Option<u16> {
    // SAFETY: GetACP has no arguments or memory preconditions.
    u16::try_from(unsafe { windows_sys::Win32::Globalization::GetACP() }).ok()
}

#[cfg(not(target_os = "windows"))]
fn windows_ansi_code_page() -> Option<u16> {
    if !cfg!(target_os = "linux")
        || (env::var_os("WSL_INTEROP").is_none() && env::var_os("WSL_DISTRO_NAME").is_none())
    {
        return None;
    }
    // A Linux azc can invoke Windows Azure CLI through a WSL/Scoop shim.
    // Query the source system's ANSI encoding, not WSL's UTF-8 locale or the
    // Windows console's (potentially different) OEM code page. Cache the probe
    // so subscription and tenant loads do not each start PowerShell.
    static CODE_PAGE: std::sync::OnceLock<Option<u16>> = std::sync::OnceLock::new();
    *CODE_PAGE.get_or_init(|| {
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Text.Encoding]::Default.CodePage",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        std::str::from_utf8(&output.stdout)
            .ok()?
            .trim()
            .parse()
            .ok()
    })
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

impl AzureError {
    pub fn requires_reauthentication(&self) -> bool {
        let Self::CommandFailed { detail, .. } = self else {
            return false;
        };
        let detail = detail.to_ascii_lowercase();
        (detail.contains("the subscription of '") && detail.contains("doesn't exist in cloud '"))
            || (detail.contains("az login") && detail.contains("please run"))
            || detail.contains("interactive authentication is needed")
            || [
                "aadsts700082",
                "aadsts70043",
                "aadsts50173",
                "aadsts50076",
                "aadsts50079",
            ]
            .iter()
            .any(|code| detail.contains(code))
    }
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
    use super::{AzureCli, AzureError, parse_json};
    use crate::model::{Subscription, TenantDescription};

    #[test]
    fn decodes_unicode_names_without_changing_switch_ids() {
        let subscriptions: Vec<Subscription> = parse_json(
            "[{\"id\":\"selected-id\",\"name\":\"Zażółć – Production 🦀\",\"tenantId\":\"tenant-a\"}]".as_bytes(),
        ).unwrap();
        assert_eq!(subscriptions[0].name, "Zażółć – Production 🦀");
        assert_eq!(subscriptions[0].id, "selected-id");
        assert_eq!(subscriptions[0].tenant_id, "tenant-a");
    }

    #[test]
    fn rejects_invalid_json_and_missing_required_subscription_fields() {
        for json in [
            br#"[{"name":"Name"}]"#.as_slice(),
            br#"[{"id":"id"}]"#,
            br#"[{"id":"id","name":"\q"}]"#,
            br#"[{"id":"id","name":"Name"}"#,
            br#"[{"id":"id","name":"Name"}] trailing"#,
        ] {
            assert!(parse_json::<Vec<Subscription>>(json).is_err());
        }
    }

    #[test]
    fn decodes_legacy_windows_names_without_replacement_characters() {
        for (code_page, bytes, name) in [
            (
                1250,
                b"[{\"id\":\"selected-id\",\"name\":\"Za\xbf\xf3\xb3\xe6\"}]".as_slice(),
                "Zażółć",
            ),
            (
                1252,
                b"[{\"id\":\"selected-id\",\"name\":\"\xc9quipe \x96 Production\"}]",
                "Équipe – Production",
            ),
            (
                936,
                b"[{\"id\":\"selected-id\",\"name\":\"\xb2\xe2\xca\xd4\"}]",
                "测试",
            ),
        ] {
            let decoded = super::decode_windows_output(bytes, code_page).unwrap();
            let subscriptions: Vec<Subscription> = parse_json(decoded.as_bytes()).unwrap();
            assert_eq!(subscriptions[0].name, name);
            assert_eq!(subscriptions[0].id, "selected-id");
        }
        assert!(super::decode_windows_output(b"\xff", 65001).is_none());
        assert!(super::decode_windows_output(b"\x81", 932).is_none());
    }

    #[test]
    fn identifies_reauthentication_without_hiding_unrelated_failures() {
        for (detail, expected) in [
            (
                "ERROR: The subscription of 'id' doesn't exist in cloud 'AzureCloud'.",
                true,
            ),
            ("ERROR: Please run 'az login' to setup account.", true),
            ("ERROR: Please run \"az login\" to reauthenticate.", true),
            ("Interactive authentication is needed.", true),
            (
                "AADSTS700082: The refresh token has expired due to inactivity.",
                true,
            ),
            (
                "AADSTS70043: The refresh token has expired or is invalid.",
                true,
            ),
            (
                "AADSTS50173: The provided grant has expired due to it being revoked.",
                true,
            ),
            (
                "AADSTS50076: You must use multi-factor authentication.",
                true,
            ),
            (
                "AADSTS50079: You must enroll in multi-factor authentication.",
                true,
            ),
            ("ERROR: network unreachable", false),
            (
                "ERROR: AuthorizationFailed: insufficient permissions",
                false,
            ),
            (
                "ERROR: The subscription of 'id' has more than one match in cloud 'AzureCloud'.",
                false,
            ),
        ] {
            let error = AzureError::CommandFailed {
                operation: "az account set",
                status: Some(1),
                detail: detail.to_owned(),
            };
            assert_eq!(error.requires_reauthentication(), expected, "{detail}");
        }
        assert!(
            !AzureError::NotFound {
                executable: "az".into()
            }
            .requires_reauthentication()
        );
    }

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

    #[cfg(unix)]
    mod reauthentication {
        use super::super::AzureCli;
        use crate::{model::Subscription, switch_subscription};
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            path::PathBuf,
            sync::{
                Mutex, MutexGuard,
                atomic::{AtomicU64, Ordering},
            },
        };

        struct FakeAzure {
            directory: PathBuf,
            _guard: MutexGuard<'static, ()>,
        }

        impl FakeAzure {
            fn new(mode: &str) -> Self {
                // Do not fork another fake while its script is open for writing.
                static SCRIPT_LOCK: Mutex<()> = Mutex::new(());
                let guard = SCRIPT_LOCK
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let path = std::env::temp_dir().join(format!(
                    "azc-reauth-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir(&path).unwrap();
                fs::write(path.join("mode"), mode).unwrap();
                fs::write(
                    path.join("az"),
                    r#"#!/bin/sh
directory=${0%/*}
mode=$(cat "$directory/mode")
case "$*" in
  'account set --subscription selected-id --only-show-errors')
    printf 'set\n' >> "$directory/attempts"
    if [ "$mode" = unrelated ]; then
      echo 'ERROR: network unreachable' >&2
      exit 1
    fi
    if [ -f "$directory/signed-in" ] && [ "$mode" != still-missing ]; then
      printf selected-id > "$directory/active"
    else
      echo "ERROR: The subscription of 'selected-id' doesn't exist in cloud 'AzureCloud'." >&2
      exit 1
    fi
    ;;
  'login --tenant tenant-a --output none' | 'login --output none')
    [ "$AZURE_CORE_LOGIN_EXPERIENCE_V2" = off ] || exit 2
    printf 'login\n' >> "$directory/attempts"
    [ "$mode" != login-fails ] || exit 1
    touch "$directory/signed-in"
    ;;
  *) exit 2 ;;
esac
"#,
                )
                .unwrap();
                fs::set_permissions(path.join("az"), fs::Permissions::from_mode(0o700)).unwrap();
                Self {
                    directory: path,
                    _guard: guard,
                }
            }

            fn azure(&self) -> AzureCli {
                AzureCli {
                    executable: self.directory.join("az").into_os_string(),
                }
            }

            fn attempts(&self) -> String {
                fs::read_to_string(self.directory.join("attempts")).unwrap()
            }
        }

        impl Drop for FakeAzure {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.directory);
            }
        }

        fn subscription() -> Subscription {
            serde_json::from_str(r#"{"id":"selected-id","name":"Selected","tenantId":"tenant-a"}"#)
                .unwrap()
        }

        #[test]
        fn signs_in_then_switches_only_to_the_original_id() {
            let fake = FakeAzure::new("");
            assert!(switch_subscription(&fake.azure(), &subscription(), |_, _| Ok(true)).unwrap());
            assert_eq!(
                fs::read_to_string(fake.directory.join("active")).unwrap(),
                "selected-id"
            );
            assert_eq!(fake.attempts(), "set\nlogin\nset\n");
        }

        #[test]
        fn declining_sign_in_leaves_context_unchanged() {
            let fake = FakeAzure::new("");
            assert!(
                !switch_subscription(&fake.azure(), &subscription(), |_, _| Ok(false)).unwrap()
            );
            assert!(!fake.directory.join("active").exists());
            assert_eq!(fake.attempts(), "set\n");
        }

        #[test]
        fn failed_login_does_not_retry_switching() {
            let fake = FakeAzure::new("login-fails");
            assert!(switch_subscription(&fake.azure(), &subscription(), |_, _| Ok(true)).is_err());
            assert!(!fake.directory.join("active").exists());
            assert_eq!(fake.attempts(), "set\nlogin\n");
        }

        #[test]
        fn missing_subscription_after_login_does_not_loop() {
            let fake = FakeAzure::new("still-missing");
            assert!(switch_subscription(&fake.azure(), &subscription(), |_, _| Ok(true)).is_err());
            assert!(!fake.directory.join("active").exists());
            assert_eq!(fake.attempts(), "set\nlogin\nset\n");
        }

        #[test]
        fn unrelated_errors_never_offer_login() {
            let fake = FakeAzure::new("unrelated");
            let error = switch_subscription(&fake.azure(), &subscription(), |_, _| {
                panic!("unrelated failure offered login")
            })
            .unwrap_err();
            assert!(error.to_string().contains("network unreachable"));
            assert_eq!(fake.attempts(), "set\n");
        }

        #[test]
        fn successful_switch_does_not_offer_login() {
            let fake = FakeAzure::new("");
            fs::write(fake.directory.join("signed-in"), "").unwrap();
            assert!(
                switch_subscription(&fake.azure(), &subscription(), |_, _| {
                    panic!("successful switch offered login")
                })
                .unwrap()
            );
            assert_eq!(
                fs::read_to_string(fake.directory.join("active")).unwrap(),
                "selected-id"
            );
            assert_eq!(fake.attempts(), "set\n");
        }

        #[test]
        fn omitted_tenant_uses_default_login() {
            let fake = FakeAzure::new("");
            let mut selected = subscription();
            selected.tenant_id.clear();
            assert!(switch_subscription(&fake.azure(), &selected, |_, _| Ok(true)).unwrap());
            assert_eq!(
                fs::read_to_string(fake.directory.join("active")).unwrap(),
                "selected-id"
            );
        }
    }
}

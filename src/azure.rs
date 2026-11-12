use std::{
    collections::HashMap,
    env,
    error::Error,
    ffi::OsString,
    fmt, io,
    process::{Command, Output, Stdio},
    thread,
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
            executable: env::var_os(AZURE_CLI_ENV)
                .filter(|path| !path.is_empty())
                .unwrap_or_else(default_executable),
        }
    }

    pub(crate) fn executable(&self) -> &std::ffi::OsStr {
        &self.executable
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

        // Run both commands concurrently on scoped threads so their startups
        // overlap and neither child can block filling its stdout/stderr pipe
        // while the other is still running.
        let (rest, metadata) = thread::scope(|scope| {
            let metadata = scope.spawn(|| {
                self.run(metadata_command, "az account list tenant metadata")
                    .and_then(|output| {
                        parse_json::<Vec<TenantDescription>>(&output.stdout)
                            .map_err(AzureError::InvalidTenantMetadataJson)
                    })
            });
            let rest = self.run(command, "az rest tenant list").and_then(|output| {
                parse_json::<Vec<TenantDescription>>(&output.stdout)
                    .map_err(AzureError::InvalidTenantJson)
            });
            let metadata = metadata
                .join()
                .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
            (rest, metadata)
        });
        // Either source alone still yields labels: the local metadata covers
        // offline use and tenants whose Resource Manager access needs sign-in.
        match (rest, metadata) {
            (Ok(mut descriptions), Ok(metadata)) => {
                Self::merge_tenant_metadata(&mut descriptions, &metadata);
                Ok(descriptions)
            }
            (Ok(descriptions), Err(_)) => Ok(descriptions),
            (Err(_), Ok(metadata)) => {
                let mut descriptions = Vec::new();
                Self::merge_tenant_metadata(&mut descriptions, &metadata);
                Ok(descriptions)
            }
            (Err(error), Err(_)) => Err(error),
        }
    }

    pub(crate) fn merge_tenant_metadata(
        descriptions: &mut Vec<TenantDescription>,
        metadata: &[TenantDescription],
    ) {
        if metadata.is_empty() {
            return;
        }

        let mut index_by_tenant: HashMap<String, usize> =
            HashMap::with_capacity(descriptions.len());
        for (index, description) in descriptions.iter().enumerate() {
            index_by_tenant
                .entry(description.tenant_id.trim().to_ascii_lowercase())
                .or_insert(index);
        }

        let mut key = String::new();
        for metadata in metadata {
            let trimmed = metadata.tenant_id.trim();
            key.clear();
            key.push_str(trimmed);
            key.make_ascii_lowercase();

            if let Some(&index) = index_by_tenant.get(key.as_str()) {
                let description = &mut descriptions[index];
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
            } else if !trimmed.is_empty() {
                let index = descriptions.len();
                descriptions.push(metadata.clone());
                index_by_tenant.insert(key.clone(), index);
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
            .stdin(Stdio::null())
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

#[cfg(any(target_os = "windows", test))]
fn find_windows_executable_in_path(path: &std::ffi::OsStr) -> Option<OsString> {
    for directory in env::split_paths(path) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        for file_name in ["az.exe", "az.cmd", "az.bat"] {
            let candidate = directory.join(file_name);
            if candidate.is_file() {
                return Some(candidate.into_os_string());
            }
        }
    }
    None
}

fn default_executable() -> OsString {
    #[cfg(target_os = "windows")]
    {
        if let Some(executable) =
            env::var_os("PATH").and_then(|path| find_windows_executable_in_path(&path))
        {
            return executable;
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

    fn reference_merge_tenant_metadata(
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

    #[test]
    fn merge_preserves_first_match_order_whitespace_case_and_progressive_appends() {
        let guid_a = "11111111-2222-3333-4444-555555555555";
        let initial_descriptions = vec![
            // Duplicate tenant IDs in initial descriptions: first match must win.
            tenant("  tenant-dup  ", "", ""),
            tenant("TENANT-DUP", "", ""),
            // Existing tenant with GUID display_name (non-meaningful) and empty domain.
            tenant(guid_a, guid_a, "   "),
            // Empty/whitespace tenant ID already in descriptions.
            tenant("   ", "", ""),
        ];
        let metadata = vec![
            // Whitespace/empty metadata records match existing empty entry or get ignored.
            tenant("", "Empty Name", "empty.example"),
            tenant(" \t ", "Ignored Second Empty", "ignored.example"),
            // Matches FIRST duplicate entry in descriptions (index 0), leaving index 1 untouched.
            tenant("Tenant-Dup", "First Dup Name", ""),
            tenant("  TENANT-DUP\n", "Later Dup Name", "dup.example"),
            // Replaces GUID name with meaningful name and fills blank domain.
            tenant(
                "  11111111-2222-3333-4444-555555555555 ",
                "Guid Resolved",
                "guid.example",
            ),
            // Appends new tenant with empty fields, then progressively fills name and domain
            // across subsequent subscription metadata records with varied case/whitespace.
            tenant("  tenant-new ", "", ""),
            tenant("TENANT-NEW", "tenant-new", "new.example"),
            tenant(
                "tenant-new",
                "  Northwind Traders  ",
                "ignored-later.example",
            ),
            tenant("TENANT-NEW", "Ignored Later Name", "ignored-2.example"),
        ];

        let mut actual = initial_descriptions.clone();
        let mut expected = initial_descriptions;
        AzureCli::merge_tenant_metadata(&mut actual, &metadata);
        reference_merge_tenant_metadata(&mut expected, &metadata);

        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 5);
        assert_eq!(actual[0].display_name, "First Dup Name");
        assert_eq!(actual[0].default_domain, "dup.example");
        assert_eq!(actual[1].display_name, "");
        assert_eq!(actual[1].default_domain, "");
        assert_eq!(actual[2].display_name, "Guid Resolved");
        assert_eq!(actual[2].default_domain, "guid.example");
        assert_eq!(actual[3].display_name, "Empty Name");
        assert_eq!(actual[3].default_domain, "empty.example");
        assert_eq!(actual[4].tenant_id, "  tenant-new ");
        assert_eq!(actual[4].display_name, "Northwind Traders");
        assert_eq!(actual[4].default_domain, "new.example");
    }

    #[test]
    fn merge_matches_reference_across_generated_edge_cases() {
        let tenant_pool = [
            "",
            "   ",
            "tenant-0",
            "TENANT-0",
            "  tenant-0  ",
            "tenant-1",
            "  TENANT-1\t",
            "00000000-1111-2222-3333-444444444444",
            "  00000000-1111-2222-3333-444444444444  ",
            "tenant-only-meta-a",
            "TENANT-ONLY-META-A",
            "tenant-only-meta-b",
        ];
        let name_pool = [
            "",
            "   ",
            "tenant-0",
            "00000000-1111-2222-3333-444444444444",
            "Contoso",
            "  Fabrikam  ",
            "Adventure Works",
        ];
        let domain_pool = ["", "   ", "contoso.example", "fabrikam.onmicrosoft.com"];

        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next_usize = |bound: usize| -> usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state as usize) % bound
        };

        for _case in 0..200 {
            let desc_len = next_usize(12);
            let meta_len = next_usize(40);
            let initial: Vec<TenantDescription> = (0..desc_len)
                .map(|_| {
                    tenant(
                        tenant_pool[next_usize(tenant_pool.len())],
                        name_pool[next_usize(name_pool.len())],
                        domain_pool[next_usize(domain_pool.len())],
                    )
                })
                .collect();
            let metadata: Vec<TenantDescription> = (0..meta_len)
                .map(|_| {
                    tenant(
                        tenant_pool[next_usize(tenant_pool.len())],
                        name_pool[next_usize(name_pool.len())],
                        domain_pool[next_usize(domain_pool.len())],
                    )
                })
                .collect();

            let mut actual = initial.clone();
            let mut expected = initial;
            AzureCli::merge_tenant_metadata(&mut actual, &metadata);
            reference_merge_tenant_metadata(&mut expected, &metadata);
            assert_eq!(actual, expected);
        }
    }

    fn enterprise_merge_fixture(
        tenant_count: usize,
        subscription_count: usize,
    ) -> (Vec<TenantDescription>, Vec<TenantDescription>) {
        let descriptions: Vec<TenantDescription> = (0..tenant_count)
            .map(|i| {
                let id = format!("{i:08x}-1111-2222-3333-{i:012x}");
                if i % 3 == 0 {
                    tenant(
                        &id,
                        &format!("Enterprise Tenant {i}"),
                        &format!("t{i}.example"),
                    )
                } else {
                    tenant(&id, "", "")
                }
            })
            .collect();

        let total_tenants = tenant_count + 50;
        let metadata: Vec<TenantDescription> = (0..subscription_count)
            .map(|s| {
                let t = (s * 37) % total_tenants;
                let id = if s % 2 == 0 {
                    format!("{t:08x}-1111-2222-3333-{t:012x}")
                } else {
                    format!("  {t:08X}-1111-2222-3333-{t:012X} ")
                };
                let name = if s % 4 == 0 { "" } else { "Contoso Division" };
                let domain = if s % 5 == 0 {
                    ""
                } else {
                    "contoso.onmicrosoft.com"
                };
                tenant(&id, name, domain)
            })
            .collect();

        (descriptions, metadata)
    }

    #[test]
    fn merge_matches_reference_at_enterprise_scale() {
        let (mut actual, metadata) = enterprise_merge_fixture(500, 10_000);
        let mut expected = actual.clone();

        AzureCli::merge_tenant_metadata(&mut actual, &metadata);
        reference_merge_tenant_metadata(&mut expected, &metadata);

        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 550);
    }

    #[test]
    #[ignore]
    fn bench_merge_tenant_metadata_enterprise_scale() {
        let (base_descriptions, metadata) = enterprise_merge_fixture(500, 10_000);
        let warmup = 5;
        let iterations = 41;

        for _ in 0..warmup {
            let mut d = std::hint::black_box(base_descriptions.clone());
            reference_merge_tenant_metadata(&mut d, std::hint::black_box(metadata.as_slice()));
            std::hint::black_box(d);

            let mut d2 = std::hint::black_box(base_descriptions.clone());
            AzureCli::merge_tenant_metadata(&mut d2, std::hint::black_box(metadata.as_slice()));
            std::hint::black_box(d2);
        }

        let mut baseline_samples = Vec::with_capacity(iterations);
        let mut optimized_samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let mut d_ref = base_descriptions.clone();
            let start_ref = std::time::Instant::now();
            reference_merge_tenant_metadata(
                std::hint::black_box(&mut d_ref),
                std::hint::black_box(metadata.as_slice()),
            );
            baseline_samples.push(start_ref.elapsed());

            let mut d_opt = base_descriptions.clone();
            let start_opt = std::time::Instant::now();
            AzureCli::merge_tenant_metadata(
                std::hint::black_box(&mut d_opt),
                std::hint::black_box(metadata.as_slice()),
            );
            optimized_samples.push(start_opt.elapsed());

            assert_eq!(d_opt, d_ref);
        }

        baseline_samples.sort_unstable();
        optimized_samples.sort_unstable();
        let baseline_median = baseline_samples[iterations / 2];
        let optimized_median = optimized_samples[iterations / 2];
        let baseline_mean_us = baseline_samples
            .iter()
            .map(|d| d.as_secs_f64() * 1_000_000.0)
            .sum::<f64>()
            / iterations as f64;
        let optimized_mean_us = optimized_samples
            .iter()
            .map(|d| d.as_secs_f64() * 1_000_000.0)
            .sum::<f64>()
            / iterations as f64;
        let baseline_median_us = baseline_median.as_secs_f64() * 1_000_000.0;
        let optimized_median_us = optimized_median.as_secs_f64() * 1_000_000.0;
        let speedup = baseline_median_us / optimized_median_us.max(0.001);

        println!(
            "bench_merge_tenant_metadata_enterprise_scale (500 tenants + 10,000 subscription records, {iterations} iterations):"
        );
        println!(
            "  baseline O(T*M) linear scan : median = {baseline_median_us:.1} us ({:.3} ms), mean = {baseline_mean_us:.1} us",
            baseline_median_us / 1_000.0
        );
        println!(
            "  current  merge_tenant_metadata: median = {optimized_median_us:.1} us ({:.3} ms), mean = {optimized_mean_us:.1} us",
            optimized_median_us / 1_000.0
        );
        println!("  speedup (median)            : {speedup:.2}x");
    }

    #[test]
    fn windows_path_discovery_skips_empty_entries_and_preserves_extension_precedence() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "azc-win-path-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root);

        let path_with_empties = std::env::join_paths([
            std::path::PathBuf::new(),
            first.clone(),
            std::path::PathBuf::new(),
            second.clone(),
            std::path::PathBuf::new(),
        ])
        .unwrap();
        assert_eq!(
            super::find_windows_executable_in_path(&path_with_empties),
            None
        );

        // Precedence within a directory: az.exe > az.cmd > az.bat, and earlier directory wins.
        std::fs::write(second.join("az.exe"), "").unwrap();
        std::fs::write(first.join("az.bat"), "").unwrap();
        assert_eq!(
            super::find_windows_executable_in_path(&path_with_empties),
            Some(first.join("az.bat").into_os_string())
        );

        std::fs::write(first.join("az.cmd"), "").unwrap();
        assert_eq!(
            super::find_windows_executable_in_path(&path_with_empties),
            Some(first.join("az.cmd").into_os_string())
        );

        std::fs::write(first.join("az.exe"), "").unwrap();
        assert_eq!(
            super::find_windows_executable_in_path(&path_with_empties),
            Some(first.join("az.exe").into_os_string())
        );
    }

    // Do not fork another fake while its script is open for writing.
    #[cfg(unix)]
    static SCRIPT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    mod reauthentication {
        use super::super::AzureCli;
        use crate::{model::Subscription, switch_subscription};
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            path::PathBuf,
            sync::{
                MutexGuard,
                atomic::{AtomicU64, Ordering},
            },
        };

        struct FakeAzure {
            directory: PathBuf,
            _guard: MutexGuard<'static, ()>,
        }

        impl FakeAzure {
            fn new(mode: &str) -> Self {
                let guard = super::SCRIPT_LOCK
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

    #[cfg(unix)]
    mod tenant_loading {
        use super::super::{AzureCli, AzureError};
        use crate::model::{Subscription, TenantDescription, tenants_from_subscriptions};
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            path::PathBuf,
            sync::{
                MutexGuard,
                atomic::{AtomicU64, Ordering},
            },
        };

        const RESOURCE_MANAGER: &str = r#"[
            {"tenantId":"tenant-a","displayName":"Contoso","defaultDomain":"contoso.example"},
            {"tenantId":"tenant-b","displayName":null,"defaultDomain":null}
        ]"#;
        // Azure CLI repeats tenant metadata for every subscription record.
        const LOCAL: &str = r#"[
            {"tenantId":"TENANT-B","displayName":null,"defaultDomain":null},
            {"tenantId":"tenant-b","displayName":"Fabrikam","defaultDomain":"fabrikam.example"},
            {"tenantId":"tenant-a","displayName":"Contoso Local","defaultDomain":"local.example"},
            {"tenantId":"tenant-c","displayName":"","defaultDomain":"northwind.example"}
        ]"#;

        struct FakeAzure {
            directory: PathBuf,
            _guard: MutexGuard<'static, ()>,
        }

        impl FakeAzure {
            /// Each source fails unless its JSON is given. The Resource
            /// Manager call only answers once the local call has finished
            /// writing its payload, so sequential starts or sequential pipe
            /// collection will deadlock/time out.
            fn new(rest: Option<&str>, local: Option<&str>) -> Self {
                let guard = super::SCRIPT_LOCK
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let path = std::env::temp_dir().join(format!(
                    "azc-tenant-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir(&path).unwrap();
                if let Some(json) = rest {
                    fs::write(path.join("rest.json"), json).unwrap();
                }
                if let Some(json) = local {
                    fs::write(path.join("local.json"), json).unwrap();
                }
                fs::write(
                    path.join("az"),
                    r#"#!/bin/sh
directory=${0%/*}
[ "$AZURE_CORE_NO_COLOR" = true ] && [ "$PYTHONIOENCODING" = utf-8 ] || exit 4
case "$*" in
  'rest --method get --url https://management.azure.com/tenants?api-version=2022-12-01 --query value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain} --output json')
    attempts=0
    while [ ! -f "$directory/local-finished" ]; do
      attempts=$((attempts + 1))
      if [ "$attempts" -gt 40 ]; then
        echo 'ERROR: tenant commands deadlocked or ran sequentially' >&2
        exit 3
      fi
      sleep 0.05
    done
    source=rest
    ;;
  'account list --all --query [].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain} --output json --only-show-errors')
    source=local
    ;;
  *) exit 2 ;;
esac
if [ -f "$directory/$source.json" ]; then
  cat "$directory/$source.json"
  [ "$source" = local ] && touch "$directory/local-finished"
  exit 0
else
  [ "$source" = local ] && touch "$directory/local-finished"
  echo "ERROR: $source tenant data unavailable" >&2
  exit 1
fi
"#,
                )
                .unwrap();
                fs::set_permissions(path.join("az"), fs::Permissions::from_mode(0o700)).unwrap();
                Self {
                    directory: path,
                    _guard: guard,
                }
            }

            fn list_tenants(&self) -> Result<Vec<TenantDescription>, AzureError> {
                AzureCli {
                    executable: self.directory.join("az").into_os_string(),
                }
                .list_tenants()
            }
        }

        impl Drop for FakeAzure {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.directory);
            }
        }

        fn labels(descriptions: &[TenantDescription]) -> Vec<String> {
            let subscriptions: Vec<Subscription> = serde_json::from_str(
                r#"[
                    {"id":"sub-1","name":"One","tenantId":"tenant-a"},
                    {"id":"sub-2","name":"Two","tenantId":"tenant-b","isDefault":true},
                    {"id":"sub-3","name":"Three","tenantId":"tenant-c"},
                    {"id":"sub-4","name":"Four","tenantId":"tenant-b"}
                ]"#,
            )
            .unwrap();
            tenants_from_subscriptions(&subscriptions, descriptions)
                .iter()
                .map(|tenant| tenant.label())
                .collect()
        }

        #[test]
        fn resource_manager_names_take_precedence_over_local_metadata() {
            let fake = FakeAzure::new(Some(RESOURCE_MANAGER), Some(LOCAL));
            let descriptions = fake.list_tenants().unwrap();
            assert_eq!(
                labels(&descriptions),
                [
                    "Contoso (contoso.example)",
                    "Fabrikam (fabrikam.example)",
                    "northwind.example"
                ]
            );
        }

        #[test]
        fn local_metadata_names_tenants_when_resource_manager_fails() {
            let fake = FakeAzure::new(None, Some(LOCAL));
            let descriptions = fake.list_tenants().unwrap();
            assert_eq!(descriptions.len(), 3);
            assert_eq!(
                labels(&descriptions),
                [
                    "Contoso Local (local.example)",
                    "Fabrikam (fabrikam.example)",
                    "northwind.example"
                ]
            );
        }

        #[test]
        fn resource_manager_names_survive_local_metadata_failure() {
            let fake = FakeAzure::new(Some(RESOURCE_MANAGER), None);
            let descriptions = fake.list_tenants().unwrap();
            assert_eq!(
                labels(&descriptions),
                ["Contoso (contoso.example)", "tenant-b", "tenant-c"]
            );
        }

        #[test]
        fn reports_resource_manager_error_when_both_sources_fail() {
            let fake = FakeAzure::new(None, None);
            let error = fake.list_tenants().unwrap_err();
            assert!(matches!(
                error,
                AzureError::CommandFailed {
                    operation: "az rest tenant list",
                    status: Some(1),
                    ..
                }
            ));
            assert!(error.to_string().contains("rest tenant data unavailable"));
        }

        #[test]
        fn drains_large_local_metadata_pipe_concurrently_without_deadlocking() {
            let mut local_records = Vec::with_capacity(12_000);
            for i in 0..12_000 {
                local_records.push(format!(
                    r#"{{"tenantId":"tenant-b","displayName":"Fabrikam Enterprise Division {i:05}","defaultDomain":"fabrikam.example"}}"#
                ));
            }
            local_records.push(
                r#"{"tenantId":"tenant-c","displayName":"Northwind","defaultDomain":"northwind.example"}"#
                    .to_owned(),
            );
            let local_json = format!("[\n{}\n]", local_records.join(",\n"));
            assert!(local_json.len() > 1_200 * 1024);
            let fake = FakeAzure::new(Some(RESOURCE_MANAGER), Some(&local_json));
            let descriptions = fake.list_tenants().unwrap();
            assert_eq!(
                labels(&descriptions),
                [
                    "Contoso (contoso.example)",
                    "Fabrikam Enterprise Division 00000 (fabrikam.example)",
                    "Northwind (northwind.example)"
                ]
            );
        }
    }
}

mod azure;
mod cache;
mod cli;
mod matcher;
mod model;
mod tui;
mod updater;
mod version;

use std::{
    env,
    error::Error,
    io::{self, IsTerminal, Write},
    process::ExitCode,
    thread,
};

use azure::{AzureCli, AzureError};
use cli::Command;
use matcher::{MatchError, unique_match, unique_tenant_match};
use model::{Subscription, TenantDescription, tenants_from_subscriptions};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("azc: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let command = cli::parse(env::args_os().skip(1))?;

    let azure;
    let (selected, tenant_mode) = match command {
        Command::Help => {
            print!("{}", cli::help());
            return Ok(());
        }
        Command::Version => {
            println!("{}", version::Version);
            return Ok(());
        }
        Command::Update => {
            match updater::update()? {
                #[cfg(not(target_os = "windows"))]
                updater::UpdateOutcome::Completed => println!("azc updated successfully"),
                #[cfg(target_os = "windows")]
                updater::UpdateOutcome::Started => {
                    println!("Cargo update started; keep this terminal open until it finishes")
                }
            }
            return Ok(());
        }
        Command::Interactive { refresh } => {
            azure = AzureCli::from_env();
            (tui::choose_subscription(azure.clone(), refresh)?, false)
        }
        Command::Switch { query, refresh } => {
            azure = AzureCli::from_env();
            let subscriptions = azure.list_subscriptions(refresh)?;
            let selected = match unique_match(&query, &subscriptions) {
                Ok(subscription) => Some(subscription.clone()),
                Err(error) => recover_in_picker(&azure, error, subscriptions, None, query, false)?,
            };
            (selected, false)
        }
        Command::Tenant { query, refresh } => {
            azure = AzureCli::from_env();
            let selected = if let Some(query) = query {
                let (subscriptions, descriptions) = load_tenant_query_accounts(&azure, refresh)?;
                let tenants = tenants_from_subscriptions(&subscriptions, &descriptions);
                match unique_tenant_match(&query, &tenants) {
                    Ok(tenant) => Some(subscriptions[tenant.subscription_index].clone()),
                    Err(error) => recover_in_picker(
                        &azure,
                        error,
                        subscriptions,
                        Some(descriptions),
                        query,
                        true,
                    )?,
                }
            } else {
                tui::choose_tenant(azure.clone(), refresh)?
            };
            (selected, true)
        }
    };

    if let Some(subscription) = selected
        && switch_subscription(&azure, &subscription, confirm_reauthentication)?
    {
        println!("{}", switched_message(&subscription, tenant_mode));
    }
    Ok(())
}

fn load_tenant_query_accounts(
    azure: &AzureCli,
    refresh: bool,
) -> Result<(Vec<Subscription>, Vec<TenantDescription>), AzureError> {
    // `--refresh` rewrites local `azureProfile.json`, which `list_tenants` reads via `az account list --all`.
    if refresh {
        let subscriptions = azure.list_subscriptions(true)?;
        let descriptions = azure.list_tenants().unwrap_or_default();
        return Ok((subscriptions, descriptions));
    }

    thread::scope(|scope| {
        let descriptions = scope.spawn(|| azure.list_tenants());
        let subscriptions = azure.list_subscriptions(false);
        let descriptions = descriptions
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
        subscriptions.map(|subscriptions| (subscriptions, descriptions.unwrap_or_default()))
    })
}

fn switched_message(subscription: &Subscription, tenant_mode: bool) -> String {
    if tenant_mode {
        format!(
            "Switched to tenant {} via {} ({})",
            subscription.tenant_id, subscription.name, subscription.id
        )
    } else {
        format!(
            "Switched to {} ({}) in tenant {}",
            subscription.name, subscription.id, subscription.tenant_id
        )
    }
}

fn can_open_picker() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Opens the picker for a direct query without a unique match, or returns the
/// match error when no terminal is available.
fn recover_in_picker(
    azure: &AzureCli,
    error: MatchError,
    subscriptions: Vec<Subscription>,
    descriptions: Option<Vec<TenantDescription>>,
    query: String,
    tenant_mode: bool,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    if !can_open_picker() {
        return Err(error.into());
    }
    let shortlist = match error {
        MatchError::NotFound { .. } => None,
        MatchError::Ambiguous { candidates, .. } => {
            Some(candidates.into_iter().map(|(index, _)| index).collect())
        }
    };
    tui::choose_from_query(
        azure.clone(),
        subscriptions,
        descriptions,
        query,
        tenant_mode,
        shortlist,
    )
}

fn switch_subscription(
    azure: &AzureCli,
    subscription: &Subscription,
    confirm: impl FnOnce(&Subscription, &AzureError) -> Result<bool, Box<dyn Error>>,
) -> Result<bool, Box<dyn Error>> {
    match azure.set_subscription(&subscription.id) {
        Ok(()) => return Ok(true),
        Err(error) if error.requires_reauthentication() => {
            if !confirm(subscription, &error)? {
                return Ok(false);
            }
        }
        Err(error) => return Err(error.into()),
    }
    azure.login(&subscription.tenant_id)?;
    azure.set_subscription(&subscription.id)?;
    Ok(true)
}

fn confirm_reauthentication(
    subscription: &Subscription,
    error: &AzureError,
) -> Result<bool, Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        let tenant = if subscription.tenant_id.trim().is_empty() {
            String::new()
        } else {
            format!(" --tenant {}", subscription.tenant_id)
        };
        return Err(format!(
            "{error}\nAzure sign-in may have expired or access may have changed. Run 'az login{tenant}', then retry azc."
        )
        .into());
    }

    eprintln!(
        "azc: Azure could not select {} ({}). Your sign-in may have expired or access may have changed.",
        subscription.name, subscription.id
    );
    if !subscription.tenant_id.trim().is_empty() {
        eprintln!(
            "Sign in to tenant {} and retry this subscription? [y/N]",
            subscription.tenant_id
        );
    } else {
        eprintln!("Sign in to Azure and retry this subscription? [y/N]");
    }
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes"))
}

#[cfg(test)]
mod tests {
    use super::switched_message;
    use crate::model::Subscription;

    #[test]
    fn formats_subscription_and_tenant_switch_confirmations() {
        let subscription = Subscription {
            id: "sub-id".to_owned(),
            name: "Payments".to_owned(),
            tenant_id: "tenant-a".to_owned(),
            is_default: false,
        };

        assert_eq!(
            switched_message(&subscription, false),
            "Switched to Payments (sub-id) in tenant tenant-a"
        );
        assert_eq!(
            switched_message(&subscription, true),
            "Switched to tenant tenant-a via Payments (sub-id)"
        );
    }

    #[cfg(unix)]
    mod tenant_query_loading {
        use super::super::load_tenant_query_accounts;
        use crate::{
            azure::{AzureCli, AzureError},
            model::{Subscription, TenantDescription, tenants_from_subscriptions},
        };
        use std::{
            env, fs,
            hint::black_box,
            os::unix::fs::PermissionsExt,
            path::PathBuf,
            sync::{
                Mutex, MutexGuard,
                atomic::{AtomicU64, Ordering},
            },
            time::Instant,
        };

        // Serialize env override and executable creation across Unix tests in this module.
        static FAKE_AZ_LOCK: Mutex<()> = Mutex::new(());

        struct FakeAzure {
            directory: PathBuf,
            previous_az_bin: Option<std::ffi::OsString>,
            _guard: MutexGuard<'static, ()>,
        }

        impl FakeAzure {
            fn new(script_body: &str) -> Self {
                let guard = FAKE_AZ_LOCK
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let directory = env::temp_dir().join(format!(
                    "azc-main-tenant-query-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir(&directory).unwrap();
                let script_path = directory.join("az");
                fs::write(&script_path, script_body).unwrap();
                fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();
                let previous_az_bin = env::var_os("AZC_AZ_BIN");
                // SAFETY: `FAKE_AZ_LOCK` serializes all reads/writes of `AZC_AZ_BIN` in `main::tests`,
                // and no other unit test in the crate mutates `AZC_AZ_BIN`.
                unsafe {
                    env::set_var("AZC_AZ_BIN", &script_path);
                }
                Self {
                    directory,
                    previous_az_bin,
                    _guard: guard,
                }
            }

            fn load(
                &self,
                refresh: bool,
            ) -> Result<(Vec<Subscription>, Vec<TenantDescription>), AzureError> {
                let azure = AzureCli::from_env();
                load_tenant_query_accounts(&azure, refresh)
            }
        }

        impl Drop for FakeAzure {
            fn drop(&mut self) {
                // SAFETY: `self._guard` is still held while restoring `AZC_AZ_BIN`.
                unsafe {
                    match &self.previous_az_bin {
                        Some(value) => env::set_var("AZC_AZ_BIN", value),
                        None => env::remove_var("AZC_AZ_BIN"),
                    }
                }
                let _ = fs::remove_dir_all(&self.directory);
            }
        }

        #[test]
        fn direct_tenant_query_overlaps_subscription_and_tenant_loading_without_refresh() {
            let fake = FakeAzure::new(
                r#"#!/bin/sh
directory=${0%/*}
case "$*" in
  'account list --output json --only-show-errors')
    attempts=0
    while [ ! -f "$directory/tenants-started" ]; do
      attempts=$((attempts + 1))
      if [ "$attempts" -gt 40 ]; then
        echo 'ERROR: list_subscriptions ran sequentially before list_tenants started' >&2
        exit 3
      fi
      sleep 0.05
    done
    cat <<'EOF'
[{"id":"sub-1","name":"One","tenantId":"tenant-a","isDefault":true},{"id":"sub-2","name":"Two","tenantId":"tenant-b"}]
EOF
    ;;
  'rest --method get --url https://management.azure.com/tenants?api-version=2022-12-01 --query value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain} --output json')
    touch "$directory/tenants-started"
    cat <<'EOF'
[{"tenantId":"tenant-a","displayName":"Contoso","defaultDomain":"contoso.example"}]
EOF
    ;;
  'account list --all --query [].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain} --output json --only-show-errors')
    touch "$directory/tenants-started"
    cat <<'EOF'
[{"tenantId":"tenant-b","displayName":"Fabrikam","defaultDomain":"fabrikam.example"}]
EOF
    ;;
  *) exit 2 ;;
esac
"#,
            );

            let (subscriptions, descriptions) = fake.load(false).unwrap();
            let labels: Vec<String> = tenants_from_subscriptions(&subscriptions, &descriptions)
                .iter()
                .map(|tenant| tenant.label())
                .collect();
            assert_eq!(
                labels,
                ["Contoso (contoso.example)", "Fabrikam (fabrikam.example)"]
            );
        }

        #[test]
        fn direct_tenant_query_propagates_subscription_error_after_joining_tenant_thread() {
            let fake = FakeAzure::new(
                r#"#!/bin/sh
directory=${0%/*}
case "$*" in
  'account list --output json --only-show-errors')
    echo 'ERROR: Please run "az login" to setup account.' >&2
    exit 1
    ;;
  'rest --method get --url https://management.azure.com/tenants?api-version=2022-12-01 --query value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain} --output json')
    sleep 0.1
    touch "$directory/rest-finished"
    echo '[]'
    ;;
  'account list --all --query [].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain} --output json --only-show-errors')
    sleep 0.1
    touch "$directory/all-finished"
    echo '[]'
    ;;
  *) exit 2 ;;
esac
"#,
            );

            let error = fake.load(false).unwrap_err();
            assert!(error.to_string().contains("az login"));
            assert!(fake.directory.join("rest-finished").exists());
            assert!(fake.directory.join("all-finished").exists());
        }

        #[test]
        fn direct_tenant_query_with_refresh_finishes_subscriptions_before_starting_tenants() {
            let fake = FakeAzure::new(
                r#"#!/bin/sh
directory=${0%/*}
case "$*" in
  'account list --output json --only-show-errors --refresh')
    sleep 0.1
    cat <<'EOF' > "$directory/refreshed-metadata.json"
[{"tenantId":"tenant-a","displayName":"Contoso Refreshed","defaultDomain":"contoso.example"},{"tenantId":"tenant-b","displayName":"Fabrikam Refreshed","defaultDomain":"fabrikam.example"}]
EOF
    touch "$directory/refresh-finished"
    cat <<'EOF'
[{"id":"sub-1","name":"One","tenantId":"tenant-a"},{"id":"sub-2","name":"Two","tenantId":"tenant-b","isDefault":true}]
EOF
    ;;
  'rest --method get --url https://management.azure.com/tenants?api-version=2022-12-01 --query value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain} --output json')
    if [ ! -f "$directory/refresh-finished" ]; then
      echo 'ERROR: rest started before --refresh finished' >&2
      exit 3
    fi
    echo '[{"tenantId":"tenant-a","displayName":null,"defaultDomain":null}]'
    ;;
  'account list --all --query [].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain} --output json --only-show-errors')
    if [ ! -f "$directory/refresh-finished" ]; then
      echo 'ERROR: account list --all started before --refresh finished' >&2
      exit 3
    fi
    cat "$directory/refreshed-metadata.json"
    ;;
  *) exit 2 ;;
esac
"#,
            );

            let (subscriptions, descriptions) = fake.load(true).unwrap();
            let labels: Vec<String> = tenants_from_subscriptions(&subscriptions, &descriptions)
                .iter()
                .map(|tenant| tenant.label())
                .collect();
            assert_eq!(
                labels,
                [
                    "Contoso Refreshed (contoso.example)",
                    "Fabrikam Refreshed (fabrikam.example)"
                ]
            );
        }

        #[test]
        #[ignore = "benchmark: cargo test --release --locked -- --ignored --nocapture bench_direct_tenant_query_account_loading"]
        fn bench_direct_tenant_query_account_loading() {
            let _fake = FakeAzure::new(
                r#"#!/bin/sh
sleep 0.15
case "$*" in
  'account list --output json --only-show-errors' | 'account list --output json --only-show-errors --refresh')
    cat <<'EOF'
[{"id":"sub-1","name":"One","tenantId":"tenant-a","isDefault":true},{"id":"sub-2","name":"Two","tenantId":"tenant-b"}]
EOF
    ;;
  'rest --method get --url https://management.azure.com/tenants?api-version=2022-12-01 --query value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain} --output json')
    cat <<'EOF'
[{"tenantId":"tenant-a","displayName":"Contoso","defaultDomain":"contoso.example"}]
EOF
    ;;
  'account list --all --query [].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain} --output json --only-show-errors')
    cat <<'EOF'
[{"tenantId":"tenant-b","displayName":"Fabrikam","defaultDomain":"fabrikam.example"}]
EOF
    ;;
  *) exit 2 ;;
esac
"#,
            );
            let azure = AzureCli::from_env();

            // Warmup
            let _ = black_box(load_tenant_query_accounts(black_box(&azure), false).unwrap());

            let runs = 5;
            let mut samples_ms = Vec::with_capacity(runs);
            for _ in 0..runs {
                let start = Instant::now();
                let loaded =
                    load_tenant_query_accounts(black_box(&azure), black_box(false)).unwrap();
                black_box(loaded);
                samples_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            samples_ms.sort_by(|a, b| a.total_cmp(b));
            let median_ms = samples_ms[runs / 2];
            let mean_ms = samples_ms.iter().sum::<f64>() / runs as f64;
            println!(
                "bench_direct_tenant_query_account_loading (refresh=false, 150ms fake az delay): median={median_ms:.2}ms mean={mean_ms:.2}ms samples={samples_ms:.2?}"
            );
        }
    }
}

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
                let subscriptions = azure.list_subscriptions(refresh)?;
                let descriptions = azure.list_tenants().unwrap_or_default();
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
}

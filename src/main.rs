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
use matcher::{MatchError, TenantMatchError, unique_match, unique_tenant_match};
use model::{Subscription, tenants_from_subscriptions};

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

    match command {
        Command::Help => print!("{}", cli::help()),
        Command::Version => println!("{}", version::Version),
        Command::Update => match updater::update()? {
            #[cfg(not(target_os = "windows"))]
            updater::UpdateOutcome::Completed => println!("azc updated successfully"),
            #[cfg(target_os = "windows")]
            updater::UpdateOutcome::Started => {
                println!("Cargo update started; keep this terminal open until it finishes")
            }
        },
        Command::Interactive { refresh } => {
            let azure = AzureCli::from_env();
            if let Some(subscription) = tui::choose_subscription(azure.clone(), refresh)? {
                if !switch_subscription(&azure, &subscription, confirm_reauthentication)? {
                    return Ok(());
                }
                println!(
                    "Switched to {} ({}) in tenant {}",
                    subscription.name, subscription.id, subscription.tenant_id
                );
            }
        }
        Command::Switch { query, refresh } => {
            let azure = AzureCli::from_env();
            let subscriptions = azure.list_subscriptions(refresh)?;
            let picked;
            let subscription = match unique_match(&query, &subscriptions) {
                Ok(subscription) => subscription,
                Err(error) => {
                    if !can_open_picker() {
                        return Err(error.into());
                    }
                    let shortlist = match error {
                        MatchError::NotFound(_) => None,
                        MatchError::Ambiguous { candidates, .. } => {
                            Some(candidates.into_iter().map(|(index, _)| index).collect())
                        }
                    };
                    let Some(subscription) = tui::choose_from_query(
                        azure.clone(),
                        subscriptions,
                        None,
                        query,
                        false,
                        shortlist,
                    )?
                    else {
                        return Ok(());
                    };
                    picked = subscription;
                    &picked
                }
            };
            if !switch_subscription(&azure, subscription, confirm_reauthentication)? {
                return Ok(());
            }
            println!(
                "Switched to {} ({}) in tenant {}",
                subscription.name, subscription.id, subscription.tenant_id
            );
        }
        Command::Tenant { query, refresh } => {
            let azure = AzureCli::from_env();
            let subscription = if let Some(query) = query {
                let subscriptions = azure.list_subscriptions(refresh)?;
                let descriptions = azure.list_tenants().unwrap_or_default();
                let tenants = tenants_from_subscriptions(&subscriptions, &descriptions);
                match unique_tenant_match(&query, &tenants) {
                    Ok(tenant) => subscriptions[tenant.subscription_index].clone(),
                    Err(error) => {
                        if !can_open_picker() {
                            return Err(error.into());
                        }
                        let shortlist = match error {
                            TenantMatchError::NotFound(_) => None,
                            TenantMatchError::Ambiguous { candidates, .. } => {
                                Some(candidates.into_iter().map(|(index, _)| index).collect())
                            }
                        };
                        let Some(subscription) = tui::choose_from_query(
                            azure.clone(),
                            subscriptions,
                            Some(descriptions),
                            query,
                            true,
                            shortlist,
                        )?
                        else {
                            return Ok(());
                        };
                        subscription
                    }
                }
            } else {
                let Some(subscription) = tui::choose_tenant(azure.clone(), refresh)? else {
                    return Ok(());
                };
                subscription
            };
            if !switch_subscription(&azure, &subscription, confirm_reauthentication)? {
                return Ok(());
            }
            println!(
                "Switched to tenant {} via {} ({})",
                subscription.tenant_id, subscription.name, subscription.id
            );
        }
    }

    Ok(())
}

fn can_open_picker() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
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

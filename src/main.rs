mod azure;
mod cli;
mod matcher;
mod model;
mod tui;
mod updater;
mod version;

use std::{env, error::Error, process::ExitCode};

use azure::AzureCli;
use cli::Command;
use matcher::unique_match;

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
                azure.set_subscription(&subscription.id)?;
                println!("Switched to {} ({})", subscription.name, subscription.id);
            }
        }
        Command::Switch { query, refresh } => {
            let azure = AzureCli::from_env();
            let subscriptions = azure.list_subscriptions(refresh)?;
            let subscription = unique_match(&query, &subscriptions)?;
            azure.set_subscription(&subscription.id)?;
            println!("Switched to {} ({})", subscription.name, subscription.id);
        }
    }

    Ok(())
}

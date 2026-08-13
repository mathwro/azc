use std::{env, process::Command};

fn main() {
    println!("cargo::rerun-if-env-changed=AZC_COMMIT_SHA");
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let commit = env::var("AZC_COMMIT_SHA")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git_output(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned());
    let source_date_epoch = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            git_output(&["show", "-s", "--format=%ct", "HEAD"])?
                .parse()
                .ok()
        })
        .unwrap_or(0);

    println!("cargo::rustc-env=AZC_COMMIT_SHA={commit}");
    println!(
        "cargo::rustc-env=AZC_BUILD_DATE={}",
        unix_epoch_date(source_date_epoch)
    );
}

fn git_output(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn unix_epoch_date(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

// Howard Hinnant's civil calendar conversion, with 1970-01-01 as day zero.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::unix_epoch_date;

    #[test]
    fn formats_epoch_as_utc_date() {
        assert_eq!(unix_epoch_date(0), "1970-01-01");
        assert_eq!(unix_epoch_date(1_786_579_200), "2026-08-13");
    }
}

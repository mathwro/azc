use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=RELEASE_COMMIT");
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");
    if let Some(manifest_dir) = env::var_os("CARGO_MANIFEST_DIR") {
        for path in git_watch_paths(Path::new(&manifest_dir)) {
            println!("cargo::rerun-if-changed={}", path.display());
        }
    }
    let commit = env::var("RELEASE_COMMIT")
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

fn git_watch_paths(manifest_dir: &Path) -> Vec<PathBuf> {
    let Some(git_dir) = resolve_git_dir(manifest_dir) else {
        return Vec::new();
    };
    let common_dir = resolve_common_dir(&git_dir);
    let head = git_dir.join("HEAD");
    if !head.is_file() {
        return Vec::new();
    }

    let mut paths = vec![head.clone()];
    if let Ok(contents) = fs::read_to_string(&head)
        && let Some(reference) = contents.trim().strip_prefix("ref:")
    {
        let ref_path = common_dir.join(reference.trim());
        if ref_path.is_file() {
            paths.push(ref_path);
        } else if let Some(parent) = ref_path
            .parent()
            .and_then(|dir| dir.ancestors().find(|d| d.is_dir()))
        {
            paths.push(parent.to_path_buf());
        }
        let packed_refs = common_dir.join("packed-refs");
        if packed_refs.is_file() {
            paths.push(packed_refs);
        }
    }
    paths
}

fn resolve_git_dir(manifest_dir: &Path) -> Option<PathBuf> {
    let dot_git = manifest_dir.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let contents = fs::read_to_string(dot_git).ok()?;
    let git_dir = contents.trim().strip_prefix("gitdir:")?.trim();
    (!git_dir.is_empty()).then(|| manifest_dir.join(git_dir))
}

fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    let Ok(contents) = fs::read_to_string(git_dir.join("commondir")) else {
        return git_dir.to_path_buf();
    };
    let common_dir = contents.trim();
    if common_dir.is_empty() {
        git_dir.to_path_buf()
    } else {
        git_dir.join(common_dir)
    }
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
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{git_watch_paths, unix_epoch_date};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "azc-build-test-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn formats_epoch_as_utc_date() {
        assert_eq!(unix_epoch_date(0), "1970-01-01");
        assert_eq!(unix_epoch_date(1_786_579_200), "2026-08-13");
    }

    #[test]
    fn ignores_missing_git_metadata_for_tarball_builds() {
        let temp = TempDir::new("tarball");
        assert!(git_watch_paths(temp.path()).is_empty());
    }

    #[test]
    fn watches_head_ref_and_packed_refs_in_standard_git_dir() {
        let temp = TempDir::new("standard");
        let git_dir = temp.path().join(".git");
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let branch_ref = git_dir.join("refs/heads/main");
        let packed_refs = git_dir.join("packed-refs");
        fs::write(&branch_ref, "0123456789abcdef\n").unwrap();
        fs::write(&packed_refs, "# pack-refs\n").unwrap();

        assert_eq!(
            git_watch_paths(temp.path()),
            vec![git_dir.join("HEAD"), branch_ref, packed_refs]
        );
    }

    #[test]
    fn watches_detached_head_without_missing_refs() {
        let temp = TempDir::new("detached");
        let git_dir = temp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "0123456789abcdef\n").unwrap();

        assert_eq!(git_watch_paths(temp.path()), vec![git_dir.join("HEAD")]);
    }

    #[test]
    fn watches_existing_parent_dir_when_branch_ref_is_packed_only() {
        let temp = TempDir::new("packed");
        let git_dir = temp.path().join(".git");
        let heads_dir = git_dir.join("refs/heads");
        fs::create_dir_all(&heads_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature/nested\n").unwrap();
        let packed_refs = git_dir.join("packed-refs");
        fs::write(&packed_refs, "# pack-refs\n").unwrap();

        assert_eq!(
            git_watch_paths(temp.path()),
            vec![git_dir.join("HEAD"), heads_dir, packed_refs]
        );
    }

    #[test]
    fn resolves_gitfile_and_commondir_for_worktrees() {
        let temp = TempDir::new("worktree");
        let common_dir = temp.path().join("repo/.git");
        let worktree_git_dir = common_dir.join("worktrees/feature");
        let worktree = temp.path().join("wt");
        fs::create_dir_all(common_dir.join("refs/heads")).unwrap();
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::create_dir_all(&worktree).unwrap();

        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
        fs::write(worktree_git_dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        let branch_ref = common_dir.join("refs/heads/feature");
        let packed_refs = common_dir.join("packed-refs");
        fs::write(&branch_ref, "fedcba9876543210\n").unwrap();
        fs::write(&packed_refs, "# pack-refs\n").unwrap();

        let watched: Vec<_> = git_watch_paths(&worktree)
            .into_iter()
            .map(|path| fs::canonicalize(path).unwrap())
            .collect();
        assert_eq!(
            watched,
            vec![
                fs::canonicalize(worktree_git_dir.join("HEAD")).unwrap(),
                fs::canonicalize(branch_ref).unwrap(),
                fs::canonicalize(packed_refs).unwrap(),
            ]
        );
    }
}

use std::{error::Error, fmt};

use crate::model::{Subscription, Tenant};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct MatchScore(u8);

pub fn ranked_indices(query: &str, subscriptions: &[Subscription]) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    let mut matches = subscriptions
        .iter()
        .enumerate()
        .filter_map(|(index, subscription)| {
            score(&query, subscription).map(|score| {
                (
                    index,
                    score,
                    subscription.name.to_lowercase(),
                    subscription.is_default,
                )
            })
        })
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| right.3.cmp(&left.3))
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.0.cmp(&right.0))
    });

    matches.into_iter().map(|entry| entry.0).collect()
}

pub fn ranked_tenant_indices(query: &str, tenants: &[Tenant]) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    let mut matches = tenants
        .iter()
        .enumerate()
        .filter_map(|(index, tenant)| {
            score_tenant(&query, tenant)
                .map(|score| (index, score, tenant.id.to_lowercase(), tenant.is_current))
        })
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| right.3.cmp(&left.3))
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.0.cmp(&right.0))
    });

    matches.into_iter().map(|entry| entry.0).collect()
}

pub fn unique_tenant_match<'a>(
    query: &str,
    tenants: &'a [Tenant],
) -> Result<&'a Tenant, TenantMatchError> {
    let query = query.trim();
    let normalized_query = query.to_lowercase();
    let indices = ranked_tenant_indices(query, tenants);
    let Some(&first_index) = indices.first() else {
        return Err(TenantMatchError::NotFound(query.to_owned()));
    };

    let best_score = score_tenant(&normalized_query, &tenants[first_index])
        .expect("ranked entries always have a score");
    let tied = indices
        .iter()
        .copied()
        .take_while(|&index| score_tenant(&normalized_query, &tenants[index]) == Some(best_score))
        .collect::<Vec<_>>();

    if tied.len() == 1 {
        return Ok(&tenants[first_index]);
    }

    Err(TenantMatchError::Ambiguous {
        query: query.to_owned(),
        candidates: tied
            .into_iter()
            .map(|index| tenants[index].id.clone())
            .collect(),
    })
}

fn score_tenant(query: &str, tenant: &Tenant) -> Option<MatchScore> {
    if query.is_empty() {
        return Some(if tenant.is_current {
            MatchScore(0)
        } else {
            MatchScore(1)
        });
    }

    let name = tenant.name.to_lowercase();
    let id = tenant.id.to_lowercase();
    if name == query || id == query {
        Some(MatchScore(0))
    } else if name.starts_with(query) || id.starts_with(query) {
        Some(MatchScore(1))
    } else if name.contains(query) || id.contains(query) {
        Some(MatchScore(2))
    } else {
        None
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum TenantMatchError {
    NotFound(String),
    Ambiguous {
        query: String,
        candidates: Vec<String>,
    },
}

impl fmt::Display for TenantMatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(query) => write!(f, "no tenant matches '{query}'"),
            Self::Ambiguous { query, candidates } => {
                write!(
                    f,
                    "'{query}' is ambiguous; matches: {}",
                    candidates.join(", ")
                )
            }
        }
    }
}

impl Error for TenantMatchError {}

pub fn unique_match<'a>(
    query: &str,
    subscriptions: &'a [Subscription],
) -> Result<&'a Subscription, MatchError> {
    let query = query.trim();
    let normalized_query = query.to_lowercase();
    let indices = ranked_indices(query, subscriptions);
    let Some(&first_index) = indices.first() else {
        return Err(MatchError::NotFound(query.to_owned()));
    };

    let best_score = score(&normalized_query, &subscriptions[first_index])
        .expect("ranked entries always have a score");
    let tied = indices
        .iter()
        .copied()
        .take_while(|&index| score(&normalized_query, &subscriptions[index]) == Some(best_score))
        .collect::<Vec<_>>();

    if tied.len() == 1 {
        return Ok(&subscriptions[first_index]);
    }

    Err(MatchError::Ambiguous {
        query: query.to_owned(),
        candidates: tied
            .into_iter()
            .map(|index| subscriptions[index].name.clone())
            .collect(),
    })
}

fn score(query: &str, subscription: &Subscription) -> Option<MatchScore> {
    if query.is_empty() {
        return Some(if subscription.is_default {
            MatchScore(0)
        } else {
            MatchScore(1)
        });
    }

    let name = subscription.name.to_lowercase();
    let id = subscription.id.to_lowercase();
    let tenant_id = subscription.tenant_id.to_lowercase();

    if name == query || id == query {
        Some(MatchScore(0))
    } else if name.starts_with(query) {
        Some(MatchScore(1))
    } else if name
        .split(|character: char| !character.is_alphanumeric())
        .any(|word| word.starts_with(query))
    {
        Some(MatchScore(2))
    } else if name.contains(query) {
        Some(MatchScore(3))
    } else if id.contains(query) || tenant_id.contains(query) {
        Some(MatchScore(4))
    } else {
        None
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum MatchError {
    NotFound(String),
    Ambiguous {
        query: String,
        candidates: Vec<String>,
    },
}

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(query) => write!(f, "no subscription matches '{query}'"),
            Self::Ambiguous { query, candidates } => {
                write!(
                    f,
                    "'{query}' is ambiguous; matches: {}",
                    candidates.join(", ")
                )
            }
        }
    }
}

impl Error for MatchError {}

#[cfg(test)]
mod tests {
    use super::{
        MatchError, TenantMatchError, ranked_indices, ranked_tenant_indices, unique_match,
        unique_tenant_match,
    };
    use crate::model::{Subscription, Tenant};

    fn subscription(name: &str, id: &str, is_default: bool) -> Subscription {
        Subscription {
            id: id.to_owned(),
            name: name.to_owned(),
            tenant_id: "tenant-one".to_owned(),
            is_default,
            state: "Enabled".to_owned(),
        }
    }

    #[test]
    fn ranks_exact_prefix_word_and_substring_matches() {
        let subscriptions = vec![
            subscription("North HR Archive", "1", false),
            subscription("HR", "2", false),
            subscription("HR Production", "3", false),
            subscription("Cohr Platform", "4", false),
        ];

        let names = ranked_indices("hr", &subscriptions)
            .into_iter()
            .map(|index| subscriptions[index].name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            ["HR", "HR Production", "North HR Archive", "Cohr Platform"]
        );
    }

    #[test]
    fn empty_query_puts_current_subscription_first() {
        let subscriptions = vec![
            subscription("Alpha", "1", false),
            subscription("Zulu", "2", true),
        ];

        assert_eq!(ranked_indices("", &subscriptions), [1, 0]);
    }

    #[test]
    fn unique_best_rank_can_select_among_lower_ranked_matches() {
        let subscriptions = vec![
            subscription("HR", "1", false),
            subscription("Shared Services", "2", false),
        ];

        assert_eq!(unique_match("hr", &subscriptions).unwrap().id, "1");
    }

    #[test]
    fn equal_best_matches_are_ambiguous() {
        let subscriptions = vec![
            subscription("HR Production", "1", false),
            subscription("HR Development", "2", false),
        ];

        assert_eq!(
            unique_match("hr", &subscriptions),
            Err(MatchError::Ambiguous {
                query: "hr".to_owned(),
                candidates: vec!["HR Development".to_owned(), "HR Production".to_owned()],
            })
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_supports_ids() {
        let subscriptions = vec![subscription(
            "People Operations",
            "ABCDEF00-0000-0000-0000-000000000000",
            false,
        )];

        assert_eq!(
            unique_match("people", &subscriptions).unwrap().name,
            "People Operations"
        );
        assert_eq!(
            unique_match("abcdef", &subscriptions).unwrap().name,
            "People Operations"
        );
    }

    #[test]
    fn ranks_current_tenant_first_and_matches_tenant_ids() {
        let tenants = vec![
            Tenant {
                id: "aaaaaaaa".to_owned(),
                name: "Tenant A".to_owned(),
                subscription_index: 0,
                subscription_count: 1,
                is_current: false,
            },
            Tenant {
                id: "bbbbbbbb".to_owned(),
                name: "Tenant B".to_owned(),
                subscription_index: 1,
                subscription_count: 2,
                is_current: true,
            },
        ];

        assert_eq!(ranked_tenant_indices("", &tenants), [1, 0]);
        assert_eq!(
            unique_tenant_match("tenant b", &tenants).unwrap().id,
            "bbbbbbbb"
        );
        assert_eq!(
            unique_tenant_match("AAAA", &tenants).unwrap().id,
            "aaaaaaaa"
        );
    }

    #[test]
    fn equal_tenant_prefixes_are_ambiguous() {
        let tenants = vec![
            Tenant {
                id: "aaaa-one".to_owned(),
                name: "Tenant One".to_owned(),
                subscription_index: 0,
                subscription_count: 1,
                is_current: false,
            },
            Tenant {
                id: "aaaa-two".to_owned(),
                name: "Tenant Two".to_owned(),
                subscription_index: 1,
                subscription_count: 1,
                is_current: false,
            },
        ];

        assert_eq!(
            unique_tenant_match("aaaa", &tenants),
            Err(TenantMatchError::Ambiguous {
                query: "aaaa".to_owned(),
                candidates: vec!["aaaa-one".to_owned(), "aaaa-two".to_owned()],
            })
        );
    }
}

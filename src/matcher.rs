use std::{error::Error, fmt};

use crate::model::{Subscription, Tenant};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct MatchScore(u8);

pub fn ranked_indices(query: &str, subscriptions: &[Subscription]) -> Vec<usize> {
    subscription_matches(query, subscriptions)
        .into_iter()
        .map(|entry| entry.0)
        .collect()
}

fn subscription_matches(
    query: &str,
    subscriptions: &[Subscription],
) -> Vec<(usize, MatchScore, String, bool)> {
    let query = query.trim().to_lowercase();
    let fragments = name_fragments(&query);
    let mut matches = subscriptions
        .iter()
        .enumerate()
        .filter_map(|(index, subscription)| {
            score(&query, &fragments, subscription).map(|score| {
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

    matches
}

pub fn ranked_tenant_indices(query: &str, tenants: &[Tenant]) -> Vec<usize> {
    tenant_matches(query, tenants)
        .into_iter()
        .map(|entry| entry.0)
        .collect()
}

fn tenant_matches(query: &str, tenants: &[Tenant]) -> Vec<(usize, MatchScore, String, bool)> {
    let query = query.trim().to_lowercase();
    let fragments = name_fragments(&query);
    let mut matches = tenants
        .iter()
        .enumerate()
        .filter_map(|(index, tenant)| {
            score_tenant(&query, &fragments, tenant)
                .map(|score| (index, score, tenant.name.to_lowercase(), tenant.is_current))
        })
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| right.3.cmp(&left.3))
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.0.cmp(&right.0))
    });

    matches
}

pub fn unique_tenant_match<'a>(
    query: &str,
    tenants: &'a [Tenant],
) -> Result<&'a Tenant, TenantMatchError> {
    let query = query.trim();
    let matches = tenant_matches(query, tenants);
    let Some(&(first_index, best_score, _, _)) = matches.first() else {
        return Err(TenantMatchError::NotFound(query.to_owned()));
    };

    if matches.get(1).is_none_or(|entry| entry.1 != best_score) {
        return Ok(&tenants[first_index]);
    }

    Err(TenantMatchError::Ambiguous {
        query: query.to_owned(),
        candidates: matches
            .into_iter()
            .take_while(|entry| entry.1 == best_score)
            .map(|(index, _, _, _)| (index, tenants[index].id.clone()))
            .collect(),
    })
}

fn score_tenant(query: &str, fragments: &[&str], tenant: &Tenant) -> Option<MatchScore> {
    if query.is_empty() {
        return Some(if tenant.is_current {
            MatchScore(0)
        } else {
            MatchScore(1)
        });
    }

    let name = tenant.name.to_lowercase();
    let id = tenant.id.to_lowercase();
    let domain = tenant.default_domain.to_lowercase();
    if name == query || domain == query || id == query {
        Some(MatchScore(0))
    } else if name.starts_with(query) || domain.starts_with(query) || id.starts_with(query) {
        Some(MatchScore(1))
    } else if name.contains(query) || domain.contains(query) || id.contains(query) {
        Some(MatchScore(2))
    } else if !fragments.is_empty() && fragments.iter().all(|fragment| name.contains(*fragment)) {
        Some(MatchScore(3))
    } else {
        None
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum TenantMatchError {
    NotFound(String),
    Ambiguous {
        query: String,
        candidates: Vec<(usize, String)>,
    },
}

impl fmt::Display for TenantMatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(query) => write!(f, "no tenant matches '{query}'"),
            Self::Ambiguous { query, candidates } => {
                write!(f, "'{query}' is ambiguous; matches: ")?;
                for (position, (_, label)) in candidates.iter().enumerate() {
                    if position != 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(label)?;
                }
                Ok(())
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
    let matches = subscription_matches(query, subscriptions);
    let Some(&(first_index, best_score, _, _)) = matches.first() else {
        return Err(MatchError::NotFound(query.to_owned()));
    };

    if matches.get(1).is_none_or(|entry| entry.1 != best_score) {
        return Ok(&subscriptions[first_index]);
    }

    Err(MatchError::Ambiguous {
        query: query.to_owned(),
        candidates: matches
            .into_iter()
            .take_while(|entry| entry.1 == best_score)
            .map(|(index, _, _, _)| (index, subscriptions[index].name.clone()))
            .collect(),
    })
}

fn name_fragments(query: &str) -> Vec<&str> {
    let mut fragments = query.split_whitespace();
    let Some(first) = fragments.next() else {
        return Vec::new();
    };
    let Some(second) = fragments.next() else {
        return Vec::new();
    };
    let mut terms = vec![first, second];
    terms.extend(fragments);
    terms
}

fn score(query: &str, fragments: &[&str], subscription: &Subscription) -> Option<MatchScore> {
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
    } else if !fragments.is_empty() && fragments.iter().all(|fragment| name.contains(*fragment)) {
        Some(MatchScore(5))
    } else {
        None
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum MatchError {
    NotFound(String),
    Ambiguous {
        query: String,
        candidates: Vec<(usize, String)>,
    },
}

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(query) => write!(f, "no subscription matches '{query}'"),
            Self::Ambiguous { query, candidates } => {
                write!(f, "'{query}' is ambiguous; matches: ")?;
                for (position, (_, label)) in candidates.iter().enumerate() {
                    if position != 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(label)?;
                }
                Ok(())
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
    fn separated_fragments_require_every_term_and_rank_after_contiguous_matches() {
        let subscriptions = vec![
            subscription("prod-platform-payments", "1", false),
            subscription("payments-dev", "2", false),
            subscription("payments prod", "3", false),
            subscription("payments-platform-prod", "4", true),
        ];
        assert_eq!(ranked_indices("PAYMENTS prod", &subscriptions), [2, 3, 0]);
        assert_eq!(ranked_indices("prod\tpayments", &subscriptions), [3, 2, 0]);
        assert!(ranked_indices("payments missing", &subscriptions).is_empty());
        assert_eq!(
            unique_match("payments prod", &subscriptions).unwrap().id,
            "3"
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
                candidates: vec![
                    (1, "HR Development".to_owned()),
                    (0, "HR Production".to_owned())
                ],
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
                default_domain: String::new(),
                subscription_index: 0,
                subscription_count: 1,
                is_current: false,
            },
            Tenant {
                id: "bbbbbbbb".to_owned(),
                name: "Tenant B".to_owned(),
                default_domain: String::new(),
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
                default_domain: String::new(),
                subscription_index: 0,
                subscription_count: 1,
                is_current: false,
            },
            Tenant {
                id: "aaaa-two".to_owned(),
                name: "Tenant Two".to_owned(),
                default_domain: String::new(),
                subscription_index: 1,
                subscription_count: 1,
                is_current: false,
            },
        ];

        assert_eq!(
            unique_tenant_match("aaaa", &tenants),
            Err(TenantMatchError::Ambiguous {
                query: "aaaa".to_owned(),
                candidates: vec![(0, "aaaa-one".to_owned()), (1, "aaaa-two".to_owned())],
            })
        );
    }

    #[test]
    fn fragment_ambiguity_preserves_source_indices_for_duplicate_names() {
        let subscriptions = vec![
            subscription("payments-platform-prod", "1", false),
            subscription("payments-dev", "2", false),
            subscription("payments-platform-prod", "3", true),
        ];

        assert_eq!(
            unique_match("prod payments", &subscriptions),
            Err(MatchError::Ambiguous {
                query: "prod payments".to_owned(),
                candidates: vec![
                    (2, "payments-platform-prod".to_owned()),
                    (0, "payments-platform-prod".to_owned()),
                ],
            })
        );
    }

    #[test]
    fn ambiguity_candidates_exclude_lower_contiguous_and_fragment_matches() {
        let subscriptions = vec![
            subscription("North payments prod", "1", true),
            subscription("payments prod B", "2", false),
            subscription("payments-platform-prod", "3", false),
            subscription("payments prod A", "4", false),
        ];

        assert_eq!(
            unique_match("payments prod", &subscriptions),
            Err(MatchError::Ambiguous {
                query: "payments prod".to_owned(),
                candidates: vec![
                    (3, "payments prod A".to_owned()),
                    (1, "payments prod B".to_owned()),
                ],
            })
        );
    }

    #[test]
    fn fragments_do_not_expand_id_matching_or_allow_skipped_letters() {
        let subscriptions = vec![
            subscription("payments-platform-prod", "1", true),
            subscription("Unrelated", "id-payments prod-end", false),
            subscription("Other", "prod-payments", false),
        ];

        assert_eq!(ranked_indices("payments prod", &subscriptions), [1, 0]);
        assert_eq!(
            unique_match("payments prod", &subscriptions).unwrap().id,
            "id-payments prod-end"
        );
        assert!(ranked_indices("tenant one", &subscriptions).is_empty());
        assert!(ranked_indices("pymnts prd", &subscriptions).is_empty());
        assert_eq!(ranked_indices("tenant-one", &subscriptions), [0, 2, 1]);
    }

    fn tenant(name: &str, id: &str, domain: &str) -> Tenant {
        Tenant {
            id: id.to_owned(),
            name: name.to_owned(),
            default_domain: domain.to_owned(),
            subscription_index: 0,
            subscription_count: 1,
            is_current: false,
        }
    }

    #[test]
    fn tenant_name_fragments_rank_after_contiguous_matches_in_every_field() {
        let tenants = vec![
            tenant("payments-platform-prod", "1", ""),
            tenant("Other", "2", "prefix-payments prod-suffix"),
            tenant("payments prod", "3", ""),
            tenant("Unrelated", "prefix-payments prod-suffix", ""),
        ];

        assert_eq!(
            ranked_tenant_indices("PAYMENTS prod", &tenants),
            [2, 1, 3, 0]
        );
        assert_eq!(
            unique_tenant_match("payments prod", &tenants).unwrap().id,
            "3"
        );
        assert_eq!(ranked_tenant_indices("prod\tpayments", &tenants), [2, 0]);
        assert!(ranked_tenant_indices("payments missing", &tenants).is_empty());
        assert!(ranked_tenant_indices("pymnts prd", &tenants).is_empty());
        assert_eq!(ranked_tenant_indices("prefix-", &tenants), [1, 3]);
    }

    #[test]
    fn tenant_ambiguity_indices_exclude_lower_matches_and_use_tenant_order() {
        let tenants = vec![
            tenant("payments-platform-prod", "fragment", ""),
            tenant("payments prod", "second", ""),
            tenant("North payments prod", "substring", ""),
            tenant("payments prod", "first", ""),
        ];

        assert_eq!(
            unique_tenant_match("payments prod", &tenants),
            Err(TenantMatchError::Ambiguous {
                query: "payments prod".to_owned(),
                candidates: vec![(1, "second".to_owned()), (3, "first".to_owned())],
            })
        );
        assert_eq!(
            unique_tenant_match("prod payments", &tenants),
            Err(TenantMatchError::Ambiguous {
                query: "prod payments".to_owned(),
                candidates: vec![
                    (2, "substring".to_owned()),
                    (1, "second".to_owned()),
                    (3, "first".to_owned()),
                    (0, "fragment".to_owned()),
                ],
            })
        );
    }
}

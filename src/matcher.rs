use std::{error::Error, fmt};

use crate::model::{Subscription, Tenant};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct MatchScore(u8);

/// A rankable item; ranking and unique selection are shared across item kinds.
trait Candidate {
    /// Noun used in "no <kind> matches" errors.
    const KIND: &'static str;

    /// Display name and current flag used to break score ties.
    fn tie_break(&self) -> (&str, bool);
    /// Scores a candidate against a pre-lowercased query and lowercased name.
    fn score(&self, query: &str, fragments: &[&str], name_lower: &str) -> Option<MatchScore>;
    /// Label listed in ambiguity errors.
    fn describe(&self) -> String;
}

impl Candidate for Subscription {
    const KIND: &'static str = "subscription";

    fn tie_break(&self) -> (&str, bool) {
        (&self.name, self.is_default)
    }

    fn score(&self, query: &str, fragments: &[&str], name_lower: &str) -> Option<MatchScore> {
        score(query, fragments, name_lower, self)
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

impl Candidate for Tenant {
    const KIND: &'static str = "tenant";

    fn tie_break(&self) -> (&str, bool) {
        (&self.name, self.is_current)
    }

    fn score(&self, query: &str, fragments: &[&str], name_lower: &str) -> Option<MatchScore> {
        score_tenant(query, fragments, name_lower, self)
    }

    fn describe(&self) -> String {
        let label = self.label();
        if label == self.id {
            label
        } else {
            format!("{label} ({})", self.id)
        }
    }
}

pub fn ranked_indices(query: &str, subscriptions: &[Subscription]) -> Vec<usize> {
    ranked(query, subscriptions)
}

pub fn ranked_tenant_indices(query: &str, tenants: &[Tenant]) -> Vec<usize> {
    ranked(query, tenants)
}

pub fn unique_match<'a>(
    query: &str,
    subscriptions: &'a [Subscription],
) -> Result<&'a Subscription, MatchError> {
    unique(query, subscriptions)
}

pub fn unique_tenant_match<'a>(
    query: &str,
    tenants: &'a [Tenant],
) -> Result<&'a Tenant, MatchError> {
    unique(query, tenants)
}

fn ranked<T: Candidate>(query: &str, items: &[T]) -> Vec<usize> {
    matches(query, items)
        .into_iter()
        .map(|(index, _)| index)
        .collect()
}

/// Orders matches by score, current item first, case-insensitive name, then source order.
fn matches<T: Candidate>(query: &str, items: &[T]) -> Vec<(usize, MatchScore)> {
    let query = query.trim().to_lowercase();
    let fragments = name_fragments(&query);
    let mut matches = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let (name, is_current) = item.tie_break();
            let name_lower = name.to_lowercase();
            let score = if query.is_empty() {
                MatchScore(u8::from(!is_current))
            } else {
                item.score(&query, &fragments, &name_lower)?
            };
            Some((score, !is_current, name_lower, index))
        })
        .collect::<Vec<_>>();
    matches.sort_unstable();
    matches
        .into_iter()
        .map(|(score, _, _, index)| (index, score))
        .collect()
}

fn unique<'a, T: Candidate>(query: &str, items: &'a [T]) -> Result<&'a T, MatchError> {
    let query = query.trim();
    let matches = matches(query, items);
    let Some(&(first_index, best_score)) = matches.first() else {
        return Err(MatchError::NotFound {
            kind: T::KIND,
            query: query.to_owned(),
        });
    };

    if matches.get(1).is_none_or(|entry| entry.1 != best_score) {
        return Ok(&items[first_index]);
    }

    Err(MatchError::Ambiguous {
        query: query.to_owned(),
        candidates: matches
            .into_iter()
            .take_while(|entry| entry.1 == best_score)
            .map(|(index, _)| (index, items[index].describe()))
            .collect(),
    })
}

fn score_tenant(
    query: &str,
    fragments: &[&str],
    name: &str,
    tenant: &Tenant,
) -> Option<MatchScore> {
    if name == query {
        return Some(MatchScore(0));
    }

    let id = tenant.id.to_lowercase();
    let domain = tenant.default_domain.to_lowercase();
    if domain == query || id == query {
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

fn score(
    query: &str,
    fragments: &[&str],
    name: &str,
    subscription: &Subscription,
) -> Option<MatchScore> {
    if name == query {
        return Some(MatchScore(0));
    }

    let id = subscription.id.to_lowercase();
    if id == query {
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
    } else if id.contains(query) || subscription.tenant_id.to_lowercase().contains(query) {
        Some(MatchScore(4))
    } else if !fragments.is_empty() && fragments.iter().all(|fragment| name.contains(*fragment)) {
        Some(MatchScore(5))
    } else {
        None
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum MatchError {
    NotFound {
        kind: &'static str,
        query: String,
    },
    Ambiguous {
        query: String,
        candidates: Vec<(usize, String)>,
    },
}

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { kind, query } => write!(f, "no {kind} matches '{query}'"),
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
        MatchError, ranked_indices, ranked_tenant_indices, unique_match, unique_tenant_match,
    };
    use crate::model::{Subscription, Tenant};

    fn subscription(name: &str, id: &str, is_default: bool) -> Subscription {
        Subscription {
            id: id.to_owned(),
            name: name.to_owned(),
            tenant_id: "tenant-one".to_owned(),
            is_default,
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
            Err(MatchError::Ambiguous {
                query: "aaaa".to_owned(),
                candidates: vec![
                    (0, "Tenant One (aaaa-one)".to_owned()),
                    (1, "Tenant Two (aaaa-two)".to_owned())
                ],
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
            Err(MatchError::Ambiguous {
                query: "payments prod".to_owned(),
                candidates: vec![
                    (1, "payments prod (second)".to_owned()),
                    (3, "payments prod (first)".to_owned())
                ],
            })
        );
        assert_eq!(
            unique_tenant_match("prod payments", &tenants),
            Err(MatchError::Ambiguous {
                query: "prod payments".to_owned(),
                candidates: vec![
                    (2, "North payments prod (substring)".to_owned()),
                    (1, "payments prod (second)".to_owned()),
                    (3, "payments prod (first)".to_owned()),
                    (0, "payments-platform-prod (fragment)".to_owned()),
                ],
            })
        );
    }

    #[test]
    fn errors_name_the_item_kind_and_label_ambiguous_tenants_with_ids() {
        let tenants = vec![
            tenant("North Europe", "1b2c", "north.example"),
            tenant("North America", "9f8e", ""),
            tenant("", "north-id", ""),
        ];

        assert_eq!(
            unique_tenant_match("north", &tenants)
                .unwrap_err()
                .to_string(),
            "'north' is ambiguous; matches: north-id, North America (9f8e), \
             North Europe (north.example) (1b2c)"
        );
        assert_eq!(
            unique_tenant_match("south", &tenants)
                .unwrap_err()
                .to_string(),
            "no tenant matches 'south'"
        );
        assert_eq!(
            unique_match("south", &[subscription("North", "1", false)])
                .unwrap_err()
                .to_string(),
            "no subscription matches 'south'"
        );
    }

    #[test]
    fn matches_and_sorts_unicode_names_case_insensitively() {
        let subscriptions = vec![
            subscription("Zażółć Dev", "1", false),
            subscription("zażółć alpha", "2", false),
            subscription("Équipe Prod", "3", false),
        ];

        assert_eq!(ranked_indices("ZAŻÓŁĆ", &subscriptions), [1, 0]);
        assert_eq!(ranked_indices("équipe", &subscriptions), [2]);
        assert_eq!(ranked_indices("", &subscriptions), [1, 0, 2]);

        assert_eq!(
            super::Candidate::score(&subscriptions[0], "zażółć", &[], "zażółć dev"),
            Some(super::MatchScore(1))
        );
    }
}

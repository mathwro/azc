use std::{cmp::Ordering, error::Error, fmt};

use crate::model::{Subscription, Tenant};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct MatchScore(u8);

/// A rankable item; ranking and unique selection are shared across item kinds.
trait Candidate {
    /// Noun used in "no <kind> matches" errors.
    const KIND: &'static str;

    /// Display name and current flag used to break score ties.
    fn tie_break(&self) -> (&str, bool);
    /// Scores a candidate against a pre-lowercased query; `name_lower` is `Some` iff `!name.is_ascii()`.
    fn score(
        &self,
        query: &str,
        query_is_ascii: bool,
        fragments: &[&str],
        name_lower: Option<&str>,
    ) -> Option<MatchScore>;
    fn describe(&self) -> String;
}

impl Candidate for Subscription {
    const KIND: &'static str = "subscription";

    fn tie_break(&self) -> (&str, bool) {
        (&self.name, self.is_default)
    }

    fn score(
        &self,
        query: &str,
        query_is_ascii: bool,
        fragments: &[&str],
        name_lower: Option<&str>,
    ) -> Option<MatchScore> {
        score(query, query_is_ascii, fragments, name_lower, self)
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

    fn score(
        &self,
        query: &str,
        query_is_ascii: bool,
        fragments: &[&str],
        name_lower: Option<&str>,
    ) -> Option<MatchScore> {
        score_tenant(query, query_is_ascii, fragments, name_lower, self)
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
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return empty_query_matches(items);
    }
    matches_non_empty(trimmed, items)
}

fn matches_non_empty<T: Candidate>(trimmed: &str, items: &[T]) -> Vec<(usize, MatchScore)> {
    let mut query_stack = [0u8; 256];
    let query_heap: String;
    let (query, query_is_ascii) = if trimmed.is_ascii() && trimmed.len() <= query_stack.len() {
        let bytes = trimmed.as_bytes();
        for (dst, &src) in query_stack[..bytes.len()].iter_mut().zip(bytes) {
            *dst = src.to_ascii_lowercase();
        }
        (
            std::str::from_utf8(&query_stack[..bytes.len()]).expect("ASCII bytes are valid UTF-8"),
            true,
        )
    } else {
        query_heap = trimmed.to_lowercase();
        let is_ascii = query_heap.is_ascii();
        (query_heap.as_str(), is_ascii)
    };

    let mut frag_buf = [""; 8];
    let mut frag_vec = Vec::new();
    let fragments = fill_name_fragments(query, &mut frag_buf, &mut frag_vec);

    let mut entries = Vec::new();
    let mut unicode_names: Vec<(usize, String)> = Vec::new();

    for (index, item) in items.iter().enumerate() {
        let (name, is_current) = item.tie_break();
        if name.is_ascii() {
            if let Some(score) = item.score(query, query_is_ascii, fragments, None) {
                entries.push((score, !is_current, name, index));
            }
        } else {
            let lower = name.to_lowercase();
            if let Some(score) = item.score(query, query_is_ascii, fragments, Some(&lower)) {
                entries.push((score, !is_current, name, index));
                unicode_names.push((index, lower));
            }
        }
    }

    sort_match_entries(&mut entries, &unicode_names);
    entries
        .into_iter()
        .map(|(score, _, _, index)| (index, score))
        .collect()
}

fn empty_query_matches<T: Candidate>(items: &[T]) -> Vec<(usize, MatchScore)> {
    let mut entries = Vec::with_capacity(items.len());
    let mut unicode_names: Vec<(usize, String)> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let (name, is_current) = item.tie_break();
        if !name.is_ascii() {
            unicode_names.push((index, name.to_lowercase()));
        }
        entries.push((MatchScore(u8::from(!is_current)), !is_current, name, index));
    }
    sort_match_entries(&mut entries, &unicode_names);
    entries
        .into_iter()
        .map(|(score, _, _, index)| (index, score))
        .collect()
}

fn sort_match_entries(
    entries: &mut [(MatchScore, bool, &str, usize)],
    unicode_names: &[(usize, String)],
) {
    if unicode_names.is_empty() {
        entries.sort_unstable_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| cmp_ascii_ignore_case(a.2.as_bytes(), b.2.as_bytes()))
                .then_with(|| a.3.cmp(&b.3))
        });
    } else {
        let lookup = |idx: usize| -> Option<&str> {
            unicode_names
                .binary_search_by_key(&idx, |&(i, _)| i)
                .ok()
                .map(|pos| unicode_names[pos].1.as_str())
        };
        entries.sort_unstable_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| cmp_case_insensitive(a.2, lookup(a.3), b.2, lookup(b.3)))
                .then_with(|| a.3.cmp(&b.3))
        });
    }
}
#[inline]
fn cmp_ascii_ignore_case(a: &[u8], b: &[u8]) -> Ordering {
    let common = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= common {
        let wa = u64::from_ne_bytes(a[i..i + 8].try_into().unwrap());
        let wb = u64::from_ne_bytes(b[i..i + 8].try_into().unwrap());
        if wa == wb {
            i += 8;
            continue;
        }
        break;
    }
    while i < common {
        let ca = a[i];
        let cb = b[i];
        if ca != cb {
            let la = ca.to_ascii_lowercase();
            let lb = cb.to_ascii_lowercase();
            if la != lb {
                return la.cmp(&lb);
            }
        }
        i += 1;
    }
    a.len().cmp(&b.len())
}

#[inline]
fn cmp_case_insensitive(
    a: &str,
    a_lower: Option<&str>,
    b: &str,
    b_lower: Option<&str>,
) -> Ordering {
    match (a_lower, b_lower) {
        (None, None) => cmp_ascii_ignore_case(a.as_bytes(), b.as_bytes()),
        (Some(al), None) => al
            .bytes()
            .cmp(b.bytes().map(|byte| byte.to_ascii_lowercase())),
        (None, Some(bl)) => a
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .cmp(bl.bytes()),
        (Some(al), Some(bl)) => al.cmp(bl),
    }
}

#[inline]
fn ascii_starts_with_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .get(..needle.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(needle.as_bytes()))
}

#[inline]
fn find_ascii_case_insensitive_byte(
    haystack: &[u8],
    mut i: usize,
    limit: usize,
    first_lower: u8,
    first_upper: u8,
) -> Option<usize> {
    if first_lower != first_upper {
        // `first_lower` is `'a'..='z'` (`0x61..=0x7A`); for ASCII `b`, `(b | 0x20) == first_lower`
        // holds iff `b == first_lower || b == first_upper`.
        let mask = u64::from_ne_bytes([first_lower; 8]);
        let case_bit = u64::from_ne_bytes([0x20; 8]);
        let ones = u64::from_ne_bytes([0x01; 8]);
        let highs = u64::from_ne_bytes([0x80; 8]);
        while i + 8 <= limit + 1 {
            let chunk = u64::from_ne_bytes(haystack[i..i + 8].try_into().unwrap());
            let diff = (chunk | case_bit) ^ mask;
            let zero_mask = diff.wrapping_sub(ones) & !diff & highs;
            if zero_mask != 0 {
                let offset = if cfg!(target_endian = "little") {
                    (zero_mask.trailing_zeros() / 8) as usize
                } else {
                    (zero_mask.leading_zeros() / 8) as usize
                };
                return Some(i + offset);
            }
            i += 8;
        }
        while i <= limit {
            if (haystack[i] | 0x20) == first_lower {
                return Some(i);
            }
            i += 1;
        }
    } else {
        let mask = u64::from_ne_bytes([first_lower; 8]);
        let ones = u64::from_ne_bytes([0x01; 8]);
        let highs = u64::from_ne_bytes([0x80; 8]);
        while i + 8 <= limit + 1 {
            let chunk = u64::from_ne_bytes(haystack[i..i + 8].try_into().unwrap());
            let diff = chunk ^ mask;
            let zero_mask = diff.wrapping_sub(ones) & !diff & highs;
            if zero_mask != 0 {
                let offset = if cfg!(target_endian = "little") {
                    (zero_mask.trailing_zeros() / 8) as usize
                } else {
                    (zero_mask.leading_zeros() / 8) as usize
                };
                return Some(i + offset);
            }
            i += 8;
        }
        while i <= limit {
            if haystack[i] == first_lower {
                return Some(i);
            }
            i += 1;
        }
    }
    None
}

#[inline]
fn ascii_contains_ignore_case(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    let Some(&first) = n.first() else {
        return true;
    };
    let len = n.len();
    if h.len() < len {
        return false;
    }
    let first_lower = first.to_ascii_lowercase();
    let first_upper = first.to_ascii_uppercase();
    let last_lower = n[len - 1].to_ascii_lowercase();
    let last_upper = n[len - 1].to_ascii_uppercase();
    let mid = if len > 2 { &n[1..len - 1] } else { &[][..] };
    let limit = h.len() - len;
    let mut i = 0;
    while let Some(pos) = find_ascii_case_insensitive_byte(h, i, limit, first_lower, first_upper) {
        if len == 1 {
            return true;
        }
        let tail = h[pos + len - 1];
        if (tail == last_lower || tail == last_upper)
            && (mid.is_empty() || h[pos + 1..pos + len - 1].eq_ignore_ascii_case(mid))
        {
            return true;
        }
        i = pos + 1;
    }
    false
}

#[inline]
fn ascii_name_rank(name: &str, query: &str) -> Option<MatchScore> {
    let h = name.as_bytes();
    let n = query.as_bytes();
    let len = n.len();
    if h.len() < len {
        return None;
    }
    if h[..len].eq_ignore_ascii_case(n) {
        return Some(if h.len() == len {
            MatchScore(0)
        } else {
            MatchScore(1)
        });
    }
    let first_lower = n[0];
    let first_upper = first_lower.to_ascii_uppercase();
    let last_lower = n[len - 1];
    let last_upper = last_lower.to_ascii_uppercase();
    let mid = if len > 2 { &n[1..len - 1] } else { &[][..] };
    let limit = h.len() - len;
    let mut substring_hit = false;
    let mut i = 1;
    while let Some(pos) = find_ascii_case_insensitive_byte(h, i, limit, first_lower, first_upper) {
        let matched = if len == 1 {
            true
        } else {
            let tail = h[pos + len - 1];
            (tail == last_lower || tail == last_upper)
                && (mid.is_empty() || h[pos + 1..pos + len - 1].eq_ignore_ascii_case(mid))
        };
        if matched {
            if !h[pos - 1].is_ascii_alphanumeric() && h[pos].is_ascii_alphanumeric() {
                return Some(MatchScore(2));
            }
            substring_hit = true;
        }
        i = pos + 1;
    }
    substring_hit.then_some(MatchScore(3))
}

enum Field<'a> {
    Ascii(&'a str),
    Lower(String),
}

impl<'a> Field<'a> {
    fn new(raw: &'a str, query_is_ascii: bool) -> Self {
        if query_is_ascii && raw.is_ascii() {
            Self::Ascii(raw)
        } else {
            Self::Lower(raw.to_lowercase())
        }
    }

    fn eq_query(&self, query: &str) -> bool {
        match self {
            Self::Ascii(s) => s.eq_ignore_ascii_case(query),
            Self::Lower(s) => s == query,
        }
    }

    fn starts_with_query(&self, query: &str) -> bool {
        match self {
            Self::Ascii(s) => ascii_starts_with_ignore_case(s, query),
            Self::Lower(s) => s.starts_with(query),
        }
    }

    fn contains_query(&self, query: &str) -> bool {
        match self {
            Self::Ascii(s) => ascii_contains_ignore_case(s, query),
            Self::Lower(s) => s.contains(query),
        }
    }
}

fn field_contains(raw: &str, query: &str, query_is_ascii: bool) -> bool {
    if query_is_ascii && raw.is_ascii() {
        ascii_contains_ignore_case(raw, query)
    } else {
        raw.to_lowercase().contains(query)
    }
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
    query_is_ascii: bool,
    fragments: &[&str],
    name_lower: Option<&str>,
    tenant: &Tenant,
) -> Option<MatchScore> {
    let name_eq = match name_lower {
        None if query_is_ascii => tenant.name.eq_ignore_ascii_case(query),
        None => false,
        Some(n) => n == query,
    };
    if name_eq {
        return Some(MatchScore(0));
    }

    let id = Field::new(&tenant.id, query_is_ascii);
    let domain = Field::new(&tenant.default_domain, query_is_ascii);
    if domain.eq_query(query) || id.eq_query(query) {
        return Some(MatchScore(0));
    }

    let name_starts = match name_lower {
        None if query_is_ascii => ascii_starts_with_ignore_case(&tenant.name, query),
        None => false,
        Some(n) => n.starts_with(query),
    };
    if name_starts || domain.starts_with_query(query) || id.starts_with_query(query) {
        return Some(MatchScore(1));
    }

    let name_contains = match name_lower {
        None if query_is_ascii => ascii_contains_ignore_case(&tenant.name, query),
        None => false,
        Some(n) => n.contains(query),
    };
    if name_contains || domain.contains_query(query) || id.contains_query(query) {
        Some(MatchScore(2))
    } else if !fragments.is_empty()
        && match name_lower {
            None if query_is_ascii => fragments
                .iter()
                .all(|fragment| ascii_contains_ignore_case(&tenant.name, fragment)),
            None => false,
            Some(n) => fragments.iter().all(|fragment| n.contains(*fragment)),
        }
    {
        Some(MatchScore(3))
    } else {
        None
    }
}

fn fill_name_fragments<'q, 'b>(
    query: &'q str,
    buf: &'b mut [&'q str; 8],
    overflow: &'b mut Vec<&'q str>,
) -> &'b [&'q str] {
    let mut iter = query.split_whitespace();
    let Some(first) = iter.next() else {
        return &[];
    };
    let Some(second) = iter.next() else {
        return &[];
    };
    buf[0] = first;
    buf[1] = second;
    let mut count = 2;
    for term in iter {
        if count < buf.len() {
            buf[count] = term;
            count += 1;
        } else {
            if overflow.is_empty() {
                overflow.extend_from_slice(buf);
            }
            overflow.push(term);
        }
    }
    if overflow.is_empty() {
        &buf[..count]
    } else {
        overflow.as_slice()
    }
}

#[cfg(test)]
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
    query_is_ascii: bool,
    fragments: &[&str],
    name_lower: Option<&str>,
    subscription: &Subscription,
) -> Option<MatchScore> {
    match name_lower {
        None => {
            if query_is_ascii {
                let name_rank = ascii_name_rank(&subscription.name, query);
                if name_rank == Some(MatchScore(0)) {
                    return name_rank;
                }
                let id_is_ascii = subscription.id.is_ascii();
                if id_is_ascii {
                    if subscription.id.len() == query.len()
                        && subscription.id.eq_ignore_ascii_case(query)
                    {
                        return Some(MatchScore(0));
                    }
                } else if subscription.id.to_lowercase() == query {
                    return Some(MatchScore(0));
                }
                if let Some(rank) = name_rank {
                    return Some(rank);
                }
                let id_contains = if id_is_ascii {
                    ascii_contains_ignore_case(&subscription.id, query)
                } else {
                    subscription.id.to_lowercase().contains(query)
                };
                if id_contains || field_contains(&subscription.tenant_id, query, true) {
                    Some(MatchScore(4))
                } else if !fragments.is_empty()
                    && fragments
                        .iter()
                        .all(|fragment| ascii_contains_ignore_case(&subscription.name, fragment))
                {
                    Some(MatchScore(5))
                } else {
                    None
                }
            } else if !subscription.id.is_ascii() {
                let id_lower = subscription.id.to_lowercase();
                if id_lower == query {
                    Some(MatchScore(0))
                } else if id_lower.contains(query)
                    || field_contains(&subscription.tenant_id, query, false)
                {
                    Some(MatchScore(4))
                } else {
                    None
                }
            } else if field_contains(&subscription.tenant_id, query, false) {
                Some(MatchScore(4))
            } else {
                None
            }
        }
        Some(name) => {
            if name == query {
                return Some(MatchScore(0));
            }
            let id = Field::new(&subscription.id, query_is_ascii);
            if id.eq_query(query) {
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
            } else if id.contains_query(query)
                || field_contains(&subscription.tenant_id, query, query_is_ascii)
            {
                Some(MatchScore(4))
            } else if !fragments.is_empty()
                && fragments.iter().all(|fragment| name.contains(*fragment))
            {
                Some(MatchScore(5))
            } else {
                None
            }
        }
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
            super::Candidate::score(&subscriptions[0], "zażółć", false, &[], Some("zażółć dev")),
            Some(super::MatchScore(1))
        );
    }
    #[test]
    fn ascii_case_insensitive_helpers_match_without_heap_allocations() {
        assert!(super::ascii_starts_with_ignore_case("HR Production", "hr"));
        assert!(super::ascii_starts_with_ignore_case("HR Production", ""));
        assert!(!super::ascii_starts_with_ignore_case("HR", "hr prod"));
        assert!(super::ascii_contains_ignore_case("North-HR_Prod", "hr_p"));
        assert!(super::ascii_contains_ignore_case("North", ""));
        assert!(!super::ascii_contains_ignore_case("HR", "hr_p"));
        assert_eq!(
            super::cmp_case_insensitive("HR_Prod", None, "hra", None),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            super::cmp_case_insensitive("hr-prod", None, "HR_Prod", None),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn matches_reference_lowercase_oracle_across_punctuation_and_mixed_unicode() {
        fn reference_ranked_subscriptions(
            query: &str,
            subscriptions: &[Subscription],
        ) -> Vec<usize> {
            let query = query.trim().to_lowercase();
            let fragments = super::name_fragments(&query);
            let mut out = subscriptions
                .iter()
                .enumerate()
                .filter_map(|(index, sub)| {
                    let name_lower = sub.name.to_lowercase();
                    let id = sub.id.to_lowercase();
                    let score = if query.is_empty() {
                        u8::from(!sub.is_default)
                    } else if name_lower == query || id == query {
                        0
                    } else if name_lower.starts_with(&query) {
                        1
                    } else if name_lower
                        .split(|c: char| !c.is_alphanumeric())
                        .any(|w| w.starts_with(&query))
                    {
                        2
                    } else if name_lower.contains(&query) {
                        3
                    } else if id.contains(&query) || sub.tenant_id.to_lowercase().contains(&query) {
                        4
                    } else if !fragments.is_empty()
                        && fragments.iter().all(|f| name_lower.contains(*f))
                    {
                        5
                    } else {
                        return None;
                    };
                    Some((score, !sub.is_default, name_lower, index))
                })
                .collect::<Vec<_>>();
            out.sort_unstable();
            out.into_iter().map(|(_, _, _, i)| i).collect()
        }

        fn reference_ranked_tenants(query: &str, tenants: &[Tenant]) -> Vec<usize> {
            let query = query.trim().to_lowercase();
            let fragments = super::name_fragments(&query);
            let mut out = tenants
                .iter()
                .enumerate()
                .filter_map(|(index, t)| {
                    let name = t.name.to_lowercase();
                    let id = t.id.to_lowercase();
                    let domain = t.default_domain.to_lowercase();
                    let score = if query.is_empty() {
                        u8::from(!t.is_current)
                    } else if name == query || domain == query || id == query {
                        0
                    } else if name.starts_with(&query)
                        || domain.starts_with(&query)
                        || id.starts_with(&query)
                    {
                        1
                    } else if name.contains(&query)
                        || domain.contains(&query)
                        || id.contains(&query)
                    {
                        2
                    } else if !fragments.is_empty() && fragments.iter().all(|f| name.contains(*f)) {
                        3
                    } else {
                        return None;
                    };
                    Some((score, !t.is_current, name, index))
                })
                .collect::<Vec<_>>();
            out.sort_unstable();
            out.into_iter().map(|(_, _, _, i)| i).collect()
        }

        let mut subscriptions = vec![
            subscription("HR_Prod", "ABCDEF01-0000-0000-0000-000000000001", false),
            subscription("hr-prod", "abcdef01-0000-0000-0000-000000000002", false),
            subscription("hra", "00000000-0000-0000-0000-000000000003", false),
            subscription("HR[Archive]", "00000000-0000-0000-0000-000000000004", false),
            subscription("Zażółć Dev", "00000000-0000-0000-0000-000000000005", false),
            subscription(
                "zażółć_alpha",
                "00000000-0000-0000-0000-000000000006",
                false,
            ),
            subscription("zebra", "00000000-0000-0000-0000-000000000007", false),
            subscription("Équipe Prod", "00000000-0000-0000-0000-000000000008", true),
            subscription(
                "İstanbul Core",
                "00000000-0000-0000-0000-000000000009",
                false,
            ),
            subscription(
                "Straße Payments",
                "00000000-0000-0000-0000-000000000010",
                false,
            ),
            subscription(
                "North-İstanbul-Prod",
                "00000000-0000-0000-0000-000000000011",
                false,
            ),
        ];
        subscriptions[4].tenant_id = "TENANT-ZAŻÓŁĆ-01".to_owned();
        subscriptions[0].tenant_id = "TENANT-SHARED-99".to_owned();

        let queries = [
            "",
            "   ",
            "hr",
            "HR_PROD",
            "archive",
            "z",
            "zażółć",
            "ZAŻÓŁĆ",
            "équipe",
            "İSTANBUL",
            "istanbul",
            "i\u{307}stanbul",
            "straße",
            "STRASSE",
            "prod north",
            "payments straße",
            "abcdef01",
            "tenant-shared",
            "tenant-zażółć",
            "a b c d e f g h i j",
            "9",
        ];

        for query in queries {
            assert_eq!(
                ranked_indices(query, &subscriptions),
                reference_ranked_subscriptions(query, &subscriptions),
                "subscription ranking mismatch for query {query:?}"
            );
        }

        let tenants = vec![
            tenant("HR_Prod", "AAAA-01", "HR_Prod.Contoso.com"),
            tenant("hr-prod", "aaaa-02", "hr-prod.contoso.com"),
            tenant("hra", "aaaa-03", "hra.contoso.com"),
            tenant("Zażółć Corp", "bbbb-01", "zazolc.example.com"),
            tenant("İstanbul Ops", "cccc-01", "istanbul.example.com"),
            tenant("zebra", "dddd-01", "zebra.example.com"),
        ];

        for query in queries {
            assert_eq!(
                ranked_tenant_indices(query, &tenants),
                reference_ranked_tenants(query, &tenants),
                "tenant ranking mismatch for query {query:?}"
            );
        }
    }

    #[test]
    #[ignore]
    fn bench_matcher_keystroke_latency_1000_subscriptions() {
        use std::{hint::black_box, time::Instant};

        let subscriptions = (0..1_000usize)
            .map(|i| {
                let env = ["dev", "staging", "prod", "sandbox"][i % 4];
                let team = [
                    "platform",
                    "identity",
                    "billing",
                    "network",
                    "security",
                    "data",
                    "analytics",
                    "compute",
                    "storage",
                    "payments",
                ][i % 10];
                Subscription {
                    id: format!("00000000-0000-0000-0000-{i:012}"),
                    name: format!("Contoso-{team}-{env}-{i:04}"),
                    tenant_id: format!("11111111-2222-3333-4444-{:012}", i % 25),
                    is_default: i == 42,
                }
            })
            .collect::<Vec<_>>();

        let cases = [
            ("empty", ""),
            ("prefix_pay", "pay"),
            ("uuid_substring", "00000000-0000-0000-0000-0000000009"),
            ("fragments_payments_prod", "payments prod"),
        ];

        let warmup_iters = 200usize;
        let bench_runs = 21usize;
        let ops_per_run = 200usize;

        let baseline_ranked = |query: &str, subscriptions: &[Subscription]| -> Vec<usize> {
            let query = query.trim().to_lowercase();
            let fragments = super::name_fragments(&query);
            let mut matches = subscriptions
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    let name_lower = item.name.to_lowercase();
                    let score = if query.is_empty() {
                        u8::from(!item.is_default)
                    } else if name_lower == query {
                        0
                    } else {
                        let id = item.id.to_lowercase();
                        if id == query {
                            0
                        } else if name_lower.starts_with(&query) {
                            1
                        } else if name_lower
                            .split(|c: char| !c.is_alphanumeric())
                            .any(|w| w.starts_with(&query))
                        {
                            2
                        } else if name_lower.contains(&query) {
                            3
                        } else if id.contains(&query)
                            || item.tenant_id.to_lowercase().contains(&query)
                        {
                            4
                        } else if !fragments.is_empty()
                            && fragments.iter().all(|f| name_lower.contains(*f))
                        {
                            5
                        } else {
                            return None;
                        }
                    };
                    Some((score, !item.is_default, name_lower, index))
                })
                .collect::<Vec<_>>();
            matches.sort_unstable();
            matches.into_iter().map(|(_, _, _, index)| index).collect()
        };

        let measure = |f: &dyn Fn(&str, &[Subscription]) -> Vec<usize>,
                       query: &str|
         -> (f64, f64) {
            for _ in 0..warmup_iters {
                black_box(f(black_box(query), black_box(&subscriptions)));
            }
            let mut samples_us = Vec::with_capacity(bench_runs);
            for _ in 0..bench_runs {
                let start = Instant::now();
                for _ in 0..ops_per_run {
                    black_box(f(black_box(query), black_box(&subscriptions)));
                }
                samples_us.push(start.elapsed().as_secs_f64() * 1_000_000.0 / ops_per_run as f64);
            }
            samples_us.sort_by(f64::total_cmp);
            let median_us = samples_us[bench_runs / 2];
            let mean_us = samples_us.iter().sum::<f64>() / bench_runs as f64;
            (median_us, mean_us)
        };

        for (label, query) in cases {
            assert_eq!(
                ranked_indices(query, &subscriptions),
                baseline_ranked(query, &subscriptions),
                "benchmark equivalence failed for {label}"
            );
            let (before_med, before_mean) = measure(&baseline_ranked, query);
            let (after_med, after_mean) = measure(&ranked_indices, query);
            let speedup = before_med / after_med;
            println!(
                "bench {label:<24}: before = {before_med:>7.2} us (mean {before_mean:>7.2} us) | \
                 after = {after_med:>7.2} us (mean {after_mean:>7.2} us) | speedup = {speedup:>5.2}x"
            );
        }
    }
}

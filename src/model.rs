use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tenant_id: String,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TenantDescription {
    pub tenant_id: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub display_name: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub default_domain: String,
}

fn null_as_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

impl TenantDescription {
    pub fn meaningful_name(&self) -> Option<&str> {
        let name = self.display_name.trim();
        let is_guid = name.len() == 36
            && name.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            });
        (!name.is_empty() && !is_guid && !name.eq_ignore_ascii_case(self.tenant_id.trim()))
            .then_some(name)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tenant {
    pub id: String,
    pub name: String,
    pub default_domain: String,
    pub subscription_index: usize,
    pub subscription_count: usize,
    pub is_current: bool,
}

impl Tenant {
    pub fn label(&self) -> String {
        match (self.name.is_empty(), self.default_domain.is_empty()) {
            (false, false) if !self.name.eq_ignore_ascii_case(&self.default_domain) => {
                format!("{} ({})", self.name, self.default_domain)
            }
            (false, _) => self.name.clone(),
            (true, false) => self.default_domain.clone(),
            (true, true) => self.id.clone(),
        }
    }
}

pub fn tenants_from_subscriptions(
    subscriptions: &[Subscription],
    descriptions: &[TenantDescription],
) -> Vec<Tenant> {
    if subscriptions.is_empty() {
        return Vec::new();
    }

    let mut description_index: HashMap<String, (Option<&str>, Option<&str>)> =
        HashMap::with_capacity(descriptions.len());
    for description in descriptions {
        let key = description.tenant_id.trim();
        if key.is_empty() {
            continue;
        }
        let entry = description_index
            .entry(key.to_ascii_lowercase())
            .or_default();
        if entry.0.is_none() {
            let domain = description.default_domain.trim();
            if !domain.is_empty() {
                entry.0 = Some(domain);
            }
        }
        if entry.1.is_none() {
            entry.1 = description.meaningful_name();
        }
    }

    let mut tenants: Vec<Tenant> = Vec::new();
    let mut tenant_indices: HashMap<String, usize> = HashMap::new();

    for (subscription_index, subscription) in subscriptions.iter().enumerate() {
        let tenant_id = subscription.tenant_id.trim();
        if tenant_id.is_empty() {
            continue;
        }

        let key = tenant_id.to_ascii_lowercase();
        if let Some(&existing_index) = tenant_indices.get(&key) {
            let tenant = &mut tenants[existing_index];
            tenant.subscription_count += 1;
            if subscription.is_default {
                tenant.is_current = true;
                tenant.subscription_index = subscription_index;
            }
        } else {
            let (domain, meaningful_name) =
                description_index.get(&key).copied().unwrap_or_default();
            let default_domain = domain.unwrap_or_default().to_owned();
            let name = meaningful_name.unwrap_or(&default_domain).to_owned();
            tenant_indices.insert(key, tenants.len());
            tenants.push(Tenant {
                id: tenant_id.to_owned(),
                name,
                default_domain,
                subscription_index,
                subscription_count: 1,
                is_current: subscription.is_default,
            });
        }
    }

    tenants
}

#[cfg(test)]
mod tenant_tests {
    use super::{Subscription, TenantDescription, tenants_from_subscriptions};

    #[test]
    fn groups_subscriptions_by_tenant_and_keeps_a_switch_target() {
        let subscriptions = vec![
            Subscription {
                id: "one".to_owned(),
                name: "One".to_owned(),
                tenant_id: "tenant-a".to_owned(),
                is_default: false,
            },
            Subscription {
                id: "two".to_owned(),
                name: "Two".to_owned(),
                tenant_id: "tenant-a".to_owned(),
                is_default: true,
            },
            Subscription {
                id: "three".to_owned(),
                name: "Three".to_owned(),
                tenant_id: "tenant-b".to_owned(),
                is_default: false,
            },
        ];
        let descriptions = vec![
            TenantDescription {
                tenant_id: "tenant-a".to_owned(),
                display_name: "Tenant A".to_owned(),
                default_domain: "tenant-a.example".to_owned(),
            },
            TenantDescription {
                tenant_id: "tenant-b".to_owned(),
                display_name: "Tenant B".to_owned(),
                default_domain: "tenant-b.example".to_owned(),
            },
        ];

        assert_eq!(
            tenants_from_subscriptions(&subscriptions, &descriptions),
            vec![
                super::Tenant {
                    id: "tenant-a".to_owned(),
                    name: "Tenant A".to_owned(),
                    default_domain: "tenant-a.example".to_owned(),
                    subscription_index: 1,
                    subscription_count: 2,
                    is_current: true,
                },
                super::Tenant {
                    id: "tenant-b".to_owned(),
                    name: "Tenant B".to_owned(),
                    default_domain: "tenant-b.example".to_owned(),
                    subscription_index: 2,
                    subscription_count: 1,
                    is_current: false,
                },
            ]
        );
    }

    #[test]
    fn uses_default_domain_when_display_name_is_missing() {
        let subscriptions = vec![Subscription {
            id: "one".to_owned(),
            name: "One".to_owned(),
            tenant_id: "tenant-a".to_owned(),
            is_default: false,
        }];
        let descriptions = vec![TenantDescription {
            tenant_id: "tenant-a".to_owned(),
            display_name: String::new(),
            default_domain: "tenant-a.example".to_owned(),
        }];

        assert_eq!(
            tenants_from_subscriptions(&subscriptions, &descriptions)[0].name,
            "tenant-a.example"
        );
    }

    #[test]
    fn skips_whitespace_tenant_ids_and_trims_stored_tenant_id() {
        let subscriptions = vec![
            Subscription {
                id: "blank".to_owned(),
                name: "Blank Tenant".to_owned(),
                tenant_id: "   \t\n ".to_owned(),
                is_default: false,
            },
            Subscription {
                id: "first".to_owned(),
                name: "First".to_owned(),
                tenant_id: " Tenant-A ".to_owned(),
                is_default: false,
            },
            Subscription {
                id: "second".to_owned(),
                name: "Second".to_owned(),
                tenant_id: "tenant-a".to_owned(),
                is_default: true,
            },
        ];

        assert_eq!(
            tenants_from_subscriptions(&subscriptions, &[]),
            vec![super::Tenant {
                id: "Tenant-A".to_owned(),
                name: String::new(),
                default_domain: String::new(),
                subscription_index: 2,
                subscription_count: 2,
                is_current: true,
            }]
        );
    }

    #[test]
    fn merges_partial_descriptions_case_insensitively_and_tracks_last_default_subscription() {
        let subscriptions = vec![
            Subscription {
                id: "blank".to_owned(),
                name: "Blank".to_owned(),
                tenant_id: "   \n\t ".to_owned(),
                is_default: true,
            },
            Subscription {
                id: "t1-s1".to_owned(),
                name: "T1 First Default".to_owned(),
                tenant_id: " Tenant-Alpha ".to_owned(),
                is_default: true,
            },
            Subscription {
                id: "t2-s1".to_owned(),
                name: "T2 First".to_owned(),
                tenant_id: "Tenant-Beta".to_owned(),
                is_default: false,
            },
            Subscription {
                id: "t1-s2".to_owned(),
                name: "T1 Second Default".to_owned(),
                tenant_id: "tenant-alpha".to_owned(),
                is_default: true,
            },
            Subscription {
                id: "t1-s3".to_owned(),
                name: "T1 Third Non-Default".to_owned(),
                tenant_id: "TENANT-ALPHA".to_owned(),
                is_default: false,
            },
            Subscription {
                id: "t3-s1".to_owned(),
                name: "T3 Unmatched".to_owned(),
                tenant_id: "Tenant-Gamma".to_owned(),
                is_default: false,
            },
        ];
        let descriptions = vec![
            // Tenant-Alpha record 1: default_domain only, GUID display_name ignored
            TenantDescription {
                tenant_id: "tenant-alpha".to_owned(),
                display_name: "00000000-1111-2222-3333-444444444444".to_owned(),
                default_domain: "  alpha.example.com  ".to_owned(),
            },
            // Tenant-Alpha record 2: display_name matching tenant_id ignored, second domain ignored
            TenantDescription {
                tenant_id: " TENANT-ALPHA ".to_owned(),
                display_name: "Tenant-Alpha".to_owned(),
                default_domain: "alpha-second.example.com".to_owned(),
            },
            // Tenant-Alpha record 3: meaningful_name picked up, blank domain ignored
            TenantDescription {
                tenant_id: "Tenant-Alpha".to_owned(),
                display_name: "  Alpha Corp  ".to_owned(),
                default_domain: "   ".to_owned(),
            },
            // Tenant-Alpha record 4: later meaningful_name ignored
            TenantDescription {
                tenant_id: "tenant-alpha".to_owned(),
                display_name: "Alpha Later".to_owned(),
                default_domain: "alpha-later.example.com".to_owned(),
            },
            // Tenant-Beta record 1: meaningful_name only, blank domain
            TenantDescription {
                tenant_id: "tenant-beta".to_owned(),
                display_name: "Beta Ltd".to_owned(),
                default_domain: "  ".to_owned(),
            },
            // Tenant-Beta record 2: default_domain picked up, blank display_name
            TenantDescription {
                tenant_id: "TENANT-BETA".to_owned(),
                display_name: String::new(),
                default_domain: " beta.example.com ".to_owned(),
            },
            // Blank tenant_id in descriptions is ignored
            TenantDescription {
                tenant_id: "   ".to_owned(),
                display_name: "Ignored".to_owned(),
                default_domain: "ignored.example.com".to_owned(),
            },
        ];

        assert_eq!(
            tenants_from_subscriptions(&subscriptions, &descriptions),
            vec![
                super::Tenant {
                    id: "Tenant-Alpha".to_owned(),
                    name: "Alpha Corp".to_owned(),
                    default_domain: "alpha.example.com".to_owned(),
                    subscription_index: 3,
                    subscription_count: 3,
                    is_current: true,
                },
                super::Tenant {
                    id: "Tenant-Beta".to_owned(),
                    name: "Beta Ltd".to_owned(),
                    default_domain: "beta.example.com".to_owned(),
                    subscription_index: 2,
                    subscription_count: 1,
                    is_current: false,
                },
                super::Tenant {
                    id: "Tenant-Gamma".to_owned(),
                    name: String::new(),
                    default_domain: String::new(),
                    subscription_index: 5,
                    subscription_count: 1,
                    is_current: false,
                },
            ]
        );
    }

    fn legacy_tenants_from_subscriptions(
        subscriptions: &[Subscription],
        descriptions: &[TenantDescription],
    ) -> Vec<super::Tenant> {
        let mut tenants: Vec<super::Tenant> = Vec::new();

        for (subscription_index, subscription) in subscriptions.iter().enumerate() {
            let tenant_id = subscription.tenant_id.trim();
            if tenant_id.is_empty() {
                continue;
            }

            if let Some(tenant) = tenants
                .iter_mut()
                .find(|tenant| tenant.id.eq_ignore_ascii_case(tenant_id))
            {
                tenant.subscription_count += 1;
                if subscription.is_default {
                    tenant.is_current = true;
                    tenant.subscription_index = subscription_index;
                }
            } else {
                let matching = || {
                    descriptions.iter().filter(|description| {
                        description.tenant_id.trim().eq_ignore_ascii_case(tenant_id)
                    })
                };
                let default_domain = matching()
                    .map(|description| description.default_domain.trim())
                    .find(|domain| !domain.is_empty())
                    .unwrap_or_default()
                    .to_owned();
                let name = matching()
                    .find_map(TenantDescription::meaningful_name)
                    .unwrap_or(&default_domain)
                    .to_owned();
                tenants.push(super::Tenant {
                    id: tenant_id.to_owned(),
                    name,
                    default_domain,
                    subscription_index,
                    subscription_count: 1,
                    is_current: subscription.is_default,
                });
            }
        }

        tenants
    }

    #[test]
    fn matches_legacy_linear_scan_across_edge_cases_and_enterprise_scale() {
        let (subscriptions, descriptions) = build_enterprise_fixture();
        assert_eq!(
            tenants_from_subscriptions(&subscriptions, &descriptions),
            legacy_tenants_from_subscriptions(&subscriptions, &descriptions)
        );
        assert_eq!(
            tenants_from_subscriptions(&subscriptions, &[]),
            legacy_tenants_from_subscriptions(&subscriptions, &[])
        );
        assert_eq!(
            tenants_from_subscriptions(&[], &descriptions),
            legacy_tenants_from_subscriptions(&[], &descriptions)
        );
    }

    fn build_enterprise_fixture() -> (Vec<Subscription>, Vec<TenantDescription>) {
        let tenant_count = 400usize;
        let subscription_count = 5_000usize;
        let mut subscriptions = Vec::with_capacity(subscription_count);
        for index in 0..subscription_count {
            let tenant_num = index % tenant_count;
            let tenant_id = if index % 2 == 0 {
                format!("10000000-0000-0000-0000-{tenant_num:012x}")
            } else {
                format!("10000000-0000-0000-0000-{tenant_num:012X}")
            };
            subscriptions.push(Subscription {
                id: format!("00000000-0000-0000-0000-{index:012x}"),
                name: format!("Enterprise Subscription {index}"),
                tenant_id,
                is_default: index == 3_456,
            });
        }

        let mut descriptions = Vec::with_capacity(1_000);
        for index in 0..1_000usize {
            let tenant_num = index % tenant_count;
            let tenant_id = format!("10000000-0000-0000-0000-{tenant_num:012x}");
            let (display_name, default_domain) = match index / tenant_count {
                0 => (String::new(), format!("tenant-{tenant_num}.example.com")),
                1 => (format!("Enterprise Tenant {tenant_num}"), String::new()),
                _ => (
                    format!("Duplicate Tenant {tenant_num}"),
                    format!("dup-{tenant_num}.example.com"),
                ),
            };
            descriptions.push(TenantDescription {
                tenant_id,
                display_name,
                default_domain,
            });
        }

        (subscriptions, descriptions)
    }

    #[test]
    #[ignore]
    fn bench_tenants_from_subscriptions_enterprise_scale() {
        use std::hint::black_box;
        use std::time::Instant;

        let (subscriptions, descriptions) = build_enterprise_fixture();

        for _ in 0..10 {
            black_box(legacy_tenants_from_subscriptions(
                black_box(&subscriptions),
                black_box(&descriptions),
            ));
            black_box(tenants_from_subscriptions(
                black_box(&subscriptions),
                black_box(&descriptions),
            ));
        }

        let iterations = 51usize;
        let mut legacy_samples_ns = Vec::with_capacity(iterations);
        let mut optimized_samples_ns = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let start = Instant::now();
            let legacy = legacy_tenants_from_subscriptions(
                black_box(&subscriptions),
                black_box(&descriptions),
            );
            legacy_samples_ns.push(start.elapsed().as_nanos() as u64);

            let start = Instant::now();
            let optimized =
                tenants_from_subscriptions(black_box(&subscriptions), black_box(&descriptions));
            optimized_samples_ns.push(start.elapsed().as_nanos() as u64);

            assert_eq!(black_box(optimized), black_box(legacy));
        }
        legacy_samples_ns.sort_unstable();
        optimized_samples_ns.sort_unstable();

        let legacy_median_us = legacy_samples_ns[iterations / 2] as f64 / 1_000.0;
        let legacy_mean_us =
            legacy_samples_ns.iter().sum::<u64>() as f64 / iterations as f64 / 1_000.0;
        let opt_median_us = optimized_samples_ns[iterations / 2] as f64 / 1_000.0;
        let opt_mean_us =
            optimized_samples_ns.iter().sum::<u64>() as f64 / iterations as f64 / 1_000.0;
        let speedup = legacy_median_us / opt_median_us;

        println!(
            "bench_tenants_from_subscriptions_enterprise_scale (S=5000, T=400, D=1000, N={iterations}):\n  before (linear): median={legacy_median_us:.2} us ({:.3} ms), mean={legacy_mean_us:.2} us ({:.3} ms)\n  after (hashmap): median={opt_median_us:.2} us ({:.3} ms), mean={opt_mean_us:.2} us ({:.3} ms)\n  speedup: {speedup:.1}x",
            legacy_median_us / 1_000.0,
            legacy_mean_us / 1_000.0,
            opt_median_us / 1_000.0,
            opt_mean_us / 1_000.0,
        );
        assert!(
            speedup > 10.0,
            "expected >10x speedup over linear scan, got {speedup:.2}x (before={legacy_median_us:.2} us, after={opt_median_us:.2} us)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Subscription, TenantDescription};

    #[test]
    fn deserializes_azure_account_shape() {
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "HR Production",
            "tenantId": "10000000-0000-0000-0000-000000000001",
            "isDefault": true,
            "state": "Enabled"
        }"#;

        let subscription: Subscription = serde_json::from_str(json).unwrap();

        assert_eq!(subscription.name, "HR Production");
        assert!(subscription.is_default);
        assert_eq!(
            subscription.tenant_id,
            "10000000-0000-0000-0000-000000000001"
        );
    }

    #[test]
    fn deserializes_tenant_display_metadata() {
        let description: TenantDescription = serde_json::from_str(
            r#"{"tenantId":"10000000-0000-0000-0000-000000000001","displayName":"Tenant A","defaultDomain":"tenant-a.example"}"#,
        )
        .unwrap();

        assert_eq!(description.display_name, "Tenant A");
        assert_eq!(description.default_domain, "tenant-a.example");
    }

    #[test]
    fn tolerates_null_tenant_metadata_fields() {
        let description: TenantDescription = serde_json::from_str(
            r#"{"tenantId":"tenant-a","displayName":null,"defaultDomain":null}"#,
        )
        .unwrap();
        let subscriptions: Vec<Subscription> =
            serde_json::from_str(r#"[{"id":"a","name":"Alpha","tenantId":"tenant-a"}]"#).unwrap();
        let tenants = super::tenants_from_subscriptions(&subscriptions, &[description]);
        assert_eq!(tenants[0].label(), "tenant-a");
    }

    #[test]
    fn tolerates_optional_azure_fields() {
        let subscription: Subscription =
            serde_json::from_str(r#"{"id":"1","name":"Minimal"}"#).unwrap();

        assert_eq!(subscription.tenant_id, "");
        assert!(!subscription.is_default);
    }
}

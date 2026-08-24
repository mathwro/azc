use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tenant_id: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub state: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TenantDescription {
    pub tenant_id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub default_domain: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tenant {
    pub id: String,
    pub name: String,
    pub subscription_index: usize,
    pub subscription_count: usize,
    pub is_current: bool,
}

pub fn tenants_from_subscriptions(
    subscriptions: &[Subscription],
    descriptions: &[TenantDescription],
) -> Vec<Tenant> {
    let mut tenants: Vec<Tenant> = Vec::new();

    for (subscription_index, subscription) in subscriptions.iter().enumerate() {
        if subscription.tenant_id.is_empty() {
            continue;
        }

        if let Some(tenant) = tenants.iter_mut().find(|tenant| {
            tenant
                .id
                .trim()
                .eq_ignore_ascii_case(subscription.tenant_id.trim())
        }) {
            tenant.subscription_count += 1;
            if subscription.is_default {
                tenant.is_current = true;
                tenant.subscription_index = subscription_index;
            }
        } else {
            let name = descriptions
                .iter()
                .find(|description| {
                    description
                        .tenant_id
                        .trim()
                        .eq_ignore_ascii_case(subscription.tenant_id.trim())
                })
                .map_or_else(String::new, |description| {
                    if description.display_name.is_empty() {
                        description.default_domain.clone()
                    } else {
                        description.display_name.clone()
                    }
                });
            tenants.push(Tenant {
                id: subscription.tenant_id.clone(),
                name,
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
                state: "Enabled".to_owned(),
            },
            Subscription {
                id: "two".to_owned(),
                name: "Two".to_owned(),
                tenant_id: "tenant-a".to_owned(),
                is_default: true,
                state: "Enabled".to_owned(),
            },
            Subscription {
                id: "three".to_owned(),
                name: "Three".to_owned(),
                tenant_id: "tenant-b".to_owned(),
                is_default: false,
                state: "Enabled".to_owned(),
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
                    subscription_index: 1,
                    subscription_count: 2,
                    is_current: true,
                },
                super::Tenant {
                    id: "tenant-b".to_owned(),
                    name: "Tenant B".to_owned(),
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
            state: "Enabled".to_owned(),
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
    fn tolerates_optional_azure_fields() {
        let subscription: Subscription =
            serde_json::from_str(r#"{"id":"1","name":"Minimal"}"#).unwrap();

        assert_eq!(subscription.tenant_id, "");
        assert!(!subscription.is_default);
        assert_eq!(subscription.state, "");
    }
}

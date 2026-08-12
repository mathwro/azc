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

#[cfg(test)]
mod tests {
    use super::Subscription;

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
    fn tolerates_optional_azure_fields() {
        let subscription: Subscription =
            serde_json::from_str(r#"{"id":"1","name":"Minimal"}"#).unwrap();

        assert_eq!(subscription.tenant_id, "");
        assert!(!subscription.is_default);
        assert_eq!(subscription.state, "");
    }
}

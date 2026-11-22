use serde::Serialize;
use waltr::{
    destination,
    grpc::waltr::auth::v1::{AuthStatus, AuthStatusKind, auth_target::Target},
};

use crate::{ClientError, WaltrTransport};

/// A Remote in the entry server's directory, without endpoint or credential data.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DestinationEntry {
    name: String,
    route_id: Option<String>,
}

/// Read the directory using an entry-local transport.
///
/// # Errors
/// Returns the transport error if the entry server cannot list authorization status.
pub async fn entry_destinations(
    entry: &impl WaltrTransport,
) -> Result<Vec<DestinationEntry>, ClientError> {
    Ok(project(entry.list_auth_statuses().await?))
}

fn project(statuses: Vec<AuthStatus>) -> Vec<DestinationEntry> {
    let mut remotes: Vec<_> = statuses
        .into_iter()
        .filter_map(|status| {
            let Some(Target::RemoteKey(name)) = status.target?.target else {
                return None;
            };
            if !destination::is_remote_name(&name) {
                return None;
            }
            let routable = status.kind == AuthStatusKind::OauthConfigured as i32
                || status.kind == AuthStatusKind::Expired as i32 && status.can_refresh;
            let route_id = routable
                .then(|| status.metadata.get(destination::ROUTE_ID))
                .flatten()
                .filter(|id| destination::is_route_id(id))
                .cloned();
            Some(DestinationEntry { name, route_id })
        })
        .collect();
    remotes.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    remotes
}

#[cfg(test)]
mod tests {
    use super::*;
    use waltr::grpc::waltr::auth::v1::AuthTarget;

    #[test]
    fn directory_exposes_only_remote_names_and_usable_grant_ids() {
        let id = "a".repeat(32);
        let status = |target, kind: AuthStatusKind, can_refresh, pin: &str| AuthStatus {
            target: Some(AuthTarget {
                target: Some(target),
            }),
            kind: kind as i32,
            can_refresh,
            message: "private status message".into(),
            metadata: [
                (destination::ROUTE_ID.into(), pin.to_owned()),
                ("private".into(), "private metadata".into()),
            ]
            .into(),
            ..Default::default()
        };
        let directory = project(vec![
            status(
                Target::RemoteKey("remote".into()),
                AuthStatusKind::Expired,
                true,
                &id,
            ),
            status(
                Target::RemoteKey("local".into()),
                AuthStatusKind::OauthConfigured,
                true,
                &id,
            ),
            status(
                Target::ProviderKey("provider".into()),
                AuthStatusKind::OauthConfigured,
                true,
                &id,
            ),
            status(
                Target::RemoteKey("inactive".into()),
                AuthStatusKind::AuthorizationRequired,
                false,
                &id,
            ),
            status(
                Target::RemoteKey("bad-pin".into()),
                AuthStatusKind::OauthConfigured,
                true,
                "unsafe",
            ),
        ]);
        assert_eq!(
            serde_json::to_value(directory).unwrap(),
            serde_json::json!([
                { "name": "bad-pin", "routeId": null },
                { "name": "inactive", "routeId": null },
                { "name": "remote", "routeId": id },
            ])
        );
    }
}

//! App-private, nonsecret server picker registry.
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Mutex};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerProfile {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerList {
    pub servers: Vec<ServerProfile>,
    pub last_connected_id: Option<String>,
}
#[derive(Debug)]
pub(crate) struct ServerProfiles {
    path: PathBuf,
    state: Mutex<ServerList>,
}
pub(crate) fn normalize_endpoint(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.len() > 8192 || input.chars().any(char::is_control) {
        return Err("Invalid server URL".into());
    }
    let authority = input
        .split_once("://")
        .map(|(_, tail)| tail.split(['/', '?', '#']).next().unwrap_or_default())
        .ok_or("Invalid server URL")?;
    if authority.contains('@') {
        return Err("Server URL must not include credentials".into());
    }
    let mut url = url::Url::parse(input).map_err(|_| "Invalid server URL")?;
    let raw_host = if authority.starts_with('[') {
        authority
            .split_once(']')
            .map(|(host, _)| format!("{host}]"))
            .unwrap_or_default()
    } else {
        authority
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    let loopback = matches!(raw_host.as_str(), "localhost" | "127.0.0.1" | "[::1]");
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err("Use HTTPS (HTTP is allowed only for exact loopback hosts), without credentials, query, or fragment".into());
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url.to_string().trim_end_matches('/').to_owned())
}
impl ServerProfiles {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        let mut state: ServerList = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|_| "Server registry is invalid")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ServerList::default(),
            Err(_) => return Err("Server registry is unavailable".into()),
        };
        let mut seen = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for profile in &state.servers {
            let endpoint =
                normalize_endpoint(profile.url.as_deref().ok_or("Invalid saved server")?)?;
            if profile.id.len() != 32
                || !profile.id.bytes().all(|byte| byte.is_ascii_hexdigit())
                || !ids.insert(&profile.id)
                || profile.name.trim().is_empty()
                || profile.name.len() > 256
                || profile.name.chars().any(char::is_control)
                || profile.url.as_deref() != Some(&endpoint)
                || !seen.insert(endpoint)
            {
                return Err("Server registry is invalid".into());
            }
        }
        if state
            .last_connected_id
            .as_deref()
            .is_some_and(|id| id != "local" && !state.servers.iter().any(|s| s.id == id))
        {
            state.last_connected_id = None;
        }
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }
    pub fn list(&self) -> ServerList {
        let mut result = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        result.servers.insert(
            0,
            ServerProfile {
                id: "local".into(),
                name: "Local".into(),
                url: None,
            },
        );
        result
    }
    pub fn get(&self, id: &str) -> Result<ServerProfile, String> {
        self.list()
            .servers
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| "Server not found".into())
    }
    fn update<T>(
        &self,
        change: impl FnOnce(&mut ServerList) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = state.clone();
        let result = change(&mut next)?;
        let bytes = serde_json::to_vec(&next).map_err(|_| "Could not save servers")?;
        std::fs::create_dir_all(self.path.parent().ok_or("Server registry is unavailable")?)
            .map_err(|_| "Could not save servers")?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)
            .and_then(|()| std::fs::rename(&temporary, &self.path))
            .map_err(|_| "Could not save servers")?;
        *state = next;
        Ok(result)
    }
    pub fn save(&self, url: &str, name: Option<String>) -> Result<ServerProfile, String> {
        let url = normalize_endpoint(url)?;
        let name = name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| url.clone());
        if name.len() > 256 || name.chars().any(char::is_control) {
            return Err("Invalid server name".into());
        }
        self.update(|state| {
            if let Some(existing) = state
                .servers
                .iter_mut()
                .find(|s| s.url.as_ref() == Some(&url))
            {
                existing.name = name;
                return Ok(existing.clone());
            }
            let mut bytes = [0u8; 16];
            rand::TryRngCore::try_fill_bytes(&mut rand::rng(), &mut bytes)
                .map_err(|_| "Could not create server ID")?;
            let id = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            let profile = ServerProfile {
                id,
                name,
                url: Some(url),
            };
            state.servers.push(profile.clone());
            Ok(profile)
        })
    }
    pub fn remove(&self, id: &str) -> Result<(), String> {
        if id == "local" {
            return Err("Local cannot be removed".into());
        }
        self.update(|state| {
            state.servers.retain(|s| s.id != id);
            if state.last_connected_id.as_deref() == Some(id) {
                state.last_connected_id = None;
            }
            Ok(())
        })
    }
    pub fn connected(&self, id: &str) -> Result<(), String> {
        self.update(|state| {
            if id == "local" || state.servers.iter().any(|s| s.id == id) {
                state.last_connected_id = Some(id.into());
            }
            Ok(())
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(
                std::env::temp_dir().join(format!(
                    "waltr-profiles-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )),
            )
        }
        fn registry(&self) -> ServerProfiles {
            ServerProfiles::open(self.0.join("servers.json")).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }
    #[test]
    fn profiles_round_trip_dedupe_and_only_remember_success() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        assert_eq!(registry.list().servers[0].id, "local");
        let first = registry.save("HTTPS://EXAMPLE.COM:443/api/", None).unwrap();
        assert_eq!(registry.list().last_connected_id, None);
        let same = registry
            .save("https://example.com/api", Some("Office".into()))
            .unwrap();
        assert_eq!(same.id, first.id);
        assert_eq!(same.name, "Office");
        registry.connected(&first.id).unwrap();
        let reopened = fixture.registry();
        assert_eq!(reopened.list().servers.len(), 2);
        assert_eq!(
            reopened.list().last_connected_id.as_deref(),
            Some(first.id.as_str())
        );
        assert!(reopened.remove("local").is_err());
        reopened.remove(&first.id).unwrap();
        assert_eq!(reopened.list().last_connected_id, None);
        reopened.connected(&first.id).unwrap();
        assert_eq!(fixture.registry().list().last_connected_id, None);
    }
    #[test]
    fn failed_persistence_does_not_mutate_memory() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        std::fs::create_dir_all(&fixture.0).unwrap();
        std::fs::write(fixture.0.join("servers.json.tmp"), []).unwrap();
        std::fs::create_dir(fixture.0.join("servers.json")).unwrap();
        assert!(registry.save("https://example.com", None).is_err());
        assert_eq!(registry.list().servers.len(), 1);
    }
    #[test]
    fn duplicate_saved_ids_fail_closed() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let first = registry.save("https://one.example", None).unwrap();
        let mut second = first.clone();
        second.url = Some("https://two.example".into());
        std::fs::write(
            &registry.path,
            serde_json::to_vec(&ServerList {
                servers: vec![first, second],
                last_connected_id: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(ServerProfiles::open(registry.path.clone()).is_err());
    }
    #[test]
    fn endpoint_normalization_is_strict() {
        assert_eq!(
            normalize_endpoint("HTTPS://EXAMPLE.COM:443/api/").unwrap(),
            "https://example.com/api"
        );
        for url in [
            "http://example.com",
            "http://127.0.0.2",
            "https://a@host",
            "https://host?q",
            "https://host#f",
            "http://localhost.evil",
            "https://@host",
            "http://2130706433",
            "http://127.1",
            "http://0x7f000001",
            "https://host:0",
        ] {
            assert!(normalize_endpoint(url).is_err(), "{url}");
        }
        for url in [
            "http://localhost:8080",
            "http://127.0.0.1",
            "http://[::1]:80",
        ] {
            assert!(normalize_endpoint(url).is_ok());
        }
    }
}

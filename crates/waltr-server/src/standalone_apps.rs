//! Explicit opt-in policy for the standalone server's owned Apps listener.

use waltr_server::mcp_apps_host::AppsContainerConfig;
use std::io;

const VARIABLES: [&str; 3] = [
    "WALTR_APP_CONTAINER_ADDR",
    "WALTR_APP_CONTAINER_ORIGIN",
    "WALTR_APP_CONTAINER_HOST_ORIGINS",
];

pub fn from_env() -> io::Result<Option<AppsContainerConfig>> {
    let mut values = [None, None, None];
    for (name, value) in VARIABLES.into_iter().zip(&mut values) {
        *value = match std::env::var(name) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => return Err(io::Error::other(format!("{name} must be UTF-8"))),
        };
    }
    parse(values, cfg!(feature = "web"))
}

fn parse(values: [Option<String>; 3], web: bool) -> io::Result<Option<AppsContainerConfig>> {
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    if !web {
        return Err(io::Error::other(
            "standalone MCP Apps requires the web feature",
        ));
    }
    let [Some(bind), Some(public_origin), Some(hosts)] = values else {
        return Err(io::Error::other(
            "standalone MCP Apps requires WALTR_APP_CONTAINER_ADDR, WALTR_APP_CONTAINER_ORIGIN, and WALTR_APP_CONTAINER_HOST_ORIGINS together",
        ));
    };
    let bind = bind.parse().map_err(io::Error::other)?;
    exact_origin(&public_origin)?;
    let allowed_host_origins: Vec<String> =
        hosts.split(',').map(str::trim).map(str::to_owned).collect();
    if !(1..=20).contains(&allowed_host_origins.len()) {
        return Err(io::Error::other("Apps requires 1..20 exact parent origins"));
    }
    for host in &allowed_host_origins {
        exact_origin(host)?;
        if host == &public_origin {
            return Err(io::Error::other(
                "Apps container must have a separate origin",
            ));
        }
    }
    Ok(Some(AppsContainerConfig {
        bind,
        public_origin: Some(public_origin),
        allowed_host_origins,
    }))
}

fn exact_origin(value: &str) -> io::Result<()> {
    let url = url::Url::parse(value).map_err(io::Error::other)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.origin().ascii_serialization() != value
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || value.contains('*')
    {
        return Err(io::Error::other(
            "standalone Apps origins must be exact HTTP(S) origins",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "web")]
    #[tokio::test]
    async fn occupied_apps_port_rolls_back_and_joins_the_application() {
        use std::sync::atomic::Ordering;

        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut values = configured();
        values[0] = Some(occupied.local_addr().unwrap().to_string());
        let config = parse(values, true).unwrap().unwrap();
        let (app, probe) = waltr_server::test_support::blocked_background_application();
        probe.cleanup_started.notified().await;

        let startup =
            tokio::spawn(app.bind_with_apps_config("127.0.0.1:0".parse().unwrap(), config));
        probe.shutdown_observed.notified().await;
        assert!(probe.cleanup_dropped.load(Ordering::SeqCst));
        assert!(!startup.is_finished());
        probe.release_cleanup.notify_one();
        assert!(startup.await.unwrap().is_err());
    }

    fn configured() -> [Option<String>; 3] {
        [
            "0.0.0.0:14334",
            "https://apps.example.test",
            "https://waltr.example.test",
        ]
        .map(|value| Some(value.into()))
    }

    #[test]
    fn ordinary_startup_remains_disabled_in_every_build() {
        for web in [false, true] {
            assert!(parse([None, None, None], web).unwrap().is_none());
        }
    }

    #[test]
    fn every_partial_configuration_fails_closed() {
        for mask in 1..7 {
            let mut values = configured();
            for (index, value) in values.iter_mut().enumerate() {
                if mask & (1 << index) == 0 {
                    *value = None;
                }
            }
            assert!(parse(values, true).is_err());
        }
    }

    #[test]
    fn unsupported_build_rejects_explicit_apps_configuration() {
        assert!(parse(configured(), false).is_err());
    }

    #[test]
    fn complete_configuration_retains_exact_listener_and_parent_policy() {
        let config = parse(configured(), true).unwrap().unwrap();
        assert_eq!(config.bind.to_string(), "0.0.0.0:14334");
        assert_eq!(
            config.public_origin.as_deref(),
            Some("https://apps.example.test")
        );
        assert_eq!(config.allowed_host_origins, ["https://waltr.example.test"]);
    }

    #[test]
    fn invalid_or_same_origin_policy_fails_closed() {
        for (index, value) in [
            (0, "not-an-address"),
            (0, ""),
            (1, ""),
            (1, "https://apps.example.test/"),
            (1, "https://apps.example.test/path"),
            (1, "https://user:pass@apps.example.test"),
            (1, "https://apps.example.test?query"),
            (2, ""),
            (2, "*"),
            (2, "https://*.example.test"),
            (2, "https://waltr.example.test,"),
            (2, "https://apps.example.test"),
            (2, "tauri://localhost"),
        ] {
            let mut values = configured();
            values[index] = Some(value.into());
            assert!(parse(values, true).is_err(), "accepted {value}");
        }
        let mut values = configured();
        values[2] = Some(vec!["https://waltr.example.test"; 21].join(","));
        assert!(parse(values, true).is_err());
    }
}

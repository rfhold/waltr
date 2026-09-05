//! Shared server outbound HTTP client configuration.

#[cfg(target_os = "android")]
use std::sync::Arc;

pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();

    #[cfg(target_os = "android")]
    {
        // Avoid rustls-platform-verifier 0.7.0's incorrect Android revocation fallback.
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports the safe default TLS protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();

        return builder.tls_backend_preconfigured(config);
    }

    #[cfg(not(target_os = "android"))]
    builder
}

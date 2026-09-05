//! Shared outbound HTTP client construction.

pub(crate) fn client_builder() -> Result<reqwest::ClientBuilder, rustls::Error> {
    let builder = reqwest::Client::builder();

    #[cfg(target_os = "android")]
    {
        // Avoid rustls-platform-verifier 0.7.0's incorrect Android revocation fallback.
        return Ok(builder.tls_backend_preconfigured(webpki_tls_config()?));
    }

    #[cfg(not(target_os = "android"))]
    Ok(builder)
}

pub(crate) fn client() -> reqwest::Client {
    client_builder()
        .expect("safe default rustls protocol versions must be available")
        .build()
        .expect("default reqwest client must build")
}

#[cfg(target_os = "android")]
fn webpki_tls_config() -> Result<rustls::ClientConfig, rustls::Error> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_client_builds() {
        super::client_builder()
            .expect("TLS configuration")
            .build()
            .expect("HTTP client");
    }
}

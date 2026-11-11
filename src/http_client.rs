//! Every reqwest client the proxy builds starts from a builder made here, so
//! all of them use the same TLS setup: ring as the crypto provider and the
//! Mozilla root set bundled at build time. The operating system's store and
//! the platform verifier are never consulted.

use once_cell::sync::Lazy;

static BUNDLED_ROOTS: Lazy<Vec<reqwest::Certificate>> = Lazy::new(|| {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|der| reqwest::Certificate::from_der(der).expect("bundled root certificate"))
        .collect()
});

/// reqwest is built without a crypto provider of its own and takes rustls'
/// process default, so ring is installed as that default unless one is
/// already set.
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// An async client builder that trusts only the bundled roots.
pub fn client_builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder().tls_certs_only(BUNDLED_ROOTS.iter().cloned())
}

/// A blocking client builder that trusts only the bundled roots.
pub fn blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    install_crypto_provider();
    reqwest::blocking::Client::builder().tls_certs_only(BUNDLED_ROOTS.iter().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_build_async_and_blocking_clients_on_ring() {
        client_builder().build().expect("async client");
        blocking_client_builder().build().expect("blocking client");

        let provider = rustls::crypto::CryptoProvider::get_default().expect("a crypto provider");
        let ring = rustls::crypto::ring::default_provider();
        let names = |groups: &[&'static dyn rustls::crypto::SupportedKxGroup]| {
            groups.iter().map(|group| group.name()).collect::<Vec<_>>()
        };
        assert_eq!(names(&provider.kx_groups), names(&ring.kx_groups));
    }

    /// The bundled DER set is the same set of anchors webpki-roots carries,
    /// except that Mozilla constrains one root (TUBITAK Kamu SM, to `.tr`)
    /// outside the certificate itself. webpki-roots keeps that constraint and
    /// the certificate cannot, so through `tls_certs_only` that root is
    /// trusted without it. A second such root fails this test.
    #[test]
    fn bundled_roots_are_the_mozilla_anchor_set() {
        let mut store = rustls::RootCertStore::empty();
        for der in webpki_root_certs::TLS_SERVER_ROOT_CERTS {
            store.add(der.clone()).expect("bundled root parses");
        }
        assert_eq!(store.roots.len(), webpki_roots::TLS_SERVER_ROOTS.len());

        let mut unconstrained = 0;
        for anchor in webpki_roots::TLS_SERVER_ROOTS {
            let bundled = store
                .roots
                .iter()
                .find(|bundled| {
                    bundled.subject == anchor.subject
                        && bundled.subject_public_key_info == anchor.subject_public_key_info
                })
                .expect("every Mozilla anchor is bundled");
            if bundled.name_constraints != anchor.name_constraints {
                assert!(bundled.name_constraints.is_none());
                assert!(
                    anchor
                        .subject
                        .as_ref()
                        .windows(b"TUBITAK".len())
                        .any(|window| window == b"TUBITAK")
                );
                unconstrained += 1;
            }
        }
        assert_eq!(unconstrained, 1);
    }
}

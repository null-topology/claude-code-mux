//! Every reqwest client the proxy builds starts from a builder made here, so
//! all of them use the same TLS setup: ring as the crypto provider and the
//! Mozilla root set bundled at build time. The operating system's store and
//! the platform verifier are never consulted.
//!
//! The roots reach reqwest as certificates, and a certificate cannot carry a
//! name constraint that Mozilla sets on a root outside the certificate. A root
//! whose Mozilla trust anchor has such a constraint is left out rather than
//! trusted for every name, so these clients never trust more than the
//! constrained anchor set does. `clippy.toml` forbids building a client any
//! other way.

use once_cell::sync::Lazy;
use rustls::pki_types::{CertificateDer, TrustAnchor};

static BUNDLED_ROOTS: Lazy<Vec<reqwest::Certificate>> = Lazy::new(|| {
    bundled_root_certs()
        .map(|der| reqwest::Certificate::from_der(der).expect("bundled root certificate"))
        .collect()
});

/// The bundled root certificates whose trust anchor matches a Mozilla anchor
/// with the same name constraints.
fn bundled_root_certs() -> impl Iterator<Item = &'static CertificateDer<'static>> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter(|der| {
            trust_anchor(der).is_some_and(|anchor| {
                webpki_roots::TLS_SERVER_ROOTS.iter().any(|mozilla| {
                    same_root(mozilla, &anchor)
                        && mozilla.name_constraints == anchor.name_constraints
                })
            })
        })
}

/// The trust anchor rustls makes of a root certificate.
fn trust_anchor(der: &CertificateDer<'_>) -> Option<TrustAnchor<'static>> {
    let mut store = rustls::RootCertStore::empty();
    store.add(der.clone()).ok()?;
    store.roots.pop()
}

/// Two anchors are the same root when subject and public key agree.
fn same_root(a: &TrustAnchor<'_>, b: &TrustAnchor<'_>) -> bool {
    a.subject == b.subject && a.subject_public_key_info == b.subject_public_key_info
}

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
    #[allow(clippy::disallowed_methods)]
    let builder = reqwest::Client::builder();
    builder.tls_certs_only(BUNDLED_ROOTS.iter().cloned())
}

/// A blocking client builder that trusts only the bundled roots.
pub fn blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    install_crypto_provider();
    #[allow(clippy::disallowed_methods)]
    let builder = reqwest::blocking::Client::builder();
    builder.tls_certs_only(BUNDLED_ROOTS.iter().cloned())
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

    /// Every bundled root carries the name constraints of its Mozilla anchor.
    /// The Mozilla anchors left out are exactly those with a name constraint
    /// their certificate does not produce, and there are only a few of them.
    #[test]
    fn bundled_roots_are_the_mozilla_anchor_set() {
        let certificates: Vec<TrustAnchor<'static>> = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|der| trust_anchor(der).expect("bundled root parses"))
            .collect();
        let bundled: Vec<TrustAnchor<'static>> = bundled_root_certs()
            .map(|der| trust_anchor(der).expect("bundled root parses"))
            .collect();
        assert_eq!(BUNDLED_ROOTS.len(), bundled.len());
        assert!(bundled.len() >= 100, "only {} roots bundled", bundled.len());

        for root in &bundled {
            assert!(
                webpki_roots::TLS_SERVER_ROOTS.iter().any(|anchor| {
                    same_root(anchor, root) && anchor.name_constraints == root.name_constraints
                }),
                "bundled root without a Mozilla anchor of the same name constraints: {root:?}"
            );
        }

        let left_out: Vec<&TrustAnchor<'static>> = webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .filter(|anchor| !bundled.iter().any(|root| same_root(anchor, root)))
            .collect();
        let constrained_outside: Vec<&TrustAnchor<'static>> = webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .filter(|anchor| {
                let certificate = certificates
                    .iter()
                    .find(|certificate| same_root(anchor, certificate))
                    .expect("every Mozilla anchor has its certificate");
                certificate.name_constraints != anchor.name_constraints
            })
            .collect();
        assert_eq!(left_out, constrained_outside);
        for anchor in &left_out {
            assert!(
                anchor.name_constraints.is_some(),
                "Mozilla anchor left out without a name constraint: {anchor:?}"
            );
        }
        assert!(
            left_out.len() <= 2,
            "{} Mozilla anchors left out: {left_out:?}",
            left_out.len()
        );
    }
}

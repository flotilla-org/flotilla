use std::sync::OnceLock;

use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsProvider {
    Ring,
    AwsLc,
}

pub const fn selected_provider() -> TlsProvider {
    if cfg!(feature = "aws-lc-provider") {
        TlsProvider::AwsLc
    } else {
        TlsProvider::Ring
    }
}

/// Installs the workspace-selected Rustls provider as this process's default.
///
/// Ring is the normal provider so clean development and CI builds avoid
/// aws-lc-sys. Production builds can opt back into AWS-LC with the
/// `aws-lc-provider` Cargo feature.
pub fn install_default_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();

    INSTALLED.get_or_init(|| {
        let selected = selected_provider();
        let expected = crypto_provider();
        if let Some(installed) = CryptoProvider::get_default() {
            assert!(
                providers_match(installed, &expected),
                "a different Rustls crypto provider was installed before Flotilla selected {selected:?}"
            );
            return;
        }

        expected.install_default().unwrap_or_else(|_| panic!("failed to install Flotilla's {selected:?} Rustls crypto provider"));
    });
}

/// Returns a Reqwest client builder after installing the selected provider.
///
/// A User-Agent is always set: api.github.com rejects requests without one
/// as 403 Forbidden, which presents as an opaque authorization failure at
/// call sites like the GitHub App installation-token mint.
pub fn client_builder() -> reqwest::ClientBuilder {
    install_default_provider();
    reqwest::Client::builder().user_agent(concat!("flotilla/", env!("CARGO_PKG_VERSION")))
}

/// Builds a Reqwest client with the selected provider installed.
pub fn client() -> reqwest::Client {
    client_builder().build().expect("build Reqwest client with Flotilla's TLS provider")
}

fn crypto_provider() -> CryptoProvider {
    #[cfg(feature = "aws-lc-provider")]
    {
        rustls::crypto::aws_lc_rs::default_provider()
    }
    #[cfg(not(feature = "aws-lc-provider"))]
    {
        rustls::crypto::ring::default_provider()
    }
}

fn providers_match(actual: &CryptoProvider, expected: &CryptoProvider) -> bool {
    actual.cipher_suites == expected.cipher_suites
        && same_dyn_items(&actual.kx_groups, &expected.kx_groups)
        && signature_algorithms_match(actual.signature_verification_algorithms, expected.signature_verification_algorithms)
        && std::ptr::eq(actual.secure_random, expected.secure_random)
        && std::ptr::eq(actual.key_provider, expected.key_provider)
}

fn signature_algorithms_match(actual: WebPkiSupportedAlgorithms, expected: WebPkiSupportedAlgorithms) -> bool {
    same_dyn_items(actual.all, expected.all)
        && actual.mapping.len() == expected.mapping.len()
        && actual.mapping.iter().zip(expected.mapping).all(
            |((actual_scheme, actual_algorithms), (expected_scheme, expected_algorithms))| {
                actual_scheme == expected_scheme && same_dyn_items(actual_algorithms, expected_algorithms)
            },
        )
}

fn same_dyn_items<T: ?Sized>(actual: &[&T], expected: &[&T]) -> bool {
    actual.len() == expected.len() && actual.iter().zip(expected).all(|(actual, expected)| std::ptr::eq(*actual, *expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_selected_provider_idempotently() {
        install_default_provider();
        install_default_provider();

        let installed = CryptoProvider::get_default().expect("TLS provider installed");
        assert!(providers_match(installed, &crypto_provider()));
        client();
    }

    #[test]
    fn provider_comparison_rejects_modified_provider() {
        let expected = crypto_provider();
        let mut modified = expected.clone();
        modified.cipher_suites.clear();

        assert!(providers_match(&expected, &crypto_provider()));
        assert!(!providers_match(&modified, &expected));
    }

    /// api.github.com rejects requests without a User-Agent as 403 Forbidden,
    /// so every client built here must send one.
    #[tokio::test]
    async fn built_client_always_sends_a_user_agent() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let addr = listener.local_addr().expect("listener address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let mut buf = [0u8; 4096];
            let read = stream.read(&mut buf).expect("read request head");
            let head = String::from_utf8_lossy(&buf[..read]).to_string();
            stream.write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").expect("write response");
            head
        });

        client().get(format!("http://{addr}/")).send().await.expect("request test server");
        let head = server.join().expect("join test server").to_ascii_lowercase();
        assert!(
            head.contains(&format!("user-agent: flotilla/{}", env!("CARGO_PKG_VERSION"))),
            "request must carry the flotilla User-Agent, got:\n{head}"
        );
    }
}

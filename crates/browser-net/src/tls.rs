use std::sync::Arc;

use rustls::{
    ClientConfig, RootCertStore,
    pki_types::CertificateDer,
    version::{TLS12, TLS13},
};

use crate::NetworkError;

pub(crate) fn validate_root_der(bytes: &[u8]) -> Result<(), NetworkError> {
    if bytes.is_empty() {
        return Err(NetworkError::InvalidTrustAnchor(
            "DER certificate is empty".to_owned(),
        ));
    }
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(bytes.to_vec()))
        .map_err(|error| NetworkError::InvalidTrustAnchor(error.to_string()))
}

pub(crate) fn client_config(
    additional_roots: &[Vec<u8>],
) -> Result<Arc<ClientConfig>, NetworkError> {
    let mut roots = oxitls_webpki_roots::webpki_root_certs();
    for root in additional_roots {
        roots
            .add(CertificateDer::from(root.clone()))
            .map_err(|error| NetworkError::InvalidTrustAnchor(error.to_string()))?;
    }

    let provider = Arc::new(oxitls_rustcrypto_provider::provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13, &TLS12])
        .map_err(|error| NetworkError::TlsInitialization(error.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

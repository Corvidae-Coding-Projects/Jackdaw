# TLS fixture identities

These certificates and private keys are non-secret test fixtures generated for the real `https_probe` TCP/TLS scenarios. They must never be used outside local disposable fixtures.

- `localhost-*`: self-signed RSA identity valid for `localhost` and `127.0.0.1`; the probe explicitly adds its DER certificate as an operator trust anchor.
- `mismatch-*`: self-signed RSA identity valid only for `mismatch.invalid`; the probe trusts its DER certificate but connects as `localhost` to prove hostname validation fails before HTTP bytes.

Both identities were generated on 2026-10-13 with OpenSSL 3.5.9, SHA-256, a 2048-bit RSA key, server-auth EKU, and a ten-year fixture validity window. PEM files make the fixtures inspectable; DER certificate and PKCS#8 key files are consumed by rustls.

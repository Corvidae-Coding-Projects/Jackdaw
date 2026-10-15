use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    process::ExitCode,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use browser_net::{
    BrowserClient, BrowserNetworkConfig, BrowserRequest, NetworkEffect, RequestControl,
};
use rustls::{
    ProtocolVersion, ServerConfig, ServerConnection, StreamOwned,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    version::{TLS12, TLS13},
};

const DEFAULT_TRUSTED_URL: &str = "https://example.com/";
const DEFAULT_REJECTED_URL: &str = "https://expired.badssl.com/";
const LOCALHOST_CERT: &[u8] = include_bytes!("../../fixtures/tls/localhost-cert.der");
const LOCALHOST_KEY: &[u8] = include_bytes!("../../fixtures/tls/localhost-key.der");
const MISMATCH_CERT: &[u8] = include_bytes!("../../fixtures/tls/mismatch-cert.der");
const MISMATCH_KEY: &[u8] = include_bytes!("../../fixtures/tls/mismatch-key.der");

#[derive(Debug)]
struct ServerObservation {
    request: Vec<u8>,
    sni: Option<String>,
    protocol: Option<ProtocolVersion>,
}

type FixtureServerError = Box<dyn std::error::Error + Send + Sync>;
type FixtureServerHandle = JoinHandle<Result<ServerObservation, FixtureServerError>>;

fn server_config(
    cert: &[u8],
    key: &[u8],
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<Arc<ServerConfig>, Box<dyn std::error::Error>> {
    let provider = Arc::new(oxitls_rustcrypto_provider::provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
        )?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

fn start_tls_server(
    config: Arc<ServerConfig>,
    response: Option<&'static [u8]>,
    send_close_notify: bool,
) -> Result<(SocketAddr, FixtureServerHandle), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let handle = thread::spawn(move || {
        let (socket, _) = listener.accept()?;
        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
        let connection = ServerConnection::new(config)?;
        let mut stream = StreamOwned::new(connection, socket);
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let read_result = loop {
            match stream.read(&mut buffer) {
                Ok(0) => break Ok(()),
                Ok(read) => {
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break Ok(());
                    }
                }
                Err(error) => break Err(error),
            }
        };
        let observation = ServerObservation {
            sni: stream.conn.server_name().map(str::to_owned),
            protocol: stream.conn.protocol_version(),
            request,
        };
        if read_result.is_ok()
            && let Some(response) = response
        {
            stream.write_all(response)?;
            if send_close_notify {
                stream.conn.send_close_notify();
            }
            stream.flush()?;
        }
        Ok(observation)
    });
    Ok((address, handle))
}

fn finish_server(
    handle: FixtureServerHandle,
) -> Result<ServerObservation, Box<dyn std::error::Error>> {
    handle
        .join()
        .map_err(|_| "TLS fixture server panicked")?
        .map_err(|error| error.to_string().into())
}

fn local_client(root: &[u8]) -> Result<BrowserClient, Box<dyn std::error::Error>> {
    let config = BrowserNetworkConfig::builder()
        .io_timeout(Duration::from_secs(2))
        .connect_timeout(Duration::from_secs(2))
        .cancellation_poll_interval(Duration::from_millis(20))
        .add_root_der(root.to_vec())?
        .build()?;
    Ok(BrowserClient::with_config(config)?)
}

fn successful_local_handshake(
    version: &'static rustls::SupportedProtocolVersion,
    expected: ProtocolVersion,
    path: &str,
    send_close_notify: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let server = server_config(LOCALHOST_CERT, LOCALHOST_KEY, &[version])?;
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nlocal-ok";
    let (address, handle) = start_tls_server(server, Some(response), send_close_notify)?;
    let response = local_client(LOCALHOST_CERT)?
        .fetch_url(&format!("https://localhost:{}/{path}", address.port()))?;
    if response.status != 200 || response.body != b"local-ok" {
        return Err("trusted local TLS response did not match".into());
    }
    let observed = finish_server(handle)?;
    let request = String::from_utf8(observed.request)?;
    if !request.starts_with(&format!("GET /{path} HTTP/1.1\r\n"))
        || !request.contains("User-Agent: rwb/0.1.0\r\n")
    {
        return Err("TLS fixture did not observe the expected request and User-Agent".into());
    }
    if observed.sni.as_deref() != Some("localhost") || observed.protocol != Some(expected) {
        return Err(format!(
            "TLS fixture observed SNI {:?} and protocol {:?}",
            observed.sni, observed.protocol
        )
        .into());
    }
    Ok(())
}

fn rejected_local_handshakes() -> Result<(), Box<dyn std::error::Error>> {
    let server = server_config(LOCALHOST_CERT, LOCALHOST_KEY, &[&TLS13, &TLS12])?;
    let (address, handle) = start_tls_server(server, None, true)?;
    let untrusted = BrowserClient::with_config(BrowserNetworkConfig::builder().build()?)?
        .fetch_url(&format!("https://localhost:{}/unknown", address.port()))
        .expect_err("unknown local issuer unexpectedly succeeded");
    if untrusted.effect() != NetworkEffect::None {
        return Err("unknown issuer did not fail with effect none".into());
    }
    if !finish_server(handle)?.request.is_empty() {
        return Err("unknown-issuer server observed HTTP bytes".into());
    }

    let server = server_config(MISMATCH_CERT, MISMATCH_KEY, &[&TLS13, &TLS12])?;
    let (address, handle) = start_tls_server(server, None, true)?;
    let mismatch = local_client(MISMATCH_CERT)?
        .fetch_url(&format!("https://localhost:{}/mismatch", address.port()))
        .expect_err("hostname-mismatched certificate unexpectedly succeeded");
    if mismatch.effect() != NetworkEffect::None {
        return Err("hostname mismatch did not fail with effect none".into());
    }
    if !finish_server(handle)?.request.is_empty() {
        return Err("hostname-mismatch server observed HTTP bytes".into());
    }
    Ok(())
}

fn cancellation_and_dispatch_effects() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let stalled = thread::spawn(move || -> std::io::Result<()> {
        let (_socket, _) = listener.accept()?;
        thread::sleep(Duration::from_millis(300));
        Ok(())
    });
    let client = local_client(LOCALHOST_CERT)?;
    let control = RequestControl::for_timeout(Duration::from_millis(80))?;
    let error = client
        .fetch_with_control(
            BrowserRequest::parse(&format!("https://localhost:{}/stall", address.port()))?,
            &control,
        )
        .expect_err("stalled TLS handshake unexpectedly completed");
    if error.effect() != NetworkEffect::None {
        return Err("stalled handshake did not fail with effect none".into());
    }
    stalled
        .join()
        .map_err(|_| "stalled TLS fixture panicked")??;

    let server = server_config(LOCALHOST_CERT, LOCALHOST_KEY, &[&TLS13, &TLS12])?;
    let (address, handle) = start_tls_server(server, None, true)?;
    let error = client
        .fetch_url(&format!("https://localhost:{}/drop", address.port()))
        .expect_err("post-dispatch connection close unexpectedly succeeded");
    if error.effect() != NetworkEffect::Unknown {
        return Err("post-dispatch failure did not report effect unknown".into());
    }
    let request = String::from_utf8(finish_server(handle)?.request)?;
    if !request.starts_with("GET /drop HTTP/1.1\r\n") {
        return Err("post-dispatch fixture did not receive the request".into());
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("https-probe: FAIL {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let trusted_url = arguments
        .next()
        .unwrap_or_else(|| DEFAULT_TRUSTED_URL.to_owned());
    let rejected_url = arguments
        .next()
        .unwrap_or_else(|| DEFAULT_REJECTED_URL.to_owned());
    if arguments.next().is_some() {
        return Err("usage: https_probe [TRUSTED_URL [REJECTED_URL]]".into());
    }

    successful_local_handshake(
        &TLS13,
        ProtocolVersion::TLSv1_3,
        "tls13-no-close-notify",
        false,
    )?;
    successful_local_handshake(&TLS12, ProtocolVersion::TLSv1_2, "tls12", true)?;
    rejected_local_handshakes()?;
    cancellation_and_dispatch_effects()?;

    let config = BrowserNetworkConfig::builder().build()?;
    let client = BrowserClient::with_config(config)?;
    let response = client.fetch_url(&trusted_url)?;
    if response.url.scheme() != "https" || !(200..300).contains(&response.status) {
        return Err(format!("trusted request failed at {}", response.url).into());
    }
    if response.body.is_empty() {
        return Err("trusted request returned an empty body".into());
    }

    let rejected = client
        .fetch_url(&rejected_url)
        .expect_err("invalid certificate unexpectedly completed a request");
    if rejected.effect() != NetworkEffect::None {
        return Err(format!(
            "certificate rejection reported {:?} effect",
            rejected.effect()
        )
        .into());
    }

    println!(
        "https-probe: PASS local=custom-root,tls12,tls13,sni,user-agent unknown-issuer=none hostname-mismatch=none handshake-timeout=none post-dispatch=unknown trusted-url={} status={} bytes={} rejected-url={} rejected-effect=none",
        response.url,
        response.status,
        response.body.len(),
        rejected_url
    );
    Ok(())
}

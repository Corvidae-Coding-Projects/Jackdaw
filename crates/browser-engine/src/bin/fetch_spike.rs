use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

use anyhow::{Context, Result, ensure};
use browser_engine::{DocumentHost, DocumentRuntime, NavigationCommit};
use browser_net::{BrowserClient, BrowserResponse};

const EXPECTED_REQUESTS: [&str; 6] = [
    "/app/index",
    "/api/data?case=%CE%BB",
    "/server-error",
    "/drop",
    "/api/late",
    "/replacement",
];

struct SpikeDocumentHost<'a> {
    client: &'a BrowserClient,
}

impl DocumentHost for SpikeDocumentHost<'_> {
    fn fetch_subresource(&self, requested_url: &str) -> Result<BrowserResponse> {
        Ok(self.client.fetch_url(requested_url)?)
    }

    fn load_local_storage(&self, origin: &str) -> Result<BTreeMap<String, String>> {
        Ok(self.client.load_local_storage(origin)?)
    }

    fn replace_local_storage(
        &self,
        origin: &str,
        entries: &BTreeMap<String, String>,
    ) -> Result<()> {
        Ok(self.client.replace_local_storage(origin, entries)?)
    }

    fn document_cookie(&self, document_url: &str) -> Result<String> {
        Ok(self.client.document_cookie(document_url)?)
    }

    fn store_document_cookie(&self, document_url: &str, assignment: &str) -> Result<()> {
        Ok(self
            .client
            .store_document_cookie(document_url, assignment)?)
    }
}

fn navigate_brokered(
    runtime: &mut DocumentRuntime,
    client: &BrowserClient,
    requested_url: &str,
) -> Result<NavigationCommit> {
    let response = client.fetch_url(requested_url)?;
    runtime.commit_brokered_navigation(&SpikeDocumentHost { client }, requested_url, response)
}

fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 512];
    loop {
        let count = stream.read(&mut chunk)?;
        ensure!(count > 0, "client closed before completing its request");
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        ensure!(
            bytes.len() <= 16 * 1024,
            "fixture request exceeded its bound"
        );
    }
    let request = std::str::from_utf8(&bytes).context("fixture request was not UTF-8")?;
    let target = request
        .lines()
        .next()
        .context("fixture request had no request line")?
        .split_whitespace()
        .nth(1)
        .context("fixture request had no target")?;
    Ok(target.to_owned())
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(())
}

fn serve(mut stream: TcpStream, expected: &str) -> Result<()> {
    let target = read_request(&mut stream)?;
    ensure!(
        target == expected,
        "expected request {expected:?}, got {target:?}"
    );
    match target.as_str() {
        "/app/index" => write_response(
            &mut stream,
            "200 OK",
            "<!doctype html><html><body><p id='success'>pending</p><p id='http-error'>pending</p><p id='network-error'>pending</p><p id='late'>original</p></body></html>",
        ),
        "/api/data?case=%CE%BB" => write_response(&mut stream, "200 OK", "payload λ"),
        "/server-error" => write_response(&mut stream, "500 Internal Server Error", "failed"),
        "/drop" => Ok(()),
        "/api/late" => write_response(&mut stream, "200 OK", "obsolete"),
        "/replacement" => write_response(
            &mut stream,
            "200 OK",
            "<!doctype html><html><body><p id='late'>replacement</p></body></html>",
        ),
        _ => unreachable!("request target was checked against the closed fixture list"),
    }
}

fn main() -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || -> Result<()> {
        for expected in EXPECTED_REQUESTS {
            let (stream, _) = listener.accept()?;
            serve(stream, expected)?;
        }
        Ok(())
    });

    let client = BrowserClient::default();
    let mut runtime = DocumentRuntime::from_html("<!doctype html><p>initial</p>")?;
    navigate_brokered(
        &mut runtime,
        &client,
        &format!("http://{address}/app/index"),
    )?;
    ensure!(
        runtime.document_url() == Some(format!("http://{address}/app/index").as_str()),
        "committed document URL was not retained"
    );

    runtime.evaluate_script(
        r#"
        fetch("../api/data?case=λ")
            .then((response) => response.text().then((body) => {
                document.getElementById("success").textContent =
                    `${response.status}:${response.ok}:${body}`;
            }));
        fetch("../server-error")
            .then((response) => response.text().then((body) => {
                document.getElementById("http-error").textContent =
                    `${response.status}:${response.ok}:${body}`;
            }));
        fetch("../drop").catch((error) => {
            document.getElementById("network-error").textContent = error.message;
        });
        "#,
    )?;
    let requests = runtime.take_page_fetch_requests()?;
    ensure!(
        requests.len() == 3,
        "page did not queue exactly three Fetch requests"
    );
    ensure!(requests[0].requested_url == "../api/data?case=λ");
    ensure!(requests[0].url == format!("http://{address}/api/data?case=%CE%BB"));

    let success_response = client.fetch_url(&requests[0].url)?;
    ensure!(runtime.complete_page_fetch(&requests[0], &success_response)?);
    ensure!(
        !runtime.complete_page_fetch(&requests[0], &success_response)?,
        "duplicate Fetch completion was applied"
    );
    let http_error_dispatch = runtime
        .prepare_page_fetch(&requests[1])?
        .context("HTTP-error Fetch became stale before dispatch")?;
    let http_error_response = client.fetch_url(http_error_dispatch.request.url.as_str())?;
    ensure!(runtime.complete_page_fetch(&requests[1], &http_error_response)?);

    let network_error_dispatch = runtime
        .prepare_page_fetch(&requests[2])?
        .context("network-error Fetch became stale before dispatch")?;
    let network_error_message = client
        .fetch_url(network_error_dispatch.request.url.as_str())
        .expect_err("dropped fixture response unexpectedly completed")
        .to_string();
    ensure!(runtime.reject_page_fetch(&requests[2], &network_error_message)?);

    let success = runtime.element_target_by_id("success")?;
    let http_error = runtime.element_target_by_id("http-error")?;
    let network_error = runtime.element_target_by_id("network-error")?;
    ensure!(runtime.observe_text(&success)? == "200:true:payload λ");
    ensure!(runtime.observe_text(&http_error)? == "500:false:failed");
    ensure!(
        runtime.observe_text(&network_error)?
            == "malformed HTTP response: missing header terminator"
    );

    runtime.evaluate_script(
        r#"
        fetch("../api/late").then((response) => response.text()).then((body) => {
            document.getElementById("late").textContent = body;
        });
        "#,
    )?;
    let late = runtime
        .take_page_fetch_requests()?
        .pop()
        .context("page did not queue the late Fetch request")?;
    let late_response = client.fetch_url(&late.url)?;
    let old_generation = runtime.document_generation();
    navigate_brokered(
        &mut runtime,
        &client,
        &format!("http://{address}/replacement"),
    )?;
    ensure!(runtime.document_generation() == old_generation + 1);
    ensure!(
        !runtime.complete_page_fetch(&late, &late_response)?,
        "obsolete Fetch completion entered the replacement realm"
    );
    let replacement = runtime.element_target_by_id("late")?;
    ensure!(runtime.observe_text(&replacement)? == "replacement");

    server
        .join()
        .map_err(|_| anyhow::anyhow!("fixture server thread panicked"))??;
    println!(
        "fetch-spike: PASS requests=4 relative=verified unicode=verified http-500=resolved network-drop=rejected duplicate=discarded stale-generation=discarded"
    );
    Ok(())
}

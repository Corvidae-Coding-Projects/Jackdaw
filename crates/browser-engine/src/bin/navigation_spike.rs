use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

use anyhow::{Context, Result, ensure};
use browser_engine::{DocumentHost, DocumentRuntime, NavigationCommit};
use browser_net::{BrowserClient, BrowserResponse};

const CASES: [(&str, &str); 4] = [
    ("plain", "network document"),
    ("unicode", "Zażółć gęślą jaźń λ"),
    ("multiline", "first\nsecond"),
    ("empty", ""),
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

fn read_request(stream: &mut TcpStream) -> Result<(String, String, Vec<u8>)> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 512];
    let header_end = loop {
        let count = stream.read(&mut chunk)?;
        ensure!(count > 0, "client closed before completing its request");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break offset;
        }
        ensure!(
            bytes.len() <= 16 * 1024,
            "fixture request exceeded its bound"
        );
    };
    let head = std::str::from_utf8(&bytes[..header_end])
        .context("fixture request headers were not UTF-8")?
        .to_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>())
        })
        .transpose()?
        .unwrap_or(0);
    ensure!(
        content_length <= 16 * 1024,
        "fixture request body exceeded its bound"
    );
    let complete_length = header_end + 4 + content_length;
    while bytes.len() < complete_length {
        let count = stream.read(&mut chunk)?;
        ensure!(
            count > 0,
            "client closed before completing its request body"
        );
        bytes.extend_from_slice(&chunk[..count]);
    }
    let request_line = head
        .lines()
        .next()
        .context("fixture request had no request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("fixture request had no method")?;
    let target = parts.next().context("fixture request had no target")?;
    Ok((
        method.to_owned(),
        target.to_owned(),
        bytes[header_end + 4..complete_length].to_vec(),
    ))
}

fn html_response(stream: &mut TcpStream, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    Ok(())
}

fn serve(mut stream: TcpStream) -> Result<()> {
    let (method, target, request_body) = read_request(&mut stream)?;
    if let Some(case_name) = target.strip_prefix("/start/") {
        ensure!(
            method == "GET",
            "redirect fixture received a non-GET request"
        );
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: ../../page/{case_name}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
    } else if let Some(case_name) = target.strip_prefix("/page/") {
        ensure!(method == "GET", "page fixture received a non-GET request");
        let text = CASES
            .iter()
            .find_map(|(name, text)| (*name == case_name).then_some(*text))
            .context("fixture received an unknown case")?;
        html_response(
            &mut stream,
            &format!(
                "<!doctype html><html><body><main id='content'>{text}</main><p id='case'>{case_name}</p></body></html>"
            ),
        )?;
    } else if target == "/styled" {
        ensure!(method == "GET", "styled fixture received a non-GET request");
        html_response(
            &mut stream,
            "<!doctype html><html><head><link rel='stylesheet' href='/assets/site.css'></head><body><main id='styled-content'>brokered stylesheet</main></body></html>",
        )?;
    } else if target == "/assets/site.css" {
        ensure!(
            method == "GET",
            "stylesheet fixture received a non-GET request"
        );
        let body = "@import url('theme/colors.css'); body { margin: 0; }";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/css; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
    } else if target == "/assets/theme/colors.css" {
        ensure!(
            method == "GET",
            "nested color stylesheet received a non-GET request"
        );
        let body = "@import \"../layout.css\"; #styled-content { background: rgb(12, 34, 56); }";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/css; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
    } else if target == "/assets/layout.css" {
        ensure!(
            method == "GET",
            "nested layout stylesheet received a non-GET request"
        );
        let body = "#styled-content { width: 420px; height: 120px; }";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/css; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
    } else if target == "/forms/edit" {
        ensure!(method == "GET", "form fixture received a non-GET request");
        html_response(
            &mut stream,
            "<!doctype html><html><body><form id='profile' method='post' action='../submit/post'><input id='name' name='name' value='Ada λ'><button id='save' name='choice' value='save'>Save</button><button id='preview' name='choice' value='preview' formmethod='get' formaction='../submit/get'>Preview</button></form></body></html>",
        )?;
    } else if target == "/submit/post" {
        ensure!(method == "POST", "POST fixture received {method}");
        ensure!(
            request_body == b"name=Ada+%CE%BB&choice=save",
            "POST fixture received unexpected encoded fields: {:?}",
            String::from_utf8_lossy(&request_body)
        );
        html_response(
            &mut stream,
            "<!doctype html><html><body><main id='content'>post:Ada λ:save</main></body></html>",
        )?;
    } else if target == "/submit/get?name=Ada+%CE%BB&choice=preview" {
        ensure!(method == "GET", "GET form fixture received {method}");
        ensure!(request_body.is_empty(), "GET form fixture received a body");
        html_response(
            &mut stream,
            "<!doctype html><html><body><main id='content'>get:Ada λ:preview</main></body></html>",
        )?;
    } else {
        let body = "not found";
        write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
    }
    stream.flush()?;
    Ok(())
}

fn navigate_brokered(
    runtime: &mut DocumentRuntime,
    client: &BrowserClient,
    requested_url: &str,
) -> Result<NavigationCommit> {
    let response = client.fetch_url(requested_url)?;
    runtime.commit_brokered_navigation(&SpikeDocumentHost { client }, requested_url, response)
}

fn back_brokered(
    runtime: &mut DocumentRuntime,
    client: &BrowserClient,
) -> Result<Option<NavigationCommit>> {
    let Some(requested_url) = runtime.back_target_url() else {
        return Ok(None);
    };
    let response = client.fetch_url(&requested_url)?;
    runtime.commit_brokered_back(&SpikeDocumentHost { client }, &requested_url, response)
}

fn forward_brokered(
    runtime: &mut DocumentRuntime,
    client: &BrowserClient,
) -> Result<Option<NavigationCommit>> {
    let Some(requested_url) = runtime.forward_target_url() else {
        return Ok(None);
    };
    let response = client.fetch_url(&requested_url)?;
    runtime.commit_brokered_forward(&SpikeDocumentHost { client }, &requested_url, response)
}

fn reload_brokered(
    runtime: &mut DocumentRuntime,
    client: &BrowserClient,
) -> Result<Option<NavigationCommit>> {
    let Some(requested_url) = runtime.reload_target_url() else {
        return Ok(None);
    };
    let response = client.fetch_url(&requested_url)?;
    runtime.commit_brokered_reload(&SpikeDocumentHost { client }, &requested_url, response)
}

fn main() -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || -> Result<()> {
        // Four redirect journeys, five direct history traversals, one final redirect journey,
        // a direct reload of that replacement entry, two form-page/submission journeys, and one
        // document plus its three-level independently fetched author stylesheet graph.
        for _ in 0..CASES.len() * 2 + 5 + 2 + 1 + 4 + 4 {
            let (stream, _) = listener.accept()?;
            serve(stream)?;
        }
        Ok(())
    });

    let client = BrowserClient::default();
    let mut runtime = DocumentRuntime::from_html(
        "<!doctype html><html><body><main id='content'>initial</main></body></html>",
    )?;
    let mut prior_target = runtime.element_target_by_id("content")?;
    let initial_generation = runtime.document_generation();

    for (index, (case_name, expected_text)) in CASES.iter().enumerate() {
        let requested_url = format!("http://{address}/start/{case_name}");
        let commit = navigate_brokered(&mut runtime, &client, &requested_url)?;
        ensure!(
            commit.requested_url == requested_url,
            "requested URL changed"
        );
        ensure!(
            commit.final_url == format!("http://{address}/page/{case_name}"),
            "relative redirect resolved to {}",
            commit.final_url
        );
        ensure!(commit.status == 200, "unexpected terminal status");
        ensure!(commit.redirect_count == 1, "unexpected redirect count");
        ensure!(
            runtime.validate_target(&prior_target).is_err(),
            "old target survived navigation"
        );
        let content = runtime.element_target_by_id("content")?;
        ensure!(
            runtime.observe_text(&content)? == *expected_text,
            "case {case_name} committed unexpected semantic text"
        );
        let case = runtime.element_target_by_id("case")?;
        ensure!(
            runtime.observe_text(&case)? == *case_name,
            "case identity was lost"
        );
        ensure!(
            runtime.document_generation() == initial_generation + index as u64 + 1,
            "document generation did not advance exactly once"
        );
        prior_target = content;
    }

    let empty_target = prior_target;
    let back_multiline = back_brokered(&mut runtime, &client)?
        .context("history unexpectedly had no preceding multiline entry")?;
    ensure!(
        back_multiline.final_url == format!("http://{address}/page/multiline"),
        "back traversal selected the wrong entry"
    );
    ensure!(
        runtime.validate_target(&empty_target).is_err(),
        "back traversal did not invalidate the prior target"
    );
    let multiline_target = runtime.element_target_by_id("content")?;
    ensure!(runtime.observe_text(&multiline_target)? == "first\nsecond");

    let back_unicode = back_brokered(&mut runtime, &client)?
        .context("history unexpectedly had no preceding Unicode entry")?;
    ensure!(
        back_unicode.final_url == format!("http://{address}/page/unicode"),
        "second back traversal selected the wrong entry"
    );
    let unicode_target = runtime.element_target_by_id("content")?;
    ensure!(runtime.observe_text(&unicode_target)? == "Zażółć gęślą jaźń λ");

    let forward_multiline = forward_brokered(&mut runtime, &client)?
        .context("history unexpectedly had no forward multiline entry")?;
    ensure!(
        forward_multiline.final_url == format!("http://{address}/page/multiline"),
        "forward traversal selected the wrong entry"
    );
    let before_reload = runtime.element_target_by_id("content")?;
    let reload = reload_brokered(&mut runtime, &client)?
        .context("reload unexpectedly had no current entry")?;
    ensure!(reload.final_url == forward_multiline.final_url);
    ensure!(
        runtime.validate_target(&before_reload).is_err(),
        "reload did not invalidate the prior target"
    );
    ensure!(
        back_brokered(&mut runtime, &client)?
            .context("history unexpectedly had no Unicode entry before replacement")?
            .final_url
            == format!("http://{address}/page/unicode")
    );

    let replacement_url = format!("http://{address}/start/plain");
    navigate_brokered(&mut runtime, &client, &replacement_url)?;
    ensure!(
        runtime.forward_target_url().is_none(),
        "a new navigation after back did not truncate forward history"
    );
    ensure!(
        reload_brokered(&mut runtime, &client)?.is_some(),
        "the replacement entry could not be reloaded"
    );

    let form_url = format!("http://{address}/forms/edit");
    navigate_brokered(&mut runtime, &client, &form_url)?;
    let post_form = runtime.element_target_by_id("profile")?;
    let save = runtime.element_target_by_id("save")?;
    let post_submission = runtime
        .submit(&post_form, Some(&save))?
        .context("POST form submission was unexpectedly cancelled")?;
    let post_request = runtime.prepare_form_submission(post_submission)?;
    let post_response = client.fetch(post_request.request)?;
    let post_commit = runtime.commit_brokered_form_submission(
        &SpikeDocumentHost { client: &client },
        post_request.requested_url,
        post_response,
    )?;
    ensure!(
        post_commit.final_url == format!("http://{address}/submit/post"),
        "POST form committed the wrong URL"
    );
    ensure!(
        runtime.validate_target(&post_form).is_err(),
        "POST form target survived its navigation"
    );
    let post_result = runtime.element_target_by_id("content")?;
    ensure!(runtime.observe_text(&post_result)? == "post:Ada λ:save");

    navigate_brokered(&mut runtime, &client, &form_url)?;
    let get_form = runtime.element_target_by_id("profile")?;
    let preview = runtime.element_target_by_id("preview")?;
    let get_submission = runtime
        .submit(&get_form, Some(&preview))?
        .context("GET form submission was unexpectedly cancelled")?;
    let get_request = runtime.prepare_form_submission(get_submission)?;
    let get_response = client.fetch(get_request.request)?;
    let get_commit = runtime.commit_brokered_form_submission(
        &SpikeDocumentHost { client: &client },
        get_request.requested_url,
        get_response,
    )?;
    ensure!(
        get_commit.final_url
            == format!("http://{address}/submit/get?name=Ada+%CE%BB&choice=preview"),
        "submitter GET override committed the wrong URL: {}",
        get_commit.final_url
    );
    ensure!(
        runtime.validate_target(&get_form).is_err(),
        "GET form target survived its navigation"
    );
    let get_result = runtime.element_target_by_id("content")?;
    ensure!(runtime.observe_text(&get_result)? == "get:Ada λ:preview");

    let styled_url = format!("http://{address}/styled");
    navigate_brokered(&mut runtime, &client, &styled_url)?;
    let styled = runtime.element_target_by_id("styled-content")?;
    ensure!(runtime.observe_text(&styled)? == "brokered stylesheet");
    let hit = runtime.hit_test(350, 100)?;
    ensure!(
        runtime.observe_text(&hit)? == "brokered stylesheet",
        "fetched author stylesheet did not control live layout geometry"
    );
    let frame = runtime.render_document()?;
    ensure!(
        frame
            .data()
            .iter()
            .any(|pixel| pixel.to_u8_array() == [12, 34, 56, 255]),
        "nested imported background color was absent from the CPU frame"
    );

    server
        .join()
        .map_err(|_| anyhow::anyhow!("fixture server thread panicked"))??;
    println!(
        "navigation-spike: PASS cases={} redirects={} history=back,forward,reload,truncation forms=post,get,submitter-override stylesheet=nested-relative-imports,brokered-layout-and-frame unicode=verified generations={} headless=true",
        CASES.len(),
        CASES.len() + 1,
        runtime.document_generation()
    );
    Ok(())
}

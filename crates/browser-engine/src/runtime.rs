//! Thin ownership and lifecycle boundary for one live document and JavaScript realm.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context as _, Result, ensure};
use blitz_dom::{BaseDocument, NodeId};
use boa_engine::{Context, Source};

use crate::{
    runtime_document::{DocumentScript, parse_document},
    runtime_navigation::{DocumentHost, NavigationHistory},
    runtime_script::new_context,
};

/// An owned element identity safe to carry across a process boundary.
///
/// Blitz node handles remain private to the document worker. The generation and
/// opaque element ID are sufficient for the owning runtime to resolve the live
/// node again while rejecting stale observations.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElementIdentity {
    document_generation: u64,
    element_id: String,
}

impl ElementIdentity {
    /// Create a process-safe identity from an observed generation and opaque element ID.
    pub fn new(document_generation: u64, element_id: impl Into<String>) -> Result<Self> {
        let element_id = element_id.into();
        ensure!(!element_id.is_empty(), "element identity must not be empty");
        Ok(Self {
            document_generation,
            element_id,
        })
    }

    /// The document generation captured when this identity was observed.
    pub fn document_generation(&self) -> u64 {
        self.document_generation
    }

    /// The opaque element ID understood by the owning document worker.
    pub fn element_id(&self) -> &str {
        &self.element_id
    }
}

/// A stable action target tied to the document generation that created it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementTarget {
    pub(crate) document_generation: u64,
    pub(crate) node_id: NodeId,
    pub(crate) element_id: String,
}

impl ElementTarget {
    /// The document generation captured when this target was observed.
    pub fn document_generation(&self) -> u64 {
        self.document_generation
    }

    /// The HTML `id` used by this initial component gate.
    pub fn element_id(&self) -> &str {
        &self.element_id
    }

    /// Convert this engine-private target into its process-safe identity.
    pub fn identity(&self) -> ElementIdentity {
        ElementIdentity {
            document_generation: self.document_generation,
            element_id: self.element_id.clone(),
        }
    }
}

/// One page console record retained by the active document realm.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct PageConsoleRecord {
    pub sequence: u64,
    pub level: PageConsoleLevel,
    pub text: String,
}

/// The page-facing console method that produced a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageConsoleLevel {
    Debug,
    Log,
    Info,
    Warn,
    Error,
}

/// One page-initiated network request retained by the active document realm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageNetworkRecord {
    pub sequence: u64,
    pub request_id: String,
    pub requested_url: String,
    pub final_url: Option<String>,
    pub status: Option<u16>,
    pub error: Option<String>,
}

/// Owns one live document generation and its thread-affine JavaScript realm.
pub struct DocumentRuntime {
    pub(crate) document_generation: u64,
    pub(crate) document: BaseDocument,
    pub(crate) context: Context,
    pub(crate) selected_values: HashMap<String, Vec<String>>,
    pub(crate) selected_files: HashMap<String, Vec<crate::runtime_forms::SelectedFile>>,
    pub(crate) caret_positions: HashMap<String, usize>,
    pub(crate) selection_anchors: HashMap<String, usize>,
    pub(crate) next_synthetic_element_id: u64,
    pub(crate) navigation_history: NavigationHistory,
    pub(crate) document_url: Option<String>,
    pub(crate) network_records: Vec<PageNetworkRecord>,
    pub(crate) decoded_images: HashMap<NodeId, (String, crate::runtime_images::DecodedImage)>,
    pub(crate) session_storage_by_origin: HashMap<String, BTreeMap<String, String>>,
    pub(crate) scroll_x: i64,
    pub(crate) scroll_y: i64,
}

impl DocumentRuntime {
    /// Parse an HTML document as generation one and create its JavaScript realm.
    pub fn from_html(html: &str) -> Result<Self> {
        let mut runtime = Self {
            document_generation: 1,
            document: parse_document(html, None),
            context: new_context(1)?,
            selected_values: HashMap::new(),
            selected_files: HashMap::new(),
            caret_positions: HashMap::new(),
            selection_anchors: HashMap::new(),
            next_synthetic_element_id: 1,
            navigation_history: NavigationHistory::default(),
            document_url: None,
            network_records: Vec::new(),
            decoded_images: HashMap::new(),
            session_storage_by_origin: HashMap::new(),
            scroll_x: 0,
            scroll_y: 0,
        };
        runtime.execute_inline_scripts()?;
        Ok(runtime)
    }

    /// Return the active document generation.
    pub fn document_generation(&self) -> u64 {
        self.document_generation
    }

    /// Return the committed network URL, when this document came from navigation.
    pub fn document_url(&self) -> Option<&str> {
        self.document_url.as_deref()
    }

    /// Capture a target for an element in the current document.
    pub fn element_target_by_id(&self, element_id: &str) -> Result<ElementTarget> {
        ensure!(!element_id.is_empty(), "element id must not be empty");
        let node_id = self
            .document
            .get_element_by_id(element_id)
            .with_context(|| {
                format!(
                    "element with id {element_id:?} does not exist in document generation {}",
                    self.document_generation
                )
            })?;
        Ok(ElementTarget {
            document_generation: self.document_generation,
            node_id,
            element_id: element_id.to_owned(),
        })
    }

    /// Resolve a process-safe identity back to a target owned by this runtime.
    pub fn resolve_element_identity(&self, identity: &ElementIdentity) -> Result<ElementTarget> {
        ensure!(
            identity.document_generation == self.document_generation,
            "stale target from document generation {}; current generation is {}",
            identity.document_generation,
            self.document_generation
        );
        let node_id = self
            .document
            .get_element_by_id(&identity.element_id)
            .with_context(|| {
                format!(
                    "element identity {:?} no longer resolves in document generation {}",
                    identity.element_id, self.document_generation
                )
            })?;
        Ok(ElementTarget {
            document_generation: identity.document_generation,
            node_id,
            element_id: identity.element_id.clone(),
        })
    }

    /// Validate that a target still belongs to this exact live document.
    pub fn validate_target(&self, target: &ElementTarget) -> Result<()> {
        ensure!(
            target.document_generation == self.document_generation,
            "stale target from document generation {}; current generation is {}",
            target.document_generation,
            self.document_generation
        );
        ensure!(
            self.document.get_element_by_id(&target.element_id) == Some(target.node_id),
            "target element {:?} no longer resolves to its observed node",
            target.element_id
        );
        Ok(())
    }

    /// Evaluate page JavaScript, drain promise jobs, then apply queued DOM mutations.
    pub fn evaluate_script(&mut self, source: &str) -> Result<()> {
        self.register_document_element_ids()?;
        let evaluation = self
            .context
            .eval(Source::from_bytes(source))
            .map_err(|error| anyhow::anyhow!("page script evaluation failed: {error}"));
        let checkpoint = self.checkpoint();
        evaluation?;
        checkpoint
    }

    fn execute_inline_scripts(&mut self) -> Result<()> {
        for script in self.document_scripts() {
            let DocumentScript::Inline(source) = script else {
                continue;
            };
            self.evaluate_page_script(&source);
        }
        Ok(())
    }

    pub(crate) fn install_network_stylesheets(
        &mut self,
        host: &dyn DocumentHost,
        base_url: &str,
    ) -> Result<()> {
        let base = browser_net::BrowserRequest::parse(base_url)?.url;
        for (node_id, relative_url) in self.document_stylesheets() {
            let Some(stylesheet_url) = base.join(&relative_url).ok() else {
                continue;
            };
            let mut visited = Vec::new();
            let Some(css) =
                Self::fetch_network_stylesheet(host, stylesheet_url.as_str(), 0, &mut visited)
            else {
                continue;
            };
            self.install_stylesheet(node_id, &css);
        }
        Ok(())
    }

    fn fetch_network_stylesheet(
        host: &dyn DocumentHost,
        stylesheet_url: &str,
        depth: usize,
        visited: &mut Vec<String>,
    ) -> Option<String> {
        const MAX_IMPORT_DEPTH: usize = 8;

        if depth > MAX_IMPORT_DEPTH
            || visited
                .iter()
                .any(|visited_url| visited_url == stylesheet_url)
        {
            return None;
        }
        visited.push(stylesheet_url.to_owned());

        let response = host.fetch_subresource(stylesheet_url).ok()?;
        if !(200..=299).contains(&response.status) {
            return None;
        }
        let css = String::from_utf8(response.body).ok()?;
        let base = browser_net::BrowserRequest::parse(stylesheet_url).ok()?.url;
        let mut resolved = String::new();
        for import_url in stylesheet_import_urls(&css) {
            let Some(import_url) = base.join(&import_url).ok() else {
                continue;
            };
            if let Some(imported_css) =
                Self::fetch_network_stylesheet(host, import_url.as_str(), depth + 1, visited)
            {
                resolved.push_str(&imported_css);
                resolved.push('\n');
            }
        }
        resolved.push_str(&css);
        Some(resolved)
    }

    pub(crate) fn execute_network_scripts(
        &mut self,
        host: &dyn DocumentHost,
        base_url: &str,
    ) -> Result<()> {
        let base = browser_net::BrowserRequest::parse(base_url)?.url;
        for script in self.document_scripts() {
            let source = match script {
                DocumentScript::Inline(source) => Some(source),
                DocumentScript::External(relative_url) => base
                    .join(&relative_url)
                    .ok()
                    .and_then(|url| host.fetch_subresource(url.as_str()).ok())
                    .filter(|response| (200..=299).contains(&response.status))
                    .and_then(|response| String::from_utf8(response.body).ok()),
            };
            if let Some(source) = source {
                self.evaluate_page_script(&source);
                self.synchronize_document_cookies(host, base_url)?;
            }
        }
        Ok(())
    }

    fn evaluate_page_script(&mut self, source: &str) {
        // An uncaught exception or resource failure is local to one page script. Supported
        // mutations are checkpointed by `evaluate_script`, and later scripts still run.
        let _ = self.evaluate_script(source);
    }

    fn register_document_element_ids(&mut self) -> Result<()> {
        let mut stack = self.document.root_node().children.clone();
        let mut element_ids = Vec::new();
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            stack.extend(node.children.iter().rev().copied());
            let Some(element) = node.data.downcast_element() else {
                continue;
            };
            if let Some(element_id) = element
                .attr(blitz_dom::LocalName::from("id"))
                .filter(|element_id| !element_id.is_empty())
            {
                element_ids.push(element_id.to_owned());
            }
        }
        let element_ids = serde_json::to_string(&element_ids)
            .context("could not encode live document element IDs")?;
        let source = format!("globalThis.__rwbRegisterElementIds({element_ids})");
        self.context
            .eval(Source::from_bytes(&source))
            .map_err(|error| {
                anyhow::anyhow!("could not register live document elements: {error}")
            })?;
        Ok(())
    }

    /// Return the bounded console records retained by the active page realm.
    pub fn console_records(&mut self) -> Result<Vec<PageConsoleRecord>> {
        let value = self
            .context
            .eval(Source::from_bytes(
                "JSON.stringify(globalThis.__rwbReadConsoleRecords())",
            ))
            .map_err(|error| anyhow::anyhow!("could not read page console records: {error}"))?;
        let json = value
            .to_string(&mut self.context)
            .map_err(|error| anyhow::anyhow!("could not stringify page console records: {error}"))?
            .to_std_string_escaped();
        serde_json::from_str(&json).context("page console records produced invalid JSON")
    }

    /// Resolve one page promise from an owned host completion.
    ///
    /// Callers must retain the generation attached to the originating request. A completion for
    /// an obsolete generation is discarded before entering the replacement realm.
    pub fn complete_host_request(
        &mut self,
        document_generation: u64,
        request_id: &str,
        value: &str,
    ) -> Result<bool> {
        ensure!(
            !request_id.is_empty(),
            "host completion ID must not be empty"
        );
        if document_generation != self.document_generation {
            return Ok(false);
        }
        let request_id =
            serde_json::to_string(request_id).context("could not encode host completion ID")?;
        let value =
            serde_json::to_string(value).context("could not encode host completion value")?;
        let source = format!("globalThis.__rwbCompleteHostRequest({request_id}, {value})");
        let result = self
            .context
            .eval(Source::from_bytes(&source))
            .map_err(|error| anyhow::anyhow!("host completion callback failed: {error}"))?;
        let completed = result.as_boolean().unwrap_or(false);
        self.checkpoint()?;
        Ok(completed)
    }

    /// Run one queued timer task, followed by a microtask and mutation checkpoint.
    pub fn run_next_timer(&mut self) -> Result<bool> {
        let result = self
            .context
            .eval(Source::from_bytes("globalThis.__rwbRunNextTimer()"))
            .map_err(|error| anyhow::anyhow!("timer callback failed: {error}"))?;
        let ran_timer = result.as_boolean().unwrap_or(false);
        self.checkpoint()?;
        Ok(ran_timer)
    }

    /// Move the document viewport by a relative delta, clamped at the top-left origin.
    ///
    /// The offsets are owned by the document runtime so painting, hit testing, semantic
    /// visibility, and both terminal and GUI input can share one viewport state.
    pub fn scroll_by(&mut self, delta_x: i32, delta_y: i32) -> (i64, i64) {
        const VIEWPORT_WIDTH: i64 = 800;
        const VIEWPORT_HEIGHT: i64 = 600;

        self.resolve_layout();
        let mut document_width = 0.0_f64;
        let mut document_height = 0.0_f64;
        let mut stack = self.document.root_node().children.clone();
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            stack.extend(node.children.iter().copied());
            if node.data.downcast_element().is_none() {
                continue;
            }
            if let Some(bounds) = self.document.get_client_bounding_rect(node_id) {
                document_width = document_width.max(bounds.x + bounds.width);
                document_height = document_height.max(bounds.y + bounds.height);
            }
        }
        let max_x = (document_width.ceil() as i64 - VIEWPORT_WIDTH).max(0);
        let max_y = (document_height.ceil() as i64 - VIEWPORT_HEIGHT).max(0);
        self.scroll_x = self
            .scroll_x
            .saturating_add(i64::from(delta_x))
            .clamp(0, max_x);
        self.scroll_y = self
            .scroll_y
            .saturating_add(i64::from(delta_y))
            .clamp(0, max_y);
        (self.scroll_x, self.scroll_y)
    }

    /// Return the current document viewport offset.
    pub fn scroll_offset(&self) -> (i64, i64) {
        (self.scroll_x, self.scroll_y)
    }

    /// Replace the active local document and realm, invalidating prior targets and callbacks.
    pub fn navigate(&mut self, html: &str) -> Result<()> {
        self.navigate_with_base_url(html, None)
    }

    /// Replace the active network document using its committed response URL as the resource base.
    pub(crate) fn navigate_network(
        &mut self,
        html: &str,
        url: &str,
        host: &dyn DocumentHost,
    ) -> Result<()> {
        if let Some(current_url) = self.document_url.clone() {
            let current_origin = serialized_origin(&current_url)?;
            host.replace_local_storage(&current_origin, &self.local_storage_entries()?)?;
            let session_entries = self.session_storage_entries()?;
            self.session_storage_by_origin
                .insert(current_origin, session_entries);
        }

        let incoming_origin = serialized_origin(url)?;
        let incoming_local_storage = host.load_local_storage(&incoming_origin)?;
        let incoming_session_storage = self
            .session_storage_by_origin
            .get(&incoming_origin)
            .cloned()
            .unwrap_or_default();

        self.replace_document(html, Some(url))?;
        self.install_local_storage(&incoming_local_storage)?;
        self.install_session_storage(&incoming_session_storage)?;
        self.install_document_cookie(&host.document_cookie(url)?)?;
        self.install_network_stylesheets(host, url)?;
        self.install_network_images(host, url)?;
        self.execute_network_scripts(host, url)?;
        host.replace_local_storage(&incoming_origin, &self.local_storage_entries()?)?;
        let session_entries = self.session_storage_entries()?;
        self.session_storage_by_origin
            .insert(incoming_origin, session_entries);
        Ok(())
    }

    fn navigate_with_base_url(&mut self, html: &str, base_url: Option<&str>) -> Result<()> {
        self.replace_document(html, base_url)?;
        self.execute_inline_scripts()
    }

    fn replace_document(&mut self, html: &str, base_url: Option<&str>) -> Result<()> {
        let generation = self
            .document_generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("document generation overflow"))?;
        let document = parse_document(html, base_url);
        self.document_generation = generation;
        self.document = document;
        self.context = new_context(generation)?;
        self.selected_values.clear();
        self.selected_files.clear();
        self.caret_positions.clear();
        self.next_synthetic_element_id = 1;
        self.document_url = base_url.map(str::to_owned);
        self.network_records.clear();
        self.decoded_images.clear();
        self.scroll_x = 0;
        self.scroll_y = 0;
        Ok(())
    }
}

fn stylesheet_import_urls(css: &str) -> Vec<String> {
    const IMPORT: &[u8] = b"@import";

    let bytes = css.as_bytes();
    let mut urls = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor..].starts_with(b"/*") {
            cursor = bytes[cursor + 2..]
                .windows(2)
                .position(|window| window == b"*/")
                .map_or(bytes.len(), |offset| cursor + offset + 4);
            continue;
        }
        if matches!(bytes[cursor], b'\'' | b'"') {
            cursor = skip_css_string(bytes, cursor);
            continue;
        }
        if bytes.len() - cursor < IMPORT.len()
            || !bytes[cursor..cursor + IMPORT.len()].eq_ignore_ascii_case(IMPORT)
        {
            cursor += 1;
            continue;
        }

        let mut value_start = cursor + IMPORT.len();
        while bytes.get(value_start).is_some_and(u8::is_ascii_whitespace) {
            value_start += 1;
        }
        let (value, next) = if matches!(bytes.get(value_start), Some(b'\'' | b'"')) {
            css_string_value(css, value_start)
        } else if bytes
            .get(value_start..value_start.saturating_add(4))
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"url("))
        {
            css_url_value(css, value_start + 4)
        } else {
            (None, value_start.saturating_add(1))
        };
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            urls.push(value);
        }
        cursor = next.max(cursor + 1);
    }
    urls
}

fn skip_css_string(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor = (cursor + 2).min(bytes.len());
        } else if bytes[cursor] == quote {
            return cursor + 1;
        } else {
            cursor += 1;
        }
    }
    bytes.len()
}

fn css_string_value(css: &str, start: usize) -> (Option<String>, usize) {
    let bytes = css.as_bytes();
    let end = skip_css_string(bytes, start);
    if end <= start + 1 || bytes.get(end - 1) != bytes.get(start) {
        return (None, end);
    }
    (Some(css[start + 1..end - 1].to_owned()), end)
}

fn css_url_value(css: &str, mut start: usize) -> (Option<String>, usize) {
    let bytes = css.as_bytes();
    while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }
    if matches!(bytes.get(start), Some(b'\'' | b'"')) {
        let (value, mut end) = css_string_value(css, start);
        while bytes.get(end).is_some_and(u8::is_ascii_whitespace) {
            end += 1;
        }
        if bytes.get(end) == Some(&b')') {
            end += 1;
        }
        return (value, end);
    }

    let end = bytes[start..]
        .iter()
        .position(|byte| *byte == b')')
        .map_or(bytes.len(), |offset| start + offset);
    let value = css[start..end].trim().to_owned();
    (Some(value), (end + 1).min(bytes.len()))
}

fn serialized_origin(url: &str) -> Result<String> {
    let parsed = browser_net::BrowserRequest::parse(url)?.url;
    let origin = parsed.origin().ascii_serialization();
    ensure!(
        origin != "null",
        "localStorage requires an HTTP or HTTPS origin"
    );
    Ok(origin)
}

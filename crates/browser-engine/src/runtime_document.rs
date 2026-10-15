//! Live document mutation and semantic readback operations.

use anyhow::{Result, ensure};
use blitz_dom::{
    BaseDocument, DocumentConfig, LocalName, Namespace, QualName, build_single_font_ctx,
};
use blitz_html::HtmlDocument;

use crate::runtime::{DocumentRuntime, ElementTarget};

pub(crate) const DEFAULT_FONT: &[u8] = include_bytes!("../assets/Cantarell-Regular.ttf");

/// One executable classic script discovered in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DocumentScript {
    Inline(String),
    External(String),
}

impl DocumentRuntime {
    /// Replace an element's children with one text node after validating its generation.
    pub fn replace_text(&mut self, target: &ElementTarget, value: &str) -> Result<()> {
        self.validate_target(target)?;
        let mut mutator = self.document.mutate();
        let text = mutator.create_text_node(value);
        mutator.replace_children(target.node_id, &[text]);
        Ok(())
    }

    /// Set one inline style property after validating the target generation.
    pub fn set_style_property(
        &mut self,
        target: &ElementTarget,
        name: &str,
        value: &str,
    ) -> Result<()> {
        self.validate_target(target)?;
        ensure!(!name.is_empty(), "style property name must not be empty");
        self.document
            .mutate()
            .set_style_property(target.node_id, name, value);
        Ok(())
    }

    /// Move a live child below a live parent using DOM append semantics.
    pub fn append_child(&mut self, parent: &ElementTarget, child: &ElementTarget) -> Result<()> {
        self.validate_target(parent)?;
        self.validate_target(child)?;
        ensure!(parent != child, "an element cannot be appended to itself");
        self.document
            .mutate()
            .append_children(parent.node_id, &[child.node_id]);
        Ok(())
    }

    /// Remove a live element from the document, invalidating its existing target.
    pub fn remove(&mut self, target: &ElementTarget) -> Result<()> {
        self.validate_target(target)?;
        self.document.mutate().remove_node(target.node_id);
        Ok(())
    }

    /// Observe the text content of the exact generation-checked target.
    pub fn observe_text(&self, target: &ElementTarget) -> Result<String> {
        self.validate_target(target)?;
        Ok(self
            .document
            .get_node(target.node_id)
            .expect("validated target must reference a live node")
            .text_content())
    }

    /// Observe the semantic text content of the complete active document.
    ///
    /// This owned string is the initial service observation seam; callers never receive Blitz
    /// node handles or borrowed document data.
    pub fn observe_document_text(&self) -> Result<String> {
        let mut document_stack = self.document.root_node().children.clone();
        let body = loop {
            let Some(node_id) = document_stack.pop() else {
                return Ok(String::new());
            };
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            if node
                .data
                .downcast_element()
                .is_some_and(|element| element.name.local.as_ref() == "body")
            {
                break node_id;
            }
            document_stack.extend(node.children.iter().rev().copied());
        };

        let body = self
            .document
            .get_node(body)
            .expect("discovered body node remains live");
        let mut stack: Vec<_> = body.children.iter().rev().copied().collect();
        let mut readable = String::new();
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            if let Some(element) = node.data.downcast_element() {
                let name = element.name.local.as_ref();
                if matches!(name, "script" | "style" | "template" | "noscript")
                    || element.attr(LocalName::from("hidden")).is_some()
                    || element
                        .attr(LocalName::from("aria-hidden"))
                        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
                    || element
                        .attr(LocalName::from("style"))
                        .map(|value| {
                            value
                                .chars()
                                .filter(|character| !character.is_ascii_whitespace())
                                .collect::<String>()
                                .to_ascii_lowercase()
                        })
                        .is_some_and(|style| {
                            style.contains("display:none") || style.contains("visibility:hidden")
                        })
                {
                    continue;
                }
            }
            if node.children.is_empty() {
                readable.push_str(&node.text_content());
            } else {
                stack.extend(node.children.iter().rev().copied());
            }
        }
        Ok(readable)
    }

    /// Return the active document's title text.
    pub fn observe_document_title(&self) -> String {
        let mut stack = self.document.root_node().children.clone();
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            stack.extend(node.children.iter().rev().copied());
            if node
                .data
                .downcast_element()
                .is_some_and(|element| element.name.local.as_ref() == "title")
            {
                return node.text_content();
            }
        }
        String::new()
    }

    /// Return a text control's live edited value.
    pub fn observe_value(&self, target: &ElementTarget) -> Result<String> {
        self.validate_target(target)?;
        self.document
            .get_node(target.node_id)
            .and_then(|node| node.data.downcast_element())
            .and_then(|element| element.text_input_data())
            .map(|input| input.editor.text().to_string())
            .ok_or_else(|| anyhow::anyhow!("target is not an editable text control"))
    }

    /// Return the live checked state for a checkbox or radio target.
    pub fn observe_checked(&self, target: &ElementTarget) -> Result<bool> {
        self.validate_target(target)?;
        self.checkbox_checked(target.node_id)
    }

    pub(crate) fn set_value_attribute(&mut self, target: blitz_dom::NodeId, value: &str) {
        self.document.mutate().set_attribute(
            target,
            QualName::new(None, Namespace::from(""), LocalName::from("value")),
            value,
        );
        self.set_live_value(target, value);
    }

    pub(crate) fn set_live_value(&mut self, target: blitz_dom::NodeId, value: &str) {
        if let Some(input) = self
            .document
            .get_node_mut(target)
            .and_then(|node| node.data.downcast_element_mut())
            .and_then(|element| element.text_input_data_mut())
        {
            input.editor.set_text(value);
            self.document.resolve(0.0);
        }
    }

    pub(crate) fn document_stylesheets(&self) -> Vec<(blitz_dom::NodeId, String)> {
        let mut stack: Vec<_> = self
            .document
            .root_node()
            .children
            .iter()
            .rev()
            .copied()
            .collect();
        let mut stylesheets = Vec::new();
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            stack.extend(node.children.iter().rev().copied());
            let Some(element) = node.data.downcast_element() else {
                continue;
            };
            if element.name.local.as_ref() != "link" {
                continue;
            }
            let is_stylesheet = element.attr(LocalName::from("rel")).is_some_and(|rel| {
                rel.split_ascii_whitespace()
                    .any(|token| token.eq_ignore_ascii_case("stylesheet"))
            });
            if !is_stylesheet {
                continue;
            }
            if let Some(href) = element
                .attr(LocalName::from("href"))
                .filter(|href| !href.trim().is_empty())
            {
                stylesheets.push((node_id, href.to_owned()));
            }
        }
        stylesheets
    }

    pub(crate) fn install_stylesheet(&mut self, node_id: blitz_dom::NodeId, css: &str) {
        let mut mutator = self.document.mutate();
        let text = mutator.create_text_node(css);
        mutator.replace_children(node_id, &[text]);
        drop(mutator);
        self.document.upsert_stylesheet_for_node(node_id);
    }

    pub(crate) fn document_scripts(&self) -> Vec<DocumentScript> {
        let mut stack: Vec<_> = self
            .document
            .root_node()
            .children
            .iter()
            .rev()
            .copied()
            .collect();
        let mut scripts = Vec::new();
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            stack.extend(node.children.iter().rev().copied());
            let Some(element) = node.data.downcast_element() else {
                continue;
            };
            if element.name.local.as_ref() != "script" {
                continue;
            }
            let script_type = element
                .attr(LocalName::from("type"))
                .unwrap_or("text/javascript")
                .trim()
                .to_ascii_lowercase();
            if !matches!(
                script_type.as_str(),
                "text/javascript" | "application/javascript"
            ) {
                continue;
            }
            scripts.push(
                element
                    .attr(LocalName::from("src"))
                    .filter(|source| !source.trim().is_empty())
                    .map(|source| DocumentScript::External(source.to_owned()))
                    .unwrap_or_else(|| DocumentScript::Inline(node.text_content())),
            );
        }
        scripts
    }

    /// Resolve style and layout for the active document.
    pub fn resolve_layout(&mut self) {
        self.document.resolve(0.0);
    }
}

pub(crate) fn parse_document(html: &str, base_url: Option<&str>) -> BaseDocument {
    let config = DocumentConfig {
        base_url: base_url.map(str::to_owned),
        font_ctx: Some(build_single_font_ctx(DEFAULT_FONT)),
        ..DocumentConfig::default()
    };
    HtmlDocument::from_html(html, config).into_inner()
}

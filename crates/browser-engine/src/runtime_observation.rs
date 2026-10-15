//! Owned semantic control projection for frontend-neutral observations.

use anyhow::Result;
use blitz_dom::{LocalName, Namespace, NodeId, QualName};
use browser_net::BrowserRequest;

use crate::runtime::{DocumentRuntime, ElementTarget};

/// One actionable control discovered from the current live document.
#[derive(Debug, Clone)]
pub struct SemanticControl {
    pub target: ElementTarget,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub disabled: bool,
    pub focused: bool,
    pub checked: Option<bool>,
    pub required: bool,
    pub invalid: bool,
}

/// One actionable link discovered from the current live document.
#[derive(Debug, Clone)]
pub struct SemanticLink {
    pub target: ElementTarget,
    pub name: String,
    pub url: String,
}

impl DocumentRuntime {
    /// Project controls as owned values without exposing Blitz node identities.
    pub fn observe_controls(&mut self) -> Result<Vec<SemanticControl>> {
        self.document.resolve(0.0);
        let mut stack = self.document.root_node().children.clone();
        let mut controls = Vec::new();
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            stack.extend(node.children.iter().rev().copied());
            let Some(element) = node.data.downcast_element() else {
                continue;
            };
            let element_id = element
                .attr(LocalName::from("id"))
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let tag = element.name.local.to_string();
            let input_type = element
                .attr(LocalName::from("type"))
                .unwrap_or("text")
                .to_owned();
            let role = match (tag.as_str(), input_type.as_str()) {
                ("button", _) | ("input", "button" | "submit") => "button",
                ("input", "checkbox") => "checkbox",
                ("input", "radio") => "radio",
                ("input" | "textarea", _) => "textbox",
                ("select", _) => "combobox",
                _ => continue,
            };
            let name = element
                .attr(LocalName::from("aria-label"))
                .or_else(|| element.attr(LocalName::from("name")))
                .map(str::to_owned)
                .unwrap_or_else(|| node.text_content());
            let value = element
                .text_input_data()
                .map(|input| input.editor.text().to_string())
                .or_else(|| element.attr(LocalName::from("value")).map(str::to_owned));
            let checked = element.checkbox_input_checked();
            let disabled = element.attr(LocalName::from("disabled")).is_some();
            let required = element.attr(LocalName::from("required")).is_some();
            let element_id = self.ensure_element_id(node_id, element_id);
            if role == "button" && !matches!(input_type.as_str(), "button" | "reset") {
                self.ensure_owning_form_id(node_id);
            }
            let focused = self.document.get_focussed_node_id() == Some(node_id);
            let invalid = required && !disabled && !self.control_is_valid(node_id)?;
            controls.push(SemanticControl {
                target: self.target_for_node(node_id, element_id),
                role: role.to_owned(),
                name,
                value,
                disabled,
                focused,
                checked,
                required,
                invalid,
            });
        }
        Ok(controls)
    }

    /// Project links as owned values with destinations resolved against the committed URL.
    pub fn observe_links(&mut self) -> Result<Vec<SemanticLink>> {
        let Some(base) = self.document_url.as_deref() else {
            return Ok(Vec::new());
        };
        let base = BrowserRequest::parse(base)?.url;
        let mut stack = self.document.root_node().children.clone();
        let mut links = Vec::new();
        while let Some(node_id) = stack.pop() {
            let Some(node) = self.document.get_node(node_id) else {
                continue;
            };
            stack.extend(node.children.iter().rev().copied());
            let Some(element) = node.data.downcast_element() else {
                continue;
            };
            if element.name.local.as_ref() != "a" {
                continue;
            }
            let Some(href) = element.attr(LocalName::from("href")).map(str::to_owned) else {
                continue;
            };
            let element_id = element
                .attr(LocalName::from("id"))
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let name = element
                .attr(LocalName::from("aria-label"))
                .map(str::to_owned)
                .unwrap_or_else(|| node.text_content());
            let element_id = self.ensure_element_id(node_id, element_id);
            links.push(SemanticLink {
                target: self.target_for_node(node_id, element_id),
                name,
                url: base.join(&href)?.to_string(),
            });
        }
        Ok(links)
    }

    /// Resolve a current link target against the committed document URL.
    ///
    /// Pointer hit testing may select a painted descendant of an anchor. The descendant remains
    /// the event target, while the enclosing anchor owns the navigation default action.
    pub fn link_url(&self, target: &ElementTarget) -> Result<Option<String>> {
        self.validate_target(target)?;
        let Some(base) = self.document_url.as_deref() else {
            return Ok(None);
        };
        let base = BrowserRequest::parse(base)?.url;
        let mut candidate = Some(target.node_id);
        while let Some(node_id) = candidate {
            let Some(node) = self.document.get_node(node_id) else {
                return Ok(None);
            };
            if let Some(element) = node.data.downcast_element()
                && element.name.local.as_ref() == "a"
                && let Some(href) = element.attr(LocalName::from("href"))
            {
                return Ok(Some(base.join(href)?.to_string()));
            }
            candidate = node.parent;
        }
        Ok(None)
    }

    pub(crate) fn ensure_element_id(
        &mut self,
        node_id: NodeId,
        existing: Option<String>,
    ) -> String {
        if let Some(existing) = existing {
            return existing;
        }
        loop {
            let sequence = self.next_synthetic_element_id;
            self.next_synthetic_element_id = self
                .next_synthetic_element_id
                .checked_add(1)
                .expect("synthetic element identity space exhausted");
            let element_id = format!("__rwb_observed_{}_{}", self.document_generation, sequence);
            if self.document.get_element_by_id(&element_id).is_some() {
                continue;
            }
            self.document.mutate().set_attribute(
                node_id,
                QualName::new(None, Namespace::from(""), LocalName::from("id")),
                &element_id,
            );
            return element_id;
        }
    }

    fn ensure_owning_form_id(&mut self, node_id: NodeId) {
        let mut ancestor = self.document.get_node(node_id).and_then(|node| node.parent);
        while let Some(candidate) = ancestor {
            if self
                .document
                .get_node(candidate)
                .and_then(|node| node.data.downcast_element())
                .is_some_and(|element| element.name.local.as_ref() == "form")
            {
                let existing = self
                    .document
                    .get_node(candidate)
                    .and_then(|node| node.data.downcast_element())
                    .and_then(|element| element.attr(LocalName::from("id")))
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
                self.ensure_element_id(candidate, existing);
                return;
            }
            ancestor = self
                .document
                .get_node(candidate)
                .and_then(|node| node.parent);
        }
    }

    fn target_for_node(&self, node_id: NodeId, element_id: String) -> ElementTarget {
        ElementTarget {
            document_generation: self.document_generation,
            node_id,
            element_id,
        }
    }
}

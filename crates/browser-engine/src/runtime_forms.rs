//! Native selection controls and successful form submission.

use anyhow::{Context as _, Result, ensure};
use blitz_dom::{LocalName, NodeId};
use boa_engine::Source;

use crate::runtime::{DocumentRuntime, ElementTarget};

/// One successful name/value pair in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormField {
    pub name: String,
    pub value: String,
}

/// One selected host file after the service has removed its local path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedFile {
    pub file_name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// One successful file control in a multipart form submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormFile {
    pub name: String,
    pub file_name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// The browser-owned result of a form submission default action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormSubmission {
    pub method: String,
    pub action: String,
    pub enctype: String,
    pub fields: Vec<FormField>,
    pub files: Vec<FormFile>,
}

impl DocumentRuntime {
    /// Resolve a public submission target into its owning form and optional submitter.
    ///
    /// Agent callers may target either the form itself or a submit button discovered in a
    /// semantic observation. Keeping this resolution beside form serialization preserves the
    /// selected submitter for its name/value and `formaction`/`formmethod` overrides.
    pub fn resolve_submission_target(
        &self,
        target: &ElementTarget,
    ) -> Result<(ElementTarget, Option<ElementTarget>)> {
        self.validate_target(target)?;
        if self.tag_name(target.node_id).as_deref() == Some("form") {
            return Ok((target.clone(), None));
        }

        let is_submitter = match self.tag_name(target.node_id).as_deref() {
            Some("button") => !matches!(
                self.attribute(target.node_id, "type").as_deref(),
                Some("button" | "reset")
            ),
            Some("input") => matches!(
                self.attribute(target.node_id, "type").as_deref(),
                Some("submit" | "image")
            ),
            _ => false,
        };
        ensure!(
            is_submitter,
            "submission target is not a form or submit control"
        );

        let mut ancestor = self
            .document
            .get_node(target.node_id)
            .and_then(|node| node.parent);
        while let Some(node_id) = ancestor {
            if self.tag_name(node_id).as_deref() == Some("form") {
                let element_id = self
                    .node_element_id(node_id)
                    .context("owning form must have an element id")?;
                return Ok((
                    ElementTarget {
                        document_generation: self.document_generation,
                        node_id,
                        element_id,
                    },
                    Some(target.clone()),
                ));
            }
            ancestor = self.document.get_node(node_id).and_then(|node| node.parent);
        }

        anyhow::bail!("submit control does not belong to a form")
    }

    /// Resolve and serialize the form default action for a clicked submit control.
    /// Non-submit controls have no form default action and return `None`.
    pub fn submission_after_click(
        &mut self,
        target: &ElementTarget,
    ) -> Result<Option<FormSubmission>> {
        self.validate_target(target)?;
        let is_submitter = match self.tag_name(target.node_id).as_deref() {
            Some("button") => !matches!(
                self.attribute(target.node_id, "type").as_deref(),
                Some("button" | "reset")
            ),
            Some("input") => matches!(
                self.attribute(target.node_id, "type").as_deref(),
                Some("submit" | "image")
            ),
            _ => false,
        };
        if !is_submitter {
            return Ok(None);
        }
        let (form, submitter) = self.resolve_submission_target(target)?;
        self.submit(&form, submitter.as_ref())
    }

    /// Attach operator-authorized files to a native file input without retaining host paths.
    pub fn select_files(
        &mut self,
        target: &ElementTarget,
        files: Vec<(String, String, Vec<u8>)>,
    ) -> Result<()> {
        self.validate_target(target)?;
        self.ensure_target_enabled(target.node_id)?;
        ensure!(
            self.tag_name(target.node_id).as_deref() == Some("input")
                && self.attribute(target.node_id, "type").as_deref() == Some("file"),
            "target is not a file input"
        );
        ensure!(
            self.has_attribute(target.node_id, "multiple") || files.len() <= 1,
            "file input does not accept multiple files"
        );
        let files = files
            .into_iter()
            .map(|(file_name, media_type, bytes)| SelectedFile {
                file_name,
                media_type,
                bytes,
            })
            .collect();
        self.selected_files.insert(target.element_id.clone(), files);
        self.dispatch_event(target.node_id, "input", true)?;
        self.dispatch_event(target.node_id, "change", true)?;
        self.checkpoint()?;
        Ok(())
    }

    /// Select exact option values and emit `input` then `change` when state changes.
    pub fn select(&mut self, target: &ElementTarget, values: &[String]) -> Result<bool> {
        self.validate_target(target)?;
        self.ensure_target_enabled(target.node_id)?;
        ensure!(
            self.tag_name(target.node_id).as_deref() == Some("select"),
            "target is not a select control"
        );
        let multiple = self.has_attribute(target.node_id, "multiple");
        ensure!(
            multiple || values.len() <= 1,
            "single-select controls accept at most one value"
        );

        let options = self.option_values(target.node_id);
        for value in values {
            ensure!(
                options.iter().any(|candidate| candidate == value),
                "select option value {value:?} does not exist"
            );
        }
        let selected: Vec<String> = if multiple {
            options
                .into_iter()
                .filter(|option| values.iter().any(|value| value == option))
                .collect()
        } else {
            values.first().cloned().into_iter().collect()
        };
        let previous = self.selected_values(target.node_id);
        if previous == selected {
            return Ok(false);
        }
        self.selected_values
            .insert(target.element_id.clone(), selected.clone());
        self.sync_select_state(
            target.node_id,
            selected.first().map(String::as_str).unwrap_or(""),
        )?;
        self.dispatch_event(target.node_id, "input", true)?;
        self.dispatch_event(target.node_id, "change", true)?;
        self.checkpoint()?;
        Ok(true)
    }

    /// Return the current selected values in option order.
    pub fn observe_selected_values(&self, target: &ElementTarget) -> Result<Vec<String>> {
        self.validate_target(target)?;
        ensure!(
            self.tag_name(target.node_id).as_deref() == Some("select"),
            "target is not a select control"
        );
        Ok(self.selected_values(target.node_id))
    }

    /// Validate and serialize a form using the explicitly selected submitter.
    pub fn submit(
        &mut self,
        form: &ElementTarget,
        submitter: Option<&ElementTarget>,
    ) -> Result<Option<FormSubmission>> {
        self.validate_target(form)?;
        ensure!(
            self.tag_name(form.node_id).as_deref() == Some("form"),
            "submission target is not a form"
        );
        if let Some(submitter) = submitter {
            self.validate_target(submitter)?;
            ensure!(
                self.descendants(form.node_id).contains(&submitter.node_id),
                "submitter does not belong to the form"
            );
            self.ensure_target_enabled(submitter.node_id)?;
        }
        let controls = self.descendants(form.node_id);
        self.validate_required_controls(&controls)?;
        if !self.dispatch_event(form.node_id, "submit", true)? {
            self.checkpoint()?;
            return Ok(None);
        }

        let mut fields = Vec::new();
        let mut files = Vec::new();
        for node_id in controls {
            if self.has_attribute(node_id, "disabled") {
                continue;
            }
            let Some(name) = self
                .attribute(node_id, "name")
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            match self.tag_name(node_id).as_deref() {
                Some("input") => {
                    let input_type = self
                        .attribute(node_id, "type")
                        .unwrap_or_else(|| "text".into());
                    if matches!(input_type.as_str(), "submit" | "button") {
                        if submitter.is_some_and(|target| target.node_id == node_id) {
                            fields.push(FormField {
                                name,
                                value: self.attribute(node_id, "value").unwrap_or_default(),
                            });
                        }
                    } else if matches!(input_type.as_str(), "checkbox" | "radio") {
                        if self.checkbox_checked(node_id)? {
                            fields.push(FormField {
                                name,
                                value: self
                                    .attribute(node_id, "value")
                                    .unwrap_or_else(|| "on".into()),
                            });
                        }
                    } else if input_type == "file" {
                        if let Some(selected) = self
                            .node_element_id(node_id)
                            .and_then(|id| self.selected_files.get(&id))
                        {
                            files.extend(selected.iter().cloned().map(|file| FormFile {
                                name: name.clone(),
                                file_name: file.file_name,
                                media_type: file.media_type,
                                bytes: file.bytes,
                            }));
                        }
                    } else {
                        let value = self
                            .document
                            .get_node(node_id)
                            .and_then(|node| node.data.downcast_element())
                            .and_then(|element| element.text_input_data())
                            .map(|input| input.editor.text().to_string())
                            .unwrap_or_else(|| {
                                self.attribute(node_id, "value").unwrap_or_default()
                            });
                        fields.push(FormField { name, value });
                    }
                }
                Some("select") => {
                    for value in self.selected_values(node_id) {
                        fields.push(FormField {
                            name: name.clone(),
                            value,
                        });
                    }
                }
                Some("textarea") => {
                    let value = self
                        .document
                        .get_node(node_id)
                        .and_then(|node| node.data.downcast_element())
                        .and_then(|element| element.text_input_data())
                        .map(|input| input.editor.text().to_string())
                        .unwrap_or_else(|| {
                            self.document
                                .get_node(node_id)
                                .map(|node| node.text_content())
                                .unwrap_or_default()
                        });
                    fields.push(FormField { name, value });
                }
                Some("button") if submitter.is_some_and(|target| target.node_id == node_id) => {
                    fields.push(FormField {
                        name,
                        value: self.attribute(node_id, "value").unwrap_or_default(),
                    });
                }
                _ => {}
            }
        }
        self.checkpoint()?;
        let submitter_method = submitter
            .and_then(|target| self.attribute(target.node_id, "formmethod"))
            .filter(|method| !method.is_empty());
        let submitter_action = submitter
            .and_then(|target| self.attribute(target.node_id, "formaction"))
            .filter(|action| !action.is_empty());
        let submitter_enctype = submitter
            .and_then(|target| self.attribute(target.node_id, "formenctype"))
            .filter(|enctype| !enctype.is_empty());
        Ok(Some(FormSubmission {
            method: submitter_method
                .or_else(|| self.attribute(form.node_id, "method"))
                .unwrap_or_else(|| "get".into())
                .to_ascii_lowercase(),
            action: submitter_action
                .or_else(|| self.attribute(form.node_id, "action"))
                .unwrap_or_default(),
            enctype: submitter_enctype
                .or_else(|| self.attribute(form.node_id, "enctype"))
                .unwrap_or_else(|| "application/x-www-form-urlencoded".into())
                .to_ascii_lowercase(),
            fields,
            files,
        }))
    }

    fn validate_required_controls(&mut self, controls: &[NodeId]) -> Result<()> {
        for &node_id in controls {
            if !self.has_attribute(node_id, "required") || self.has_attribute(node_id, "disabled") {
                continue;
            }
            if !self.control_is_valid(node_id)? {
                self.focus_node(node_id)?;
                anyhow::bail!("required form control is invalid");
            }
        }
        Ok(())
    }

    pub(crate) fn control_is_valid(&self, node_id: NodeId) -> Result<bool> {
        Ok(match self.tag_name(node_id).as_deref() {
            Some("select") => !self.selected_values(node_id).is_empty(),
            Some("input")
                if matches!(
                    self.attribute(node_id, "type").as_deref(),
                    Some("checkbox" | "radio")
                ) =>
            {
                self.checkbox_checked(node_id)?
            }
            Some("input" | "textarea") => self
                .document
                .get_node(node_id)
                .and_then(|node| node.data.downcast_element())
                .and_then(|element| element.text_input_data())
                .map(|input| !input.editor.text().to_string().is_empty())
                .unwrap_or_else(|| {
                    self.attribute(node_id, "value")
                        .is_some_and(|value| !value.is_empty())
                }),
            _ => true,
        })
    }

    fn selected_values(&self, select: NodeId) -> Vec<String> {
        if let Some(id) = self.node_element_id(select)
            && let Some(values) = self.selected_values.get(&id)
        {
            return values.clone();
        }
        let options = self
            .descendants(select)
            .into_iter()
            .filter(|node| self.tag_name(*node).as_deref() == Some("option"));
        let mut selected = options
            .filter(|node| self.has_attribute(*node, "selected"))
            .map(|node| self.option_value(node))
            .collect::<Vec<_>>();
        if selected.is_empty()
            && !self.has_attribute(select, "multiple")
            && let Some(first) = self
                .descendants(select)
                .into_iter()
                .find(|node| self.tag_name(*node).as_deref() == Some("option"))
        {
            selected.push(self.option_value(first));
        }
        selected
    }

    fn option_values(&self, select: NodeId) -> Vec<String> {
        self.descendants(select)
            .into_iter()
            .filter(|node| self.tag_name(*node).as_deref() == Some("option"))
            .map(|node| self.option_value(node))
            .collect()
    }

    fn option_value(&self, option: NodeId) -> String {
        self.attribute(option, "value").unwrap_or_else(|| {
            self.document
                .get_node(option)
                .map(|node| node.text_content())
                .unwrap_or_default()
        })
    }

    fn sync_select_state(&mut self, node_id: NodeId, value: &str) -> Result<()> {
        let id = self
            .node_element_id(node_id)
            .context("select control must have an element id")?;
        let id = serde_json::to_string(&id).context("could not encode select id")?;
        let value = serde_json::to_string(value).context("could not encode select value")?;
        self.context
            .eval(Source::from_bytes(&format!(
                "globalThis.__rwbSetControlState({id}, {value}, false)"
            )))
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!("could not synchronize select state: {error}"))
    }

    fn descendants(&self, root: NodeId) -> Vec<NodeId> {
        let mut result = Vec::new();
        let mut stack: Vec<NodeId> = self
            .document
            .get_node(root)
            .map(|node| node.children.iter().rev().copied().collect())
            .unwrap_or_default();
        while let Some(node_id) = stack.pop() {
            result.push(node_id);
            if let Some(node) = self.document.get_node(node_id) {
                stack.extend(node.children.iter().rev().copied());
            }
        }
        result
    }

    fn tag_name(&self, node_id: NodeId) -> Option<String> {
        self.document
            .get_node(node_id)?
            .data
            .downcast_element()
            .map(|element| element.name.local.to_string())
    }

    fn attribute(&self, node_id: NodeId, name: &str) -> Option<String> {
        self.document
            .get_node(node_id)?
            .data
            .downcast_element()?
            .attr(LocalName::from(name))
            .map(str::to_owned)
    }

    fn has_attribute(&self, node_id: NodeId, name: &str) -> bool {
        self.attribute(node_id, name).is_some()
    }
}

//! User interaction, event dispatch, focus, and form-control default actions.

use anyhow::{Context as _, Result, ensure};
use blitz_dom::{LocalName, NodeId};
use boa_engine::Source;

use crate::runtime::{DocumentRuntime, ElementTarget};

impl DocumentRuntime {
    /// Dispatch focus and click events, then perform the control's default action when enabled.
    pub fn click(&mut self, target: &ElementTarget) -> Result<bool> {
        self.validate_target(target)?;
        self.document.resolve(0.0);
        let target_node = target.node_id;
        self.ensure_target_enabled(target_node)?;
        self.focus_node(target_node)?;
        let radio_group = self.radio_group_snapshot(target_node)?;
        let prior_checked = self
            .is_checkable(target_node)
            .then(|| self.checkbox_checked(target_node))
            .transpose()?;
        if let Some(group) = &radio_group {
            self.activate_radio(target_node)?;
            self.sync_control_states(group.iter().map(|(node, _)| *node))?;
        } else if let Some(checked) = prior_checked {
            self.set_checkbox_checked(target_node, !checked)?;
            self.sync_control_state(target_node)?;
        } else {
            self.sync_control_state(target_node)?;
        }
        let default_enabled = self.dispatch_event(target_node, "click", true)?;
        if let Some(checked) = prior_checked {
            let changed = self.checkbox_checked(target_node)? != checked;
            if default_enabled {
                if changed {
                    self.dispatch_event(target_node, "input", true)?;
                    self.dispatch_event(target_node, "change", true)?;
                }
            } else if let Some(group) = &radio_group {
                self.restore_checked_states(group)?;
                self.sync_control_states(group.iter().map(|(node, _)| *node))?;
            } else {
                self.set_checkbox_checked(target_node, checked)?;
                self.sync_control_state(target_node)?;
            }
        }
        self.checkpoint()?;
        Ok(default_enabled)
    }

    /// Replace a text control's live value and emit observable editing events.
    pub fn fill(&mut self, target: &ElementTarget, value: &str) -> Result<()> {
        self.validate_target(target)?;
        self.document.resolve(0.0);
        let target_node = target.node_id;
        ensure!(
            self.is_text_input(target_node),
            "target is not an editable text control"
        );
        self.ensure_target_enabled(target_node)?;
        self.focus_node(target_node)?;
        self.set_value_attribute(target_node, value);
        self.caret_positions
            .insert(target.element_id.clone(), value.chars().count());
        self.selection_anchors.remove(&target.element_id);
        self.sync_control_state(target_node)?;
        self.dispatch_event(target_node, "input", true)?;
        self.dispatch_event(target_node, "change", true)?;
        self.checkpoint()
    }

    /// Dispatch one keyboard action and apply supported text-control default editing.
    pub fn key(&mut self, target: &ElementTarget, key: &str, modifiers: &[String]) -> Result<bool> {
        self.validate_target(target)?;
        ensure!(!key.is_empty(), "keyboard key must not be empty");
        for modifier in modifiers {
            ensure!(
                matches!(modifier.as_str(), "Shift" | "Control" | "Alt" | "Meta"),
                "unsupported keyboard modifier: {modifier}"
            );
        }
        self.document.resolve(0.0);
        let target_node = target.node_id;
        ensure!(
            self.is_text_input(target_node),
            "target is not an editable text control"
        );
        self.ensure_target_enabled(target_node)?;
        self.focus_node(target_node)?;
        let default_enabled = self.dispatch_key_event(target_node, "keydown", key)?;
        if default_enabled {
            let value = self.observe_value(target)?;
            let mut characters = value.chars().collect::<Vec<_>>();
            let character_count = characters.len();
            let element_id = target.element_id.clone();
            let mut caret = self
                .caret_positions
                .get(&element_id)
                .copied()
                .unwrap_or(character_count)
                .min(character_count);
            let mut anchor = self.selection_anchors.get(&element_id).copied();
            let shift = modifiers.iter().any(|modifier| modifier == "Shift");
            let command_modifier = modifiers
                .iter()
                .any(|modifier| matches!(modifier.as_str(), "Control" | "Alt" | "Meta"));
            let existing_selection = anchor
                .filter(|anchor| *anchor != caret)
                .map(|anchor| (anchor.min(caret), anchor.max(caret)));
            let changed = match key {
                "ArrowLeft" => {
                    if shift {
                        anchor.get_or_insert(caret);
                        caret = caret.saturating_sub(1);
                    } else if let Some((start, _)) = existing_selection {
                        caret = start;
                        anchor = None;
                    } else {
                        caret = caret.saturating_sub(1);
                        anchor = None;
                    }
                    false
                }
                "ArrowRight" => {
                    if shift {
                        anchor.get_or_insert(caret);
                        caret = caret.saturating_add(1).min(character_count);
                    } else if let Some((_, end)) = existing_selection {
                        caret = end;
                        anchor = None;
                    } else {
                        caret = caret.saturating_add(1).min(character_count);
                        anchor = None;
                    }
                    false
                }
                "ArrowUp" => {
                    if shift {
                        anchor.get_or_insert(caret);
                    } else {
                        anchor = None;
                    }
                    let current_start = characters[..caret]
                        .iter()
                        .rposition(|character| *character == '\n')
                        .map_or(0, |newline| newline + 1);
                    if current_start > 0 {
                        let column = caret - current_start;
                        let previous_end = current_start - 1;
                        let previous_start = characters[..previous_end]
                            .iter()
                            .rposition(|character| *character == '\n')
                            .map_or(0, |newline| newline + 1);
                        caret = (previous_start + column).min(previous_end);
                    }
                    false
                }
                "ArrowDown" => {
                    if shift {
                        anchor.get_or_insert(caret);
                    } else {
                        anchor = None;
                    }
                    let current_start = characters[..caret]
                        .iter()
                        .rposition(|character| *character == '\n')
                        .map_or(0, |newline| newline + 1);
                    let current_end = characters[caret..]
                        .iter()
                        .position(|character| *character == '\n')
                        .map_or(character_count, |offset| caret + offset);
                    if current_end < character_count {
                        let column = caret - current_start;
                        let next_start = current_end + 1;
                        let next_end = characters[next_start..]
                            .iter()
                            .position(|character| *character == '\n')
                            .map_or(character_count, |offset| next_start + offset);
                        caret = (next_start + column).min(next_end);
                    }
                    false
                }
                "Home" => {
                    if shift {
                        anchor.get_or_insert(caret);
                    } else {
                        anchor = None;
                    }
                    caret = characters[..caret]
                        .iter()
                        .rposition(|character| *character == '\n')
                        .map_or(0, |newline| newline + 1);
                    false
                }
                "End" => {
                    if shift {
                        anchor.get_or_insert(caret);
                    } else {
                        anchor = None;
                    }
                    caret = characters[caret..]
                        .iter()
                        .position(|character| *character == '\n')
                        .map_or(character_count, |offset| caret + offset);
                    false
                }
                "Backspace" | "Delete" if existing_selection.is_some() => {
                    let (start, end) = existing_selection.expect("selection was checked");
                    characters.drain(start..end);
                    caret = start;
                    anchor = None;
                    true
                }
                "Backspace" if caret > 0 => {
                    characters.remove(caret - 1);
                    caret -= 1;
                    anchor = None;
                    true
                }
                "Delete" if caret < character_count => {
                    characters.remove(caret);
                    anchor = None;
                    true
                }
                _ if key.chars().count() == 1 && !command_modifier => {
                    if let Some((start, end)) = existing_selection {
                        characters.drain(start..end);
                        caret = start;
                    }
                    characters.insert(
                        caret,
                        key.chars().next().expect("single key has a character"),
                    );
                    caret += 1;
                    anchor = None;
                    true
                }
                _ => false,
            };
            self.caret_positions.insert(element_id.clone(), caret);
            if let Some(anchor) = anchor.filter(|anchor| *anchor != caret) {
                self.selection_anchors.insert(element_id.clone(), anchor);
            } else {
                self.selection_anchors.remove(&element_id);
            }
            if changed {
                let value = characters.into_iter().collect::<String>();
                self.set_live_value(target_node, &value);
                self.sync_control_state(target_node)?;
                self.dispatch_event(target_node, "input", true)?;
            }
        }
        self.dispatch_key_event(target_node, "keyup", key)?;
        self.checkpoint()?;
        Ok(default_enabled)
    }

    /// Append text through keyboard and input events, honoring cancelled keydown events.
    pub fn type_text(&mut self, target: &ElementTarget, text: &str) -> Result<()> {
        for character in text.chars() {
            self.key(target, &character.to_string(), &[])?;
        }
        Ok(())
    }

    /// Set a checkbox or radio to the requested state.
    pub fn check(&mut self, target: &ElementTarget, checked: bool) -> Result<bool> {
        self.validate_target(target)?;
        self.document.resolve(0.0);
        let target_node = target.node_id;
        let current = self.checkbox_checked(target_node)?;
        let radio_group = self.radio_group_snapshot(target_node)?;
        ensure!(
            checked || radio_group.is_none(),
            "a radio control cannot be unchecked directly"
        );
        self.ensure_target_enabled(target_node)?;
        self.focus_node(target_node)?;
        if current == checked {
            self.sync_control_state(target_node)?;
            return Ok(false);
        }
        if let Some(group) = &radio_group {
            self.activate_radio(target_node)?;
            self.sync_control_states(group.iter().map(|(node, _)| *node))?;
        } else {
            self.set_checkbox_checked(target_node, checked)?;
            self.sync_control_state(target_node)?;
        }
        self.dispatch_event(target_node, "input", true)?;
        self.dispatch_event(target_node, "change", true)?;
        self.checkpoint()?;
        Ok(true)
    }

    /// Focus a live target through the same transition path used by interactions.
    pub fn focus(&mut self, target: &ElementTarget) -> Result<()> {
        self.validate_target(target)?;
        self.document.resolve(0.0);
        self.ensure_target_enabled(target.node_id)?;
        self.focus_node(target.node_id)?;
        self.checkpoint()
    }

    fn is_text_input(&self, node: NodeId) -> bool {
        self.document
            .get_node(node)
            .and_then(|node| node.data.downcast_element())
            .and_then(|element| element.text_input_data())
            .is_some()
    }

    pub(crate) fn ensure_target_enabled(&self, target_node: NodeId) -> Result<()> {
        let disabled = self
            .document
            .get_node(target_node)
            .and_then(|node| node.data.downcast_element())
            .is_some_and(|element| element.attr(LocalName::from("disabled")).is_some());
        ensure!(!disabled, "target control is disabled");
        Ok(())
    }

    pub(crate) fn focus_node(&mut self, target_node: NodeId) -> Result<()> {
        if self.document.get_focussed_node_id() == Some(target_node) {
            return Ok(());
        }
        if let Some(previous) = self.document.get_focussed_node_id()
            && self.node_element_id(previous).is_some()
        {
            self.dispatch_event(previous, "blur", false)?;
            self.dispatch_event(previous, "focusout", true)?;
        }
        self.document.set_focus_to(target_node);
        self.dispatch_event(target_node, "focus", false)?;
        self.dispatch_event(target_node, "focusin", true)?;
        Ok(())
    }

    pub(crate) fn dispatch_event(
        &mut self,
        target_node: NodeId,
        event_type: &str,
        bubbles: bool,
    ) -> Result<bool> {
        self.dispatch_event_with_details(target_node, event_type, bubbles, "{}")
    }

    fn dispatch_key_event(
        &mut self,
        target_node: NodeId,
        event_type: &str,
        key: &str,
    ) -> Result<bool> {
        let key = serde_json::to_string(key).context("could not encode keyboard key")?;
        self.dispatch_event_with_details(
            target_node,
            event_type,
            true,
            &format!("{{ key: {key} }}"),
        )
    }

    fn dispatch_event_with_details(
        &mut self,
        target_node: NodeId,
        event_type: &str,
        bubbles: bool,
        details: &str,
    ) -> Result<bool> {
        let path = self.event_path(target_node)?;
        let path = serde_json::to_string(&path).context("could not encode event path")?;
        let event_type =
            serde_json::to_string(event_type).context("could not encode event type")?;
        let source =
            format!("globalThis.__rwbDispatchEvent({path}, {event_type}, {bubbles}, {details})");
        let result = self
            .context
            .eval(Source::from_bytes(&source))
            .map_err(|error| anyhow::anyhow!("event dispatch failed: {error}"))?;
        self.checkpoint()?;
        Ok(result.as_boolean().unwrap_or(false))
    }

    fn event_path(&self, target_node: NodeId) -> Result<Vec<String>> {
        let mut path = Vec::new();
        let mut node_id = target_node;
        ensure!(
            self.document.get_node(node_id).is_some(),
            "event target node is no longer live"
        );
        loop {
            if let Some(id) = self.node_element_id(node_id) {
                path.push(id);
            }
            let Some(parent) = self
                .document
                .get_node(node_id)
                .expect("live event path")
                .parent
            else {
                break;
            };
            node_id = parent;
        }
        Ok(path)
    }

    pub(crate) fn node_element_id(&self, node_id: NodeId) -> Option<String> {
        self.document
            .get_node(node_id)?
            .data
            .attr(LocalName::from("id"))
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    }

    fn is_checkable(&self, node_id: NodeId) -> bool {
        self.document
            .get_node(node_id)
            .and_then(|node| node.data.downcast_element())
            .is_some_and(|element| element.checkbox_input_checked().is_some())
    }

    fn radio_group_snapshot(&self, node_id: NodeId) -> Result<Option<Vec<(NodeId, bool)>>> {
        let element = self
            .document
            .get_node(node_id)
            .and_then(|node| node.data.downcast_element())
            .context("radio target stopped being an element")?;
        if element.attr(LocalName::from("type")) != Some("radio") {
            return Ok(None);
        }
        let Some(name) = element.attr(LocalName::from("name")).map(str::to_owned) else {
            return Ok(Some(vec![(node_id, self.checkbox_checked(node_id)?)]));
        };
        let mut stack = self.document.root_node().children.clone();
        let mut group = Vec::new();
        while let Some(candidate) = stack.pop() {
            let Some(node) = self.document.get_node(candidate) else {
                continue;
            };
            stack.extend(node.children.iter().copied());
            let Some(candidate_element) = node.data.downcast_element() else {
                continue;
            };
            if candidate_element.attr(LocalName::from("type")) == Some("radio")
                && candidate_element.attr(LocalName::from("name")) == Some(name.as_str())
                && let Some(checked) = candidate_element.checkbox_input_checked()
            {
                group.push((candidate, checked));
            }
        }
        Ok(Some(group))
    }

    fn activate_radio(&mut self, node_id: NodeId) -> Result<()> {
        let group = self
            .radio_group_snapshot(node_id)?
            .context("target is not a radio control")?;
        for (candidate, _) in group {
            self.set_checkbox_checked(candidate, candidate == node_id)?;
        }
        Ok(())
    }

    fn restore_checked_states(&mut self, states: &[(NodeId, bool)]) -> Result<()> {
        for (node_id, checked) in states {
            if self.document.get_node(*node_id).is_some() {
                self.set_checkbox_checked(*node_id, *checked)?;
            }
        }
        Ok(())
    }

    fn sync_control_states(&mut self, nodes: impl IntoIterator<Item = NodeId>) -> Result<()> {
        for node_id in nodes {
            if self.node_element_id(node_id).is_some() {
                self.sync_control_state(node_id)?;
            }
        }
        Ok(())
    }

    pub(crate) fn checkbox_checked(&self, node_id: NodeId) -> Result<bool> {
        self.document
            .get_node(node_id)
            .and_then(|node| node.data.downcast_element())
            .and_then(|element| element.checkbox_input_checked())
            .context("target is not a checkbox control")
    }

    fn set_checkbox_checked(&mut self, node_id: NodeId, checked: bool) -> Result<()> {
        self.document
            .get_node_mut(node_id)
            .and_then(|node| node.data.downcast_element_mut())
            .context("checkbox target stopped being an element")?
            .set_checkbox_input_checked(checked);
        Ok(())
    }

    fn sync_control_state(&mut self, node_id: NodeId) -> Result<()> {
        let element_id = self
            .node_element_id(node_id)
            .context("control must have an element id")?;
        let element = self
            .document
            .get_node(node_id)
            .and_then(|node| node.data.downcast_element())
            .context("control target stopped being an element")?;
        let value = element
            .text_input_data()
            .map(|input| input.editor.text().to_string())
            .unwrap_or_default();
        let checked = element.checkbox_input_checked().unwrap_or(false);
        let element_id =
            serde_json::to_string(&element_id).context("could not encode control id")?;
        let value = serde_json::to_string(&value).context("could not encode control value")?;
        let source = format!("globalThis.__rwbSetControlState({element_id}, {value}, {checked})");
        self.context
            .eval(Source::from_bytes(&source))
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!("could not synchronize control state: {error}"))
    }
}

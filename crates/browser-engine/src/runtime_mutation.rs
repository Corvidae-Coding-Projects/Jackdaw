//! Microtask checkpoints and generation-filtered DOM mutation application.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context as _, Result, ensure};
use blitz_dom::{LocalName, Namespace, NodeId, QualName};
use boa_engine::Source;
use serde::Deserialize;

use crate::{runtime::DocumentRuntime, runtime_navigation::DocumentHost};

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ScriptMutation {
    CreateElement {
        generation: u64,
        element_id: String,
        value: String,
    },
    SetId {
        generation: u64,
        element_id: String,
        value: String,
    },
    SetText {
        generation: u64,
        element_id: String,
        value: String,
    },
    SetStyle {
        generation: u64,
        element_id: String,
        property_name: String,
        value: String,
    },
    AppendChild {
        generation: u64,
        element_id: String,
        value: String,
    },
    Remove {
        generation: u64,
        element_id: String,
    },
}

impl DocumentRuntime {
    pub(crate) fn checkpoint(&mut self) -> Result<()> {
        self.context
            .run_jobs()
            .map_err(|error| anyhow::anyhow!("page microtask checkpoint failed: {error}"))?;
        let mutations = self.take_script_mutations()?;
        let mut created = HashMap::<String, NodeId>::new();
        for mutation in mutations {
            match mutation {
                ScriptMutation::CreateElement {
                    generation,
                    element_id,
                    value: tag_name,
                } => {
                    if generation != self.document_generation {
                        continue;
                    }
                    ensure!(
                        !created.contains_key(&element_id),
                        "page script reused created element identity {element_id:?}"
                    );
                    let name = QualName::new(
                        None,
                        Namespace::from("http://www.w3.org/1999/xhtml"),
                        LocalName::from(tag_name),
                    );
                    let node = self.document.mutate().create_element(name, Vec::new());
                    created.insert(element_id, node);
                }
                ScriptMutation::SetId {
                    generation,
                    element_id,
                    value,
                } => {
                    if generation != self.document_generation {
                        continue;
                    }
                    ensure!(
                        !value.is_empty(),
                        "page script assigned an empty element id"
                    );
                    let node = self.script_node(&created, &element_id)?;
                    self.document.mutate().set_attribute(
                        node,
                        QualName::new(None, Namespace::from(""), LocalName::from("id")),
                        &value,
                    );
                    if let Some(node) = created.remove(&element_id) {
                        created.insert(value, node);
                    }
                }
                ScriptMutation::SetText {
                    generation,
                    element_id,
                    value,
                } => {
                    if generation != self.document_generation {
                        continue;
                    }
                    let node = self.script_node(&created, &element_id)?;
                    let mut mutator = self.document.mutate();
                    let text = mutator.create_text_node(&value);
                    mutator.replace_children(node, &[text]);
                }
                ScriptMutation::SetStyle {
                    generation,
                    element_id,
                    property_name,
                    value,
                } => {
                    if generation != self.document_generation {
                        continue;
                    }
                    ensure!(
                        !property_name.is_empty(),
                        "style property name must not be empty"
                    );
                    let node = self.script_node(&created, &element_id)?;
                    self.document
                        .mutate()
                        .set_style_property(node, &property_name, &value);
                }
                ScriptMutation::AppendChild {
                    generation,
                    element_id,
                    value: child_id,
                } => {
                    if generation != self.document_generation {
                        continue;
                    }
                    let parent = self.script_node(&created, &element_id)?;
                    let child = self.script_node(&created, &child_id)?;
                    ensure!(parent != child, "an element cannot be appended to itself");
                    self.document.mutate().append_children(parent, &[child]);
                }
                ScriptMutation::Remove {
                    generation,
                    element_id,
                } => {
                    if generation != self.document_generation {
                        continue;
                    }
                    let node = self.script_node(&created, &element_id)?;
                    self.document.mutate().remove_node(node);
                }
            }
        }
        Ok(())
    }

    fn take_script_mutations(&mut self) -> Result<Vec<ScriptMutation>> {
        let value = self
            .context
            .eval(Source::from_bytes(
                "JSON.stringify(globalThis.__rwbTakeMutations())",
            ))
            .map_err(|error| anyhow::anyhow!("could not collect page mutations: {error}"))?;
        let json = value
            .to_string(&mut self.context)
            .map_err(|error| anyhow::anyhow!("could not stringify page mutations: {error}"))?
            .to_std_string_escaped();
        serde_json::from_str(&json).context("page mutation queue produced invalid JSON")
    }

    /// Replace the page realm's local-storage contents with an origin-scoped snapshot.
    pub fn install_local_storage(&mut self, entries: &BTreeMap<String, String>) -> Result<()> {
        let entries = serde_json::to_string(&entries.iter().collect::<Vec<_>>())
            .context("could not encode localStorage snapshot")?;
        let source = format!("globalThis.__rwbReplaceLocalStorage({entries})");
        self.context
            .eval(Source::from_bytes(&source))
            .map_err(|error| anyhow::anyhow!("could not install localStorage snapshot: {error}"))?;
        Ok(())
    }

    /// Snapshot the complete page-visible local-storage map for durable origin persistence.
    pub fn local_storage_entries(&mut self) -> Result<BTreeMap<String, String>> {
        self.storage_entries("__rwbReadLocalStorage", "localStorage")
    }

    /// Replace the page realm's session-storage contents with this tab's origin snapshot.
    pub fn install_session_storage(&mut self, entries: &BTreeMap<String, String>) -> Result<()> {
        let entries = serde_json::to_string(&entries.iter().collect::<Vec<_>>())
            .context("could not encode sessionStorage snapshot")?;
        let source = format!("globalThis.__rwbReplaceSessionStorage({entries})");
        self.context
            .eval(Source::from_bytes(&source))
            .map_err(|error| {
                anyhow::anyhow!("could not install sessionStorage snapshot: {error}")
            })?;
        Ok(())
    }

    /// Snapshot the page-visible session-storage map for this live browsing context.
    pub fn session_storage_entries(&mut self) -> Result<BTreeMap<String, String>> {
        self.storage_entries("__rwbReadSessionStorage", "sessionStorage")
    }

    /// Install the policy-filtered cookie string visible to page script.
    pub(crate) fn install_document_cookie(&mut self, cookie: &str) -> Result<()> {
        let cookie =
            serde_json::to_string(cookie).context("could not encode document.cookie snapshot")?;
        let source = format!("globalThis.__rwbReplaceDocumentCookie({cookie})");
        self.context
            .eval(Source::from_bytes(&source))
            .map_err(|error| {
                anyhow::anyhow!("could not install document.cookie snapshot: {error}")
            })?;
        Ok(())
    }

    /// Apply queued page cookie assignments through the browser cookie policy and refresh the
    /// script-visible string. HttpOnly values never cross this boundary into the page realm.
    pub(crate) fn synchronize_document_cookies(
        &mut self,
        host: &dyn DocumentHost,
        document_url: &str,
    ) -> Result<()> {
        let value = self
            .context
            .eval(Source::from_bytes(
                "JSON.stringify(globalThis.__rwbTakeDocumentCookieAssignments())",
            ))
            .map_err(|error| {
                anyhow::anyhow!("could not collect document.cookie assignments: {error}")
            })?;
        let json = value
            .to_string(&mut self.context)
            .map_err(|error| {
                anyhow::anyhow!("could not stringify document.cookie assignments: {error}")
            })?
            .to_std_string_escaped();
        let assignments = serde_json::from_str::<Vec<String>>(&json)
            .context("page document.cookie assignments produced invalid JSON")?;
        for assignment in assignments {
            host.store_document_cookie(document_url, &assignment)?;
        }
        self.install_document_cookie(&host.document_cookie(document_url)?)
    }

    fn storage_entries(
        &mut self,
        reader: &str,
        storage_name: &str,
    ) -> Result<BTreeMap<String, String>> {
        let source = format!("JSON.stringify(globalThis.{reader}())");
        let value = self
            .context
            .eval(Source::from_bytes(&source))
            .map_err(|error| anyhow::anyhow!("could not read {storage_name} snapshot: {error}"))?;
        let json = value
            .to_string(&mut self.context)
            .map_err(|error| {
                anyhow::anyhow!("could not stringify {storage_name} snapshot: {error}")
            })?
            .to_std_string_escaped();
        let entries = serde_json::from_str::<Vec<(String, String)>>(&json)
            .with_context(|| format!("page {storage_name} snapshot produced invalid JSON"))?;
        Ok(entries.into_iter().collect())
    }

    fn script_node(&self, created: &HashMap<String, NodeId>, element_id: &str) -> Result<NodeId> {
        created
            .get(element_id)
            .copied()
            .or_else(|| self.document.get_element_by_id(element_id))
            .with_context(|| format!("page script targeted missing element {element_id:?}"))
    }
}

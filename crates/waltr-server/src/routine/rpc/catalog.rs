//! Bounded complete replacement catalog; production loading remains a separate cutover.
use crate::routine::{
    definition::{Budgets, Contract, Definition, Entrypoint, Envelope, Port, Trigger},
    ports::PortError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio::sync::watch;

/// Public start contexts offered by the host, never inferred from selectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StartContext {
    /// Start without a source session.
    Global,
    /// Start with an exact authorized source session.
    Session,
}

/// A pinned definition and independently declared public start offers.
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    /// Immutable program definition.
    pub definition: Arc<Definition>,
    /// Exact entrypoint to offered contexts; empty means not publicly offered.
    pub offers: BTreeMap<String, BTreeSet<StartContext>>,
}
impl CatalogEntry {
    /// Validate formal discovery offers without granting actual source access.
    pub fn permits(&self, entrypoint: &str, has_source: bool) -> bool {
        self.offers.get(entrypoint).is_some_and(|offers| {
            offers.contains(&if has_source {
                StartContext::Session
            } else {
                StartContext::Global
            })
        })
    }
}
/// Source-free admission projection; historical offers never authorize another start.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicDefinition {
    /// Exact name.
    pub name: String,
    /// Public description.
    pub description: String,
    /// Immutable revision identity.
    pub revision: String,
    /// Envelope API identity.
    pub api_version: String,
    /// Exact execution engine identity.
    pub engine: String,
    /// Pinned named descriptors.
    pub entrypoints: BTreeMap<String, Entrypoint>,
    /// Historical public offers.
    pub offers: BTreeMap<String, BTreeSet<StartContext>>,
    /// Input value contract.
    pub parameters: Contract,
    /// Output value contract.
    pub result: Contract,
    /// Declared ports, without live bindings.
    pub ports: BTreeMap<String, Port>,
    /// Required identities, not caller grants.
    pub capabilities: BTreeSet<String>,
    /// Pinned finite budgets.
    pub budgets: Budgets,
}
impl PublicDefinition {
    /// Capture once, excluding executable source and private runtime data.
    pub fn capture(entry: &CatalogEntry) -> Result<Self, PortError> {
        let definition = &entry.definition;
        let result = Self::from_envelope(
            definition.envelope().clone(),
            definition.revision().into(),
            definition.engine().into(),
            entry.offers.clone(),
        );
        if serde_json::to_vec(&result)
            .map_err(|_| PortError::Invalid)?
            .len()
            > 65536
        {
            return Err(PortError::Invalid);
        }
        Ok(result)
    }
    /// Older internal admissions have no public offers; never consult the live catalog.
    pub fn historical(revision: &crate::routine::history::Revision) -> Result<Self, PortError> {
        let envelope =
            serde_json::from_str(&revision.envelope).map_err(|_| PortError::Unavailable)?;
        Ok(Self::from_envelope(
            envelope,
            revision.id.clone(),
            revision.engine.clone(),
            BTreeMap::new(),
        ))
    }
    fn from_envelope(
        envelope: Envelope,
        revision: String,
        engine: String,
        offers: BTreeMap<String, BTreeSet<StartContext>>,
    ) -> Self {
        Self {
            name: envelope.name,
            description: envelope.description,
            revision,
            engine,
            api_version: envelope.api_version,
            entrypoints: envelope.spec.entrypoints,
            offers,
            parameters: envelope.spec.parameters,
            result: envelope.spec.result,
            ports: envelope.spec.ports,
            capabilities: envelope.spec.capabilities,
            budgets: envelope.spec.budgets,
        }
    }
}

/// One complete immutable bounded catalog replacement.
#[derive(Clone, Debug)]
pub struct CatalogSnapshot {
    /// Content revision including definition identity and start offers.
    pub revision: String,
    /// At most 256 exact named entries.
    pub entries: BTreeMap<String, CatalogEntry>,
    /// Host-supported identities, not grants.
    pub supported_capabilities: BTreeSet<String>,
}
/// Runtime/test-injectable complete-snapshot source; not a configuration engine.
pub trait Catalog: Send + Sync {
    /// Subscribe atomically, with the current replacement already available.
    fn subscribe(
        &self,
        principal: &str,
    ) -> Result<watch::Receiver<Arc<CatalogSnapshot>>, PortError>;
}
/// In-memory publisher with bounded replacement and no invocation/execution state.
#[derive(Debug)]
pub struct MemoryCatalog {
    sender: watch::Sender<Arc<CatalogSnapshot>>,
}
impl MemoryCatalog {
    /// Construct a validated complete catalog.
    pub fn new(
        entries: BTreeMap<String, CatalogEntry>,
        supported_capabilities: BTreeSet<String>,
    ) -> Result<Self, PortError> {
        let snapshot = build(entries, supported_capabilities)?;
        let (sender, _) = watch::channel(Arc::new(snapshot));
        Ok(Self { sender })
    }
    /// Atomically replace all offered definitions, including deletions.
    pub fn replace(
        &self,
        entries: BTreeMap<String, CatalogEntry>,
        supported_capabilities: BTreeSet<String>,
    ) -> Result<(), PortError> {
        self.sender
            .send_replace(Arc::new(build(entries, supported_capabilities)?));
        Ok(())
    }
}
impl Catalog for MemoryCatalog {
    fn subscribe(
        &self,
        principal: &str,
    ) -> Result<watch::Receiver<Arc<CatalogSnapshot>>, PortError> {
        if principal.is_empty() || principal.len() > 256 {
            return Err(PortError::Denied);
        }
        Ok(self.sender.subscribe())
    }
}
fn build(
    entries: BTreeMap<String, CatalogEntry>,
    supported_capabilities: BTreeSet<String>,
) -> Result<CatalogSnapshot, PortError> {
    if entries.len() > 256
        || supported_capabilities.len() > 64
        || supported_capabilities
            .iter()
            .any(|v| v.is_empty() || v.len() > 128)
    {
        return Err(PortError::Invalid);
    }
    let mut hash = Sha256::new();
    hash.update(b"waltr:routine-catalog:v1\0");
    let mut bytes = 0usize;
    for (name, entry) in &entries {
        let envelope = entry.definition.envelope();
        if name != &envelope.name || entry.offers.len() > 32 {
            return Err(PortError::Invalid);
        }
        PublicDefinition::capture(entry)?;
        bytes = bytes
            .checked_add(entry.definition.canonical().len())
            .ok_or(PortError::Invalid)?;
        if bytes > 2 * 1024 * 1024 {
            return Err(PortError::Invalid);
        }
        hash.update(entry.definition.revision().as_bytes());
        for (key, offers) in &entry.offers {
            let descriptor = envelope
                .spec
                .entrypoints
                .get(key)
                .ok_or(PortError::Invalid)?;
            if !offers.is_empty()
                && !matches!(descriptor.trigger, Trigger::Manual | Trigger::Interactive)
            {
                return Err(PortError::Invalid);
            }
            hash.update((key.len() as u64).to_be_bytes());
            hash.update(key.as_bytes());
            hash.update([
                u8::from(offers.contains(&StartContext::Global)),
                u8::from(offers.contains(&StartContext::Session)),
            ]);
        }
    }
    for cap in &supported_capabilities {
        hash.update((cap.len() as u64).to_be_bytes());
        hash.update(cap.as_bytes());
    }
    Ok(CatalogSnapshot {
        revision: super::hex(hash.finalize().as_slice()),
        entries,
        supported_capabilities,
    })
}

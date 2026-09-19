use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredProvenance {
    pub label: Label,
    pub influences: Influences,
}

impl StoredProvenance {
    pub fn legacy(unknown: bool) -> Self {
        if unknown { Self { label: [Source::UnknownPrivate].into(), influences: [Influence::Unknown].into() } }
        else { Self::default() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredContext {
    pub context: ContextId,
    pub provenance: StoredProvenance,
    pub clearance: Option<Label>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Metadata {
    pub version: u32,
    pub code: BTreeMap<String, StoredProvenance>,
    pub contexts: Vec<StoredContext>,
    pub grants: Vec<SharingGrant>,
    pub next_id: u64,
}

impl Default for Metadata {
    fn default() -> Self {
        Self { version: SCHEMA_VERSION, code: BTreeMap::new(), contexts: Vec::new(), grants: Vec::new(), next_id: 1 }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyMetadata {
    version: u32,
    policies: Vec<(Source, FlowPolicy)>,
    app_labels: BTreeMap<String, Label>,
    agent_labels: Vec<LegacyAgent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAgent {
    account: String,
    room: String,
    label: Label,
}

pub(super) fn decode(bytes: &[u8]) -> Result<Metadata, String> {
    let invalid = || "Information-flow metadata is unreadable or corrupt. Private data access is blocked.".to_string();
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(2) => serde_json::from_value(value).map_err(|_| invalid()),
        Some(1) => {
            let legacy: LegacyMetadata = serde_json::from_value(value).map_err(|_| invalid())?;
            if legacy.version != 1 { return Err(invalid()); }
            let mut metadata = Metadata::default();
            let mut seen = BTreeSet::new();
            for (source, policy) in legacy.policies {
                if !seen.insert(source.clone()) { return Err("Duplicate legacy sharing policy.".into()); }
                validate_source(&source)?;
                for recipient in policy.recipients {
                    let grant = SharingGrant { id: next_grant_id(&mut metadata)?, source: source.clone(), recipient,
                        reader: ReaderScope::AllReaders, duration: SharingDuration::Permanent };
                    sharing::validate_grant(&grant)?;
                    metadata.grants.push(grant);
                }
            }
            // Every old instance shared this storage. Its complete union is
            // inherited by all new compartments; no source can be recovered
            // by splitting the files. Old files remain in their original jail.
            for (app, label) in legacy.app_labels {
                metadata.code.insert(app, StoredProvenance { label, influences: [Influence::Unknown].into() });
            }
            for entry in legacy.agent_labels {
                metadata.contexts.push(StoredContext {
                    context: ContextId::Agent { account: entry.account, room: entry.room },
                    provenance: StoredProvenance { label: entry.label, influences: [Influence::Unknown].into() },
                    clearance: None,
                });
            }
            Ok(metadata)
        }
        _ => Err("Unsupported or incomplete information-flow metadata; private data access is blocked.".into()),
    }
}

pub(super) fn next_grant_id(metadata: &mut Metadata) -> Result<u64, String> {
    let id = metadata.next_id;
    if id == 0 || id >= (1 << 63) - 1 { return Err("Persistent sharing grant identities exhausted.".into()); }
    metadata.next_id += 1;
    Ok(id)
}

pub(super) fn context_path(root: &Path, context: &ContextId) -> PathBuf {
    // Serialized enum variants and fields keep public/private, account and
    // room identities distinct. SHA-256 avoids path-length and traversal bugs.
    let digest = Sha256::digest(serde_json::to_vec(context).expect("ContextId is serializable"));
    root.join("app_compartments").join(format!("{digest:x}"))
}

pub(super) fn effective_provenance(metadata: &Metadata, entry: &StoredContext) -> StoredProvenance {
    let mut provenance = entry.provenance.clone();
    if let Some(app) = entry.context.app() && let Some(code) = metadata.code.get(app) {
        provenance.label.extend(code.label.iter().cloned());
        provenance.influences.extend(code.influences.iter().cloned());
    }
    provenance
}

pub(super) fn validate_metadata(metadata: &Metadata) -> Result<(), String> {
    if metadata.version != SCHEMA_VERSION { return Err("Unsupported information-flow metadata version.".into()); }
    if metadata.next_id == 0 || metadata.next_id >= 1 << 63 { return Err("Invalid sharing grant counter.".into()); }
    for (app, provenance) in &metadata.code {
        validate_app(app)?;
        validate_provenance(provenance)?;
    }
    let mut contexts = BTreeSet::new();
    for entry in &metadata.contexts {
        validate_context(&entry.context)?;
        if !contexts.insert(&entry.context) { return Err("Duplicate compartment provenance.".into()); }
        if entry.context.app().is_some_and(|app| !metadata.code.contains_key(app)) {
            return Err("Missing app code provenance.".into());
        }
        validate_provenance(&entry.provenance)?;
        if let Some(clearance) = &entry.clearance { for source in clearance { validate_source(source)?; } }
        if matches!(entry.context, ContextId::PublicApp { .. }) && entry.clearance != Some(Label::new()) {
            return Err("Invalid public app clearance.".into());
        }
        check_clearance(&entry.context, entry.clearance.as_ref(), &entry.provenance.label)?;
    }
    let mut ids = BTreeSet::new();
    for grant in &metadata.grants {
        sharing::validate_grant(grant)?;
        if grant.duration != SharingDuration::Permanent || grant.id == 0 || grant.id >= metadata.next_id || !ids.insert(grant.id) {
            return Err("Invalid persistent sharing grant.".into());
        }
    }
    Ok(())
}

fn validate_provenance(provenance: &StoredProvenance) -> Result<(), String> {
    for source in &provenance.label { validate_source(source)?; }
    for influence in &provenance.influences { integrity::validate_influence(influence)?; }
    Ok(())
}

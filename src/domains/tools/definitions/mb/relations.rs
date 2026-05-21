//! MusicBrainz relations tool.
//!
//! Returns the relation graph for a MusicBrainz entity: producers, engineers,
//! cover versions, performers, songwriting credits, label-imprint chains,
//! collaborations, and so on. None of the existing search tools surface this
//! information; this tool is the agent's access point to the part of MB
//! that matters most for enrichment workflows.
//!
//! ### Design
//!
//! - One MB call per request. We `?inc=` every relation category in a single
//!   round-trip, then filter post-fetch on the caller's `kinds` list. A
//!   smarter "scope `?inc=` by `kinds`" optimisation would need a kind →
//!   target-entity-type mapping table and is deferred until the bandwidth
//!   actually matters.
//! - Stable wire format: the upstream [`RelationContent`] variant is
//!   flattened into a `target_type` string (lowercase, hyphenated — matches
//!   what MB itself prints in URLs). Dates flatten to plain strings.
//!   `relation_type` (the "kind") and `direction` are passed through
//!   verbatim.
//! - Reverse direction defaults to **on** because the common query
//!   ("who produced this release") looks for `direction="backward"`
//!   relations — switching it off filters them out.

use musicbrainz_rs::{
    Fetch,
    entity::{
        artist::Artist,
        label::Label,
        recording::Recording,
        relations::{Relation, RelationContent},
        release::Release,
        release_group::ReleaseGroup,
        work::Work,
    },
};
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{info, instrument};

use super::MbBlockingTool;
use super::common::{error_result, is_mbid, structured_result};

/// Hard cap on returned relations. MB's graph for a popular release can run
/// into the hundreds; past this the agent should narrow `kinds` instead of
/// asking for everything.
const MAX_RELATIONS: usize = 200;
/// Default cap when the caller doesn't pass one.
const DEFAULT_LIMIT: usize = 100;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Entity kinds we support as the *source* of a relation query.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelationEntityType {
    Artist,
    Release,
    ReleaseGroup,
    Recording,
    Work,
    Label,
}

impl RelationEntityType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Artist => "artist",
            Self::Release => "release",
            Self::ReleaseGroup => "release-group",
            Self::Recording => "recording",
            Self::Work => "work",
            Self::Label => "label",
        }
    }
}

/// Parameters for `mb_get_relations`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MbRelationsParams {
    /// The kind of entity to fetch relations for. Determines which MB
    /// endpoint we hit.
    pub entity_type: RelationEntityType,

    /// MBID of the source entity (UUID format).
    pub mbid: String,

    /// Optional whitelist of `relation_type` values to keep (case-sensitive
    /// — MB uses lowercase like `"producer"`, `"composer"`, `"cover"`,
    /// `"performer"`). When omitted, every relation MB returns comes
    /// through. Filtering is applied post-fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,

    /// When `true` (default), include relations whose direction is
    /// `"backward"` — i.e. relations pointing AT this entity. The common
    /// "who produced this release" query needs these; without them the
    /// release-side response would be empty.
    #[serde(default = "default_include_reverse")]
    pub include_reverse: bool,

    /// Maximum number of relations to return after filtering. Hard-capped
    /// at `MAX_RELATIONS = 200`.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_include_reverse() -> bool {
    true
}
fn default_limit() -> usize {
    DEFAULT_LIMIT
}

// ============================================================================
// Structured Output
// ============================================================================

/// One relation, in our stable wire format.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RelationInfo {
    /// Relation type name (e.g. `"producer"`, `"composer"`, `"cover"`,
    /// `"performer"`). Passed through verbatim from MB.
    pub kind: String,
    /// `"forward"` (this entity relates to target) or `"backward"`
    /// (target relates to this entity).
    pub direction: String,
    /// Target entity's category — `"artist"`, `"recording"`, `"release"`,
    /// `"release-group"`, `"work"`, `"label"`, `"place"`, `"area"`,
    /// `"event"`, `"series"`, `"url"`.
    pub target_type: String,
    /// Target entity's MBID.
    pub target_mbid: String,
    /// Target entity's display string — `.name` for most entities,
    /// `.title` for releases / recordings / works, `.resource` (URL)
    /// for URL targets.
    pub target_name: String,
    /// Modifiers attached to the relation (e.g. `["additional"]` for an
    /// "additional producer" relation). Empty / `None` for most entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<String>>,
    /// Begin date in MB's `YYYY[-MM[-DD]]` form, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub begin: Option<String>,
    /// End date in MB's `YYYY[-MM[-DD]]` form, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
}

/// Result of an `mb_get_relations` call.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RelationsResult {
    /// Echo of `entity_type`, normalised to the MB string form.
    pub entity_type: String,
    /// Echo of the source MBID.
    pub mbid: String,
    /// Display name of the source entity (artist.name / release.title /
    /// etc.) so the agent doesn't need a second lookup to know what it's
    /// looking at.
    pub entity_name: String,
    /// Filtered + capped relation list.
    pub relations: Vec<RelationInfo>,
    /// Total relations MB returned for this entity, before `kinds` /
    /// `include_reverse` / `limit` were applied.
    pub raw_count: usize,
    /// Number of relations after `kinds` + `include_reverse` filtering,
    /// before `limit` truncation.
    pub matched_count: usize,
    /// `true` when `matched_count > limit` and the response is truncated.
    pub truncated: bool,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Relations-graph tool.
pub struct MbRelationsTool;

impl MbBlockingTool for MbRelationsTool {
    type Params = MbRelationsParams;

    const NAME: &'static str = "mb_get_relations";

    const DESCRIPTION: &'static str = "Return the MusicBrainz relation graph for an entity (artist / release / \
         release_group / recording / work / label). Surfaces producers, engineers, performers, \
         cover versions, songwriting credits, label-imprint chains and every other relation MB \
         tracks — none of which the per-entity search tools expose. Filter by `kinds` (case-\
         sensitive MB relation-type names like 'producer' / 'composer' / 'cover'); set \
         `include_reverse=false` to hide backward relations. Hard-capped at 200 relations per \
         call.";

    #[instrument(skip_all, fields(entity = ?params.entity_type, mbid = %params.mbid))]
    fn execute(params: &MbRelationsParams) -> CallToolResult {
        if !is_mbid(&params.mbid) {
            return error_result(&format!(
                "Invalid MBID '{}' — must be a UUID (xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx)",
                params.mbid
            ));
        }
        let limit = params.limit.clamp(1, MAX_RELATIONS);

        info!(
            "mb_get_relations: entity_type={:?} mbid={} kinds={:?} include_reverse={} limit={}",
            params.entity_type, params.mbid, params.kinds, params.include_reverse, limit
        );

        // Fetch the source entity with every relation category in one
        // round-trip. Each branch returns (entity_name, relations).
        let (entity_name, relations) = match params.entity_type {
            RelationEntityType::Artist => match fetch_artist_with_all_rels(&params.mbid) {
                Ok(out) => out,
                Err(e) => return error_result(&e),
            },
            RelationEntityType::Release => match fetch_release_with_all_rels(&params.mbid) {
                Ok(out) => out,
                Err(e) => return error_result(&e),
            },
            RelationEntityType::ReleaseGroup => {
                match fetch_release_group_with_all_rels(&params.mbid) {
                    Ok(out) => out,
                    Err(e) => return error_result(&e),
                }
            }
            RelationEntityType::Recording => match fetch_recording_with_all_rels(&params.mbid) {
                Ok(out) => out,
                Err(e) => return error_result(&e),
            },
            RelationEntityType::Work => match fetch_work_with_all_rels(&params.mbid) {
                Ok(out) => out,
                Err(e) => return error_result(&e),
            },
            RelationEntityType::Label => match fetch_label_with_all_rels(&params.mbid) {
                Ok(out) => out,
                Err(e) => return error_result(&e),
            },
        };

        let raw_count = relations.len();

        // Apply caller filters (kinds + direction) post-fetch.
        let kinds_filter: Option<&[String]> = params.kinds.as_deref();
        let mut matched: Vec<RelationInfo> = relations
            .iter()
            .filter(|r| {
                if !params.include_reverse && r.direction == "backward" {
                    return false;
                }
                match kinds_filter {
                    Some(list) => list.iter().any(|k| k == &r.relation_type),
                    None => true,
                }
            })
            .filter_map(to_relation_info)
            .collect();

        let matched_count = matched.len();
        let truncated = matched_count > limit;
        if truncated {
            matched.truncate(limit);
        }

        let summary = if matched.is_empty() {
            format!(
                "No relations matched for {} '{}' ({} candidate(s) before filtering)",
                params.entity_type.as_str(),
                params.mbid,
                raw_count
            )
        } else {
            format!(
                "{} relation(s) for {} '{}' ({} raw / {} matched{})",
                matched.len(),
                params.entity_type.as_str(),
                entity_name,
                raw_count,
                matched_count,
                if truncated { ", truncated" } else { "" }
            )
        };

        let payload = RelationsResult {
            entity_type: params.entity_type.as_str().to_string(),
            mbid: params.mbid.clone(),
            entity_name,
            relations: matched,
            raw_count,
            matched_count,
            truncated,
        };
        structured_result(summary, payload)
    }
}

// ============================================================================
// Per-entity fetchers (each chains every with_*_relations builder)
// ============================================================================

/// Push every `with_*_relations` toggle onto an `&mut FetchQuery`. Macro
/// rather than a function because each entity has a distinct
/// `FetchQuery<Entity>` type — the methods are identical via the
/// `impl_relations_includes!` trait, so a macro is the cheapest way to
/// stay DRY without trait-object gymnastics.
macro_rules! enable_all_relations {
    ($fetch:expr) => {{
        $fetch.with_area_relations();
        $fetch.with_artist_relations();
        $fetch.with_event_relations();
        $fetch.with_genre_relations();
        $fetch.with_instrument_relations();
        $fetch.with_label_relations();
        $fetch.with_place_relations();
        $fetch.with_recording_relations();
        $fetch.with_release_relations();
        $fetch.with_release_group_relations();
        $fetch.with_series_relations();
        $fetch.with_url_relations();
        $fetch.with_work_relations();
    }};
}

fn fetch_artist_with_all_rels(mbid: &str) -> Result<(String, Vec<Relation>), String> {
    let mut fetch = Artist::fetch();
    fetch.id(mbid);
    enable_all_relations!(fetch);
    let entity = fetch
        .execute()
        .map_err(|e| format!("Artist relations fetch failed: {}", e))?;
    Ok((entity.name, entity.relations.unwrap_or_default()))
}

fn fetch_release_with_all_rels(mbid: &str) -> Result<(String, Vec<Relation>), String> {
    let mut fetch = Release::fetch();
    fetch.id(mbid);
    enable_all_relations!(fetch);
    let entity = fetch
        .execute()
        .map_err(|e| format!("Release relations fetch failed: {}", e))?;
    Ok((entity.title, entity.relations.unwrap_or_default()))
}

fn fetch_release_group_with_all_rels(mbid: &str) -> Result<(String, Vec<Relation>), String> {
    let mut fetch = ReleaseGroup::fetch();
    fetch.id(mbid);
    enable_all_relations!(fetch);
    let entity = fetch
        .execute()
        .map_err(|e| format!("ReleaseGroup relations fetch failed: {}", e))?;
    Ok((entity.title, entity.relations.unwrap_or_default()))
}

fn fetch_recording_with_all_rels(mbid: &str) -> Result<(String, Vec<Relation>), String> {
    let mut fetch = Recording::fetch();
    fetch.id(mbid);
    enable_all_relations!(fetch);
    let entity = fetch
        .execute()
        .map_err(|e| format!("Recording relations fetch failed: {}", e))?;
    Ok((entity.title, entity.relations.unwrap_or_default()))
}

fn fetch_work_with_all_rels(mbid: &str) -> Result<(String, Vec<Relation>), String> {
    let mut fetch = Work::fetch();
    fetch.id(mbid);
    enable_all_relations!(fetch);
    let entity = fetch
        .execute()
        .map_err(|e| format!("Work relations fetch failed: {}", e))?;
    Ok((entity.title, entity.relations.unwrap_or_default()))
}

fn fetch_label_with_all_rels(mbid: &str) -> Result<(String, Vec<Relation>), String> {
    let mut fetch = Label::fetch();
    fetch.id(mbid);
    enable_all_relations!(fetch);
    let entity = fetch
        .execute()
        .map_err(|e| format!("Label relations fetch failed: {}", e))?;
    Ok((entity.name, entity.relations.unwrap_or_default()))
}

// ============================================================================
// Relation → RelationInfo mapping
// ============================================================================

/// Extract `(target_type, target_mbid, target_name)` from a relation's
/// embedded entity. Returns `None` for variants that aren't fully
/// populated by the MB response (defensive — every documented variant
/// carries id + name/title, but a future upstream addition might not).
fn target_of(content: &RelationContent) -> Option<(&'static str, String, String)> {
    use RelationContent::*;
    Some(match content {
        Artist(a) => ("artist", a.id.clone(), a.name.clone()),
        Recording(r) => ("recording", r.id.clone(), r.title.clone()),
        Release(r) => ("release", r.id.clone(), r.title.clone()),
        ReleaseGroup(rg) => ("release-group", rg.id.clone(), rg.title.clone()),
        Work(w) => ("work", w.id.clone(), w.title.clone()),
        Label(l) => ("label", l.id.clone(), l.name.clone()),
        Place(p) => ("place", p.id.clone(), p.name.clone()),
        Area(a) => ("area", a.id.clone(), a.name.clone()),
        Event(e) => ("event", e.id.clone(), e.name.clone()),
        Series(s) => ("series", s.id.clone(), s.name.clone()),
        Url(u) => ("url", u.id.clone(), u.resource.clone()),
    })
}

fn to_relation_info(rel: &Relation) -> Option<RelationInfo> {
    let (target_type, target_mbid, target_name) = target_of(&rel.content)?;
    let attributes = rel.attributes.clone().filter(|list| !list.is_empty());
    Some(RelationInfo {
        kind: rel.relation_type.clone(),
        direction: rel.direction.clone(),
        target_type: target_type.to_string(),
        target_mbid,
        target_name,
        attributes,
        begin: rel.begin.as_ref().map(|d| d.0.clone()),
        end: rel.end.as_ref().map(|d| d.0.clone()),
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_type_normalises_to_mb_strings() {
        assert_eq!(RelationEntityType::Artist.as_str(), "artist");
        assert_eq!(RelationEntityType::Release.as_str(), "release");
        // Hyphen, matching MB's URL form.
        assert_eq!(RelationEntityType::ReleaseGroup.as_str(), "release-group");
        assert_eq!(RelationEntityType::Recording.as_str(), "recording");
        assert_eq!(RelationEntityType::Work.as_str(), "work");
        assert_eq!(RelationEntityType::Label.as_str(), "label");
    }

    #[test]
    fn params_parse_defaults() {
        let v = serde_json::json!({
            "entity_type": "release",
            "mbid": "18079f7b-78c3-3980-b16e-c5db63cc10a5"
        });
        let p: MbRelationsParams = serde_json::from_value(v).unwrap();
        assert!(matches!(p.entity_type, RelationEntityType::Release));
        assert!(p.kinds.is_none());
        // include_reverse defaults to true (the common case).
        assert!(p.include_reverse);
        assert_eq!(p.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn params_parse_release_group_with_hyphenated_snake_case() {
        // "release_group" (snake_case in JSON) → ReleaseGroup variant.
        let v = serde_json::json!({
            "entity_type": "release_group",
            "mbid": "18079f7b-78c3-3980-b16e-c5db63cc10a5"
        });
        let p: MbRelationsParams = serde_json::from_value(v).unwrap();
        assert!(matches!(p.entity_type, RelationEntityType::ReleaseGroup));
    }

    #[test]
    fn invalid_mbid_refused_before_network() {
        let r = MbRelationsTool::execute(&MbRelationsParams {
            entity_type: RelationEntityType::Artist,
            mbid: "not-a-uuid".to_string(),
            kinds: None,
            include_reverse: true,
            limit: 100,
        });
        assert!(r.is_error.unwrap_or(false));
    }

    #[test]
    fn target_of_extracts_artist_id_and_name() {
        use musicbrainz_rs::entity::artist::Artist;
        let artist = Artist {
            id: "a74b1b7f-71a5-4011-9441-d0b5e4122711".to_string(),
            name: "Radiohead".to_string(),
            ..Default::default()
        };
        let content = RelationContent::Artist(Box::new(artist));
        let (ty, mbid, name) = target_of(&content).unwrap();
        assert_eq!(ty, "artist");
        assert_eq!(mbid, "a74b1b7f-71a5-4011-9441-d0b5e4122711");
        assert_eq!(name, "Radiohead");
    }

    #[test]
    fn target_of_extracts_url_resource_as_name() {
        use musicbrainz_rs::entity::url::Url;
        // Url doesn't derive Default upstream — construct explicitly.
        let url = Url {
            id: "00000000-0000-0000-0000-000000000000".to_string(),
            resource: "https://example.com/foo".to_string(),
            tags: None,
            relations: None,
        };
        let content = RelationContent::Url(Box::new(url));
        let (ty, _mbid, name) = target_of(&content).unwrap();
        assert_eq!(ty, "url");
        // For URL targets the "name" surfaces the resource string.
        assert_eq!(name, "https://example.com/foo");
    }
}

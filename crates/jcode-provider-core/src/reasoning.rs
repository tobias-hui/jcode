//! Shared reasoning-effort ladders.
//!
//! Keep these in provider-core so provider runtimes and UI clients expose the
//! same ordered values. `swarm` and `swarm-deep` are Jcode UI sentinels rather
//! than wire-level provider values, but they belong in the selectable ladder.

/// A parsed `[provider] model_picker_hidden` entry.
///
/// Grammar (case-insensitive, whitespace trimmed):
/// - `model` — bare model id: drops every route for that id (original behavior).
/// - `lane:model` — lane prefix selects the API method: one of the dual-auth
///   tokens (`openai-api-key`, `openai-oauth`, `anthropic-api-key`,
///   `claude-oauth`), `openrouter`, an openai-compatible profile id (`kimi`,
///   `qwencloud`, or the full `openai-compatible:kimi` form), or a provider
///   label (`openai`, `kimi code`). Only routes whose lane matches are dropped.
/// - `lane:model:effort[,effort...]` — additionally prunes only the picker
///   rows expanded for those reasoning efforts. Bare `model:effort` (no lane)
///   applies the effort filter to every lane of the model. Effort accepts
///   both wire values and picker labels (`medium`/`med`, `xhigh`, `none`, ...).
/// - `lane:model:!effort[,effort...]` — the negated form prunes every effort
///   row *except* the listed ones, so "keep only `med`" is one entry that
///   stays correct when a provider's effort ladder changes in a release.
///
/// A model id can itself contain `:` (e.g. ollama `model:tag`), so a colon is
/// only treated as a lane separator when the prefix is a known lane (the
/// fixed tokens above plus the caller-supplied configured profile ids);
/// otherwise the whole string stays the bare model id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenPickerRule {
    pub model: String,
    /// Normalized lane selector (see [`normalize_hidden_picker_lane`]), if any.
    pub lane: Option<String>,
    /// Canonical reasoning efforts (`medium` not `med`); empty = no effort
    /// constraint. When non-empty, the rule matches rows whose effort is in
    /// the list (or, if [`Self::efforts_negated`], whose effort is not).
    pub efforts: Vec<String>,
    /// True when `efforts` lists the rows to *keep* (the `!med` form).
    pub efforts_negated: bool,
}

/// Normalize a hidden-rule lane selector to a comparable key: lowercase with
/// every non-alphanumeric character removed (`openai-compatible:kimi` and
/// `kimi` both normalize to `openaicompatiblekimi`).
pub fn normalize_hidden_picker_lane(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Lane tokens that are always recognized without config context.
const FIXED_HIDDEN_LANES: &[&str] = &[
    "openai-api-key",
    "openai-oauth",
    "anthropic-api-key",
    "claude-oauth",
    "openrouter",
    "openai-compatible",
];

/// True when a raw colon-prefix segment names a lane: a fixed token or one of
/// the caller-supplied configured lane/profile ids (both normalized).
fn hidden_picker_segment_is_lane(segment: &str, configured_lanes: &[String]) -> bool {
    let normalized = normalize_hidden_picker_lane(segment);
    if normalized.is_empty() {
        return false;
    }
    FIXED_HIDDEN_LANES
        .iter()
        .any(|lane| normalize_hidden_picker_lane(lane) == normalized)
        || configured_lanes.iter().any(|lane| lane == &normalized)
}

/// Parse one raw `model_picker_hidden` entry. `configured_lanes` must be
/// normalized (see [`normalize_hidden_picker_lane`]). Returns `None` for empty
/// or whitespace-only input.
pub fn parse_hidden_picker_rule(
    entry: &str,
    configured_lanes: &[String],
) -> Option<HiddenPickerRule> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    let lower = entry.to_ascii_lowercase();
    let segments: Vec<&str> = lower.split(':').map(|s| s.trim()).collect();
    let mut lane: Option<String> = None;
    let mut efforts: Vec<String> = Vec::new();
    let mut efforts_negated = false;
    let mut model = lower.clone();
    if segments.len() >= 2 {
        // Trailing effort segment: `...:med`, `...:low,high`, or `...:!med`.
        if let Some((list, negated)) =
            parse_hidden_effort_segment(segments.last().copied().unwrap_or_default())
        {
            efforts = list;
            efforts_negated = negated;
            model = segments[..segments.len() - 1].join(":");
        }
    }
    if let Some((prefix, rest)) = model.split_once(':') {
        let prefix = prefix.to_string();
        let rest = rest.trim().to_string();
        if !rest.is_empty() && hidden_picker_segment_is_lane(&prefix, configured_lanes) {
            if normalize_hidden_picker_lane(&prefix) == "openaicompatible" {
                // Full form `openai-compatible:<profile>:<model>`: absorb the
                // profile segment into the lane only when it is a configured
                // profile id, so ids like `model:tag` stay unambiguous.
                if let Some((profile, model_id)) = rest.split_once(':') {
                    let profile_norm = normalize_hidden_picker_lane(profile.trim());
                    let model_id = model_id.trim();
                    if !profile_norm.is_empty()
                        && !model_id.is_empty()
                        && configured_lanes.contains(&profile_norm)
                    {
                        lane = Some(format!("openaicompatible{profile_norm}"));
                        model = model_id.to_string();
                    }
                }
            }
            if lane.is_none() {
                lane = Some(normalize_hidden_picker_lane(&prefix));
                model = rest;
            }
        }
    }
    let model = model.trim().to_string();
    if model.is_empty() {
        return None;
    }
    Some(HiddenPickerRule {
        model,
        lane,
        efforts,
        efforts_negated,
    })
}

/// Parse a trailing effort segment into a canonical list, or `None` when the
/// segment is not an effort list (so it stays part of the model id). Accepts
/// an optional leading `!` negation marker and comma-separated values.
fn parse_hidden_effort_segment(segment: &str) -> Option<(Vec<String>, bool)> {
    let (segment, negated) = match segment.strip_prefix('!') {
        Some(rest) => (rest, true),
        None => (segment, false),
    };
    if segment.is_empty() {
        return None;
    }
    let mut list = Vec::new();
    for part in segment.split(',') {
        let part = part.trim();
        let canonical = if part == "med" {
            Some("medium")
        } else {
            canonical_reasoning_effort(part)
        };
        list.push(canonical?.to_string());
    }
    Some((list, negated))
}

/// Parse all raw `model_picker_hidden` entries, dropping empty ones.
pub fn parse_hidden_picker_rules<'a>(
    entries: impl IntoIterator<Item = &'a str>,
    configured_lanes: &[String],
) -> Vec<HiddenPickerRule> {
    entries
        .into_iter()
        .filter_map(|entry| parse_hidden_picker_rule(entry, configured_lanes))
        .collect()
}

/// True when a route's api_method/provider pair matches a normalized lane
/// selector: the normalized api_method (`openaicompatiblekimi`), its bare
/// profile id (`kimi`), a provider label (`kimi code`, `openai`), or the
/// dual-auth tokens where api_method and lane vocabulary coincide.
pub fn hidden_picker_lane_matches_route(lane: &str, api_method: &str, provider: &str) -> bool {
    let api_norm = normalize_hidden_picker_lane(api_method);
    if api_norm == lane {
        return true;
    }
    if lane == "openaicompatible" && api_norm.starts_with("openaicompatible") {
        // Bare `openai-compatible:` lane drops every named profile route.
        return true;
    }
    if let Some((_, profile)) = api_method.split_once(':') {
        if normalize_hidden_picker_lane(profile) == lane {
            return true;
        }
    }
    crate::model_route_provider_labels_match(provider, lane)
}

/// True when a hidden rule selects this (route, effort) picker row.
/// `effort` is the canonical reasoning effort for expanded rows, or `None`
/// for plain (non-expanded) rows. A rule without an effort constraint
/// matches rows in every state; a rule *with* efforts matches only expanded
/// rows whose effort is selected by the list (negated lists select the rows
/// to keep, so they match everything *not* in the list).
pub fn hidden_picker_rule_matches_route(
    rule: &HiddenPickerRule,
    route: &crate::ModelRoute,
    effort: Option<&str>,
) -> bool {
    hidden_picker_rule_matches(
        rule,
        &route.model,
        &route.provider,
        &route.api_method,
        effort,
    )
}

/// String-field variant of [`hidden_picker_rule_matches_route`] for callers
/// (like the TUI picker) that hold route fields rather than a `ModelRoute`.
pub fn hidden_picker_rule_matches(
    rule: &HiddenPickerRule,
    model: &str,
    provider: &str,
    api_method: &str,
    effort: Option<&str>,
) -> bool {
    if rule.model != model.trim().to_ascii_lowercase() {
        return false;
    }
    if let Some(lane) = &rule.lane
        && !hidden_picker_lane_matches_route(lane, api_method, provider)
    {
        return false;
    }
    if !rule.efforts.is_empty() {
        // Effort-scoped rules only ever prune expanded effort rows: the plain
        // (unexpanded) route row must survive so the kept effort row can still
        // be rendered, and route-level filters never see effort rows at all.
        let Some(effort) = effort else { return false };
        let listed = rule.efforts.iter().any(|e| e == effort);
        return if rule.efforts_negated {
            !listed
        } else {
            listed
        };
    }
    true
}

/// OpenAI Responses API effort levels, followed by Jcode's swarm modes.
pub const OPENAI_SELECTABLE_EFFORTS: &[&str] = &[
    "none",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "swarm",
    "swarm-deep",
];

/// OpenRouter's unified reasoning effort levels.
///
/// OpenRouter currently treats `max` as an alias for `xhigh`, so it is not a
/// separate rung in this ladder.
pub const OPENROUTER_SELECTABLE_EFFORTS: &[&str] = &[
    "none",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "swarm",
    "swarm-deep",
];

/// Direct DeepSeek effort levels, followed by Jcode's swarm modes.
pub const DEEPSEEK_SELECTABLE_EFFORTS: &[&str] = &[
    "none",
    "low",
    "medium",
    "high",
    "max",
    "swarm",
    "swarm-deep",
];

/// Moonshot Kimi Code coding-plan effort levels (docs: Model Configuration).
///
/// Both K3 and K2.8 Preview accept `reasoning_effort: low | high | max` and
/// return HTTP 400 for any other value. `medium` is not a wire value (the
/// endpoint maps it to `high`), and `none` disables thinking server-side, so
/// both stay selectable in the UX ladder while the request builder does the
/// mapping. Without an explicit effort no field is sent and the endpoint
/// applies its native default (high for K3, max for K2.8 Preview).
pub const KIMI_SELECTABLE_EFFORTS: &[&str] = &[
    "none",
    "low",
    "medium",
    "high",
    "max",
    "swarm",
    "swarm-deep",
];

/// Convert a provider-advertised OpenAI/OpenRouter effort into the canonical
/// static value used by the provider trait.
pub fn canonical_reasoning_effort(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" => Some("none"),
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
}

/// Infer the selectable effort ladder when only provider/model identity is
/// available, such as in a remote TUI session.
pub fn inferred_reasoning_efforts(
    provider_name: Option<&str>,
    model_name: Option<&str>,
) -> Vec<&'static str> {
    let provider = provider_name.unwrap_or_default().to_ascii_lowercase();
    let model = model_name.unwrap_or_default().to_ascii_lowercase();

    if provider.contains("openrouter") {
        return OPENROUTER_SELECTABLE_EFFORTS.to_vec();
    }

    if provider.contains("deepseek") || model.contains("deepseek") {
        return DEEPSEEK_SELECTABLE_EFFORTS.to_vec();
    }

    if provider.contains("kimi") || model.contains("kimi") {
        return KIMI_SELECTABLE_EFFORTS.to_vec();
    }

    if provider.contains("z.ai") || provider == "zai" || model.starts_with("glm-") {
        return OPENAI_SELECTABLE_EFFORTS.to_vec();
    }

    let is_openai_model = model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.starts_with("o5");
    if provider.contains("openai-compatible") {
        return if is_openai_model {
            OPENAI_SELECTABLE_EFFORTS.to_vec()
        } else {
            Vec::new()
        };
    }

    let is_anthropic = provider.contains("anthropic")
        || provider.contains("claude")
        || model.starts_with("claude-");
    if is_anthropic {
        let caps = crate::anthropic_reasoning_caps(&model);
        if !caps.supports_reasoning_effort() {
            return Vec::new();
        }
        let mut efforts = vec!["none", "low", "medium", "high"];
        if caps.xhigh_effort {
            efforts.push("xhigh");
        }
        if caps.max_effort {
            efforts.push("max");
        }
        efforts.extend(["swarm", "swarm-deep"]);
        return efforts;
    }

    let is_openai = provider.contains("openai") || provider.contains("codex") || is_openai_model;
    if is_openai {
        return OPENAI_SELECTABLE_EFFORTS.to_vec();
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lanes(configured: &[&str]) -> Vec<String> {
        configured
            .iter()
            .map(|l| normalize_hidden_picker_lane(l))
            .collect()
    }

    #[test]
    fn hidden_picker_rules_parse_bare_lane_and_effort() {
        let configured = lanes(&["kimi", "qwencloud", "glm", "deepseek-vision"]);

        let bare = parse_hidden_picker_rule("gpt-reserve", &configured).unwrap();
        assert_eq!(bare.model, "gpt-reserve");
        assert_eq!(bare.lane, None);
        assert!(bare.efforts.is_empty());

        let laned = parse_hidden_picker_rule("openai-api-key:GPT-5.6-Luna", &configured).unwrap();
        assert_eq!(laned.model, "gpt-5.6-luna");
        assert_eq!(laned.lane.as_deref(), Some("openaiapikey"));
        assert!(laned.efforts.is_empty());

        let eff = parse_hidden_picker_rule("openai-oauth:gpt-5.6-luna:med", &configured).unwrap();
        assert_eq!(eff.lane.as_deref(), Some("openaioauth"));
        assert_eq!(eff.efforts, vec!["medium".to_string()]);
        assert!(!eff.efforts_negated);

        let keep =
            parse_hidden_picker_rule("openai-api-key:gpt-6-astra:!med", &configured).unwrap();
        assert_eq!(keep.efforts, vec!["medium".to_string()]);
        assert!(keep.efforts_negated);

        let list = parse_hidden_picker_rule("gpt-6-astra:none,minimal", &configured).unwrap();
        assert_eq!(
            list.efforts,
            vec!["none".to_string(), "minimal".to_string()]
        );
        assert!(list.lane.is_none());

        // Not an effort segment -> stays part of the model id.
        let not_effort = parse_hidden_picker_rule("handy-proofreader:latest", &configured).unwrap();
        assert!(not_effort.efforts.is_empty());

        let profile = parse_hidden_picker_rule("kimi:k3-256k", &configured).unwrap();
        assert_eq!(profile.lane.as_deref(), Some("kimi"));
        assert_eq!(profile.model, "k3-256k");

        // Unknown prefixes (ollama model:tag) stay bare model ids.
        let tagged = parse_hidden_picker_rule("handy-proofreader:latest", &configured).unwrap();
        assert_eq!(tagged.model, "handy-proofreader:latest");
        assert_eq!(tagged.lane, None);
        assert!(tagged.efforts.is_empty());

        assert!(parse_hidden_picker_rule("", &configured).is_none());
        assert!(parse_hidden_picker_rule("   ", &configured).is_none());
    }

    #[test]
    fn hidden_picker_rule_matching_respects_lane_and_effort() {
        let configured = lanes(&["kimi"]);
        let route = |model: &str, provider: &str, api_method: &str| crate::ModelRoute {
            model: model.to_string(),
            provider: provider.to_string(),
            api_method: api_method.to_string(),
            available: true,
            detail: String::new(),
            cheapness: None,
        };
        let api_key = route("gpt-6-astra", "OpenAI", "openai-api-key");
        let oauth = route("gpt-6-astra", "OpenAI", "openai-oauth");
        let kimi = route("k3", "Kimi Code", "openai-compatible:kimi");

        let bare = parse_hidden_picker_rule("gpt-6-astra", &configured).unwrap();
        assert!(hidden_picker_rule_matches_route(
            &bare,
            &api_key,
            Some("medium")
        ));
        assert!(hidden_picker_rule_matches_route(&bare, &oauth, None));

        let api_only = parse_hidden_picker_rule("openai-api-key:gpt-6-astra", &configured).unwrap();
        assert!(hidden_picker_rule_matches_route(
            &api_only,
            &api_key,
            Some("low")
        ));
        assert!(!hidden_picker_rule_matches_route(
            &api_only,
            &oauth,
            Some("low")
        ));

        let med_only =
            parse_hidden_picker_rule("openai-api-key:gpt-6-astra:med", &configured).unwrap();
        assert!(hidden_picker_rule_matches_route(
            &med_only,
            &api_key,
            Some("medium")
        ));
        assert!(!hidden_picker_rule_matches_route(
            &med_only,
            &api_key,
            Some("low")
        ));
        // Effort-scoped rules must not drop the plain (unexpanded) route row.
        assert!(!hidden_picker_rule_matches_route(&med_only, &api_key, None));

        let keep_med =
            parse_hidden_picker_rule("openai-api-key:gpt-6-astra:!med", &configured).unwrap();
        assert!(!hidden_picker_rule_matches_route(
            &keep_med,
            &api_key,
            Some("medium")
        ));
        assert!(hidden_picker_rule_matches_route(
            &keep_med,
            &api_key,
            Some("low")
        ));
        assert!(hidden_picker_rule_matches_route(
            &keep_med,
            &api_key,
            Some("max")
        ));
        assert!(!hidden_picker_rule_matches_route(
            &keep_med,
            &oauth,
            Some("low")
        ));
        // Negated rules must also spare the plain route row (the kept row's host).
        assert!(!hidden_picker_rule_matches_route(&keep_med, &api_key, None));

        let profile = parse_hidden_picker_rule("kimi:k3", &configured).unwrap();
        assert!(hidden_picker_rule_matches_route(&profile, &kimi, None));
        let full_form = parse_hidden_picker_rule("openai-compatible:kimi:k3", &configured).unwrap();
        assert!(hidden_picker_rule_matches_route(&full_form, &kimi, None));
    }

    #[test]
    fn provider_ladders_preserve_distinct_max_semantics() {
        assert_eq!(
            inferred_reasoning_efforts(Some("openai"), Some("gpt-5.4")),
            OPENAI_SELECTABLE_EFFORTS
        );
        assert!(OPENAI_SELECTABLE_EFFORTS.contains(&"max"));
        assert!(OPENAI_SELECTABLE_EFFORTS.contains(&"minimal"));
        assert!(OPENROUTER_SELECTABLE_EFFORTS.contains(&"minimal"));
        assert!(!OPENROUTER_SELECTABLE_EFFORTS.contains(&"max"));
        assert!(DEEPSEEK_SELECTABLE_EFFORTS.contains(&"max"));
        assert_eq!(
            inferred_reasoning_efforts(Some("openai-compatible:custom"), Some("gpt-5.6")),
            OPENAI_SELECTABLE_EFFORTS,
            "direct OpenAI-compatible runtimes use the OpenAI reasoning_effort vocabulary"
        );
    }

    #[test]
    fn zai_and_glm_identities_expose_openai_efforts() {
        assert_eq!(
            inferred_reasoning_efforts(Some("Z.AI"), Some("glm-5.3")),
            OPENAI_SELECTABLE_EFFORTS
        );
        assert_eq!(
            inferred_reasoning_efforts(Some("openai-compatible:zai"), Some("glm-5.3-flash")),
            OPENAI_SELECTABLE_EFFORTS
        );
    }

    #[test]
    fn anthropic_ladder_comes_from_model_capabilities() {
        assert_eq!(
            inferred_reasoning_efforts(Some("anthropic"), Some("claude-sonnet-4-6")),
            vec![
                "none",
                "low",
                "medium",
                "high",
                "max",
                "swarm",
                "swarm-deep"
            ]
        );
        assert_eq!(
            inferred_reasoning_efforts(Some("anthropic"), Some("claude-opus-4-7")),
            vec![
                "none",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "swarm",
                "swarm-deep"
            ]
        );
    }
}

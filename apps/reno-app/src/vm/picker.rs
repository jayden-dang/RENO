//! The composer's picker, as plain data: the tabs of its rail, what a search lists on each, and
//! the effort track. The popover draws what this returns.
//!
//! Ported from zeron's `crates/ui/src/pickers.rs` (`scoped_model_rows`, `default_reasoning`,
//! `clamp_reasoning`) and `popover::match_rank`, onto Reno's types: a harness is an
//! [`ExecutorId`], a model is what that harness's CLI reported ([`models::ModelChoice`]).

use reno_proto::{Effort, ExecutorId};

use super::models::{HarnessChoice, ModelChoice, Pick};

/// How many matches from other harnesses a search adds under "In other harnesses".
pub(crate) const OTHER_HARNESSES: usize = 3;

/// One tab of the picker's rail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    /// The starred models of every harness.
    Favourites,
    /// Reno picks (Home only).
    Auto,
    /// One harness's models.
    Harness(ExecutorId),
}

impl Tab {
    /// A stable word for debug selectors: `favourites`, `auto`, the harness id.
    pub(crate) fn key(&self) -> String {
        match self {
            Self::Favourites => "favourites".to_owned(),
            Self::Auto => "auto".to_owned(),
            Self::Harness(id) => id.to_string(),
        }
    }
}

/// A tab with what the rail says about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TabView {
    pub(crate) tab: Tab,
    /// Shown on hover and on keyboard focus.
    pub(crate) name: String,
    /// A warning dot: the harness is signed out.
    pub(crate) signed_out: bool,
    /// The harness reported no models: its logo is dimmed.
    pub(crate) empty: bool,
}

/// The rail: Favourites, Auto (Home only), then each harness in the order given. A live run has
/// only its own harness (`live`), as a run cannot move to another.
pub(crate) fn tabs(
    harnesses: &[HarnessChoice],
    live: Option<&ExecutorId>,
    home: bool,
) -> Vec<TabView> {
    let harness = |h: &HarnessChoice| TabView {
        tab: Tab::Harness(h.id.clone()),
        name: h.name.clone(),
        signed_out: !h.usable,
        empty: h.models.is_empty(),
    };
    if let Some(live) = live {
        return harnesses
            .iter()
            .filter(|h| &h.id == live)
            .map(harness)
            .collect();
    }
    let fixed = |tab, name: &str| TabView {
        tab,
        name: name.to_owned(),
        signed_out: false,
        empty: false,
    };
    let mut all = vec![fixed(Tab::Favourites, "Favourites")];
    if home {
        all.push(fixed(Tab::Auto, "Auto"));
    }
    all.extend(harnesses.iter().map(harness));
    all
}

/// The tab the picker opens on: the shown pick's harness, else Auto on Home, else Favourites.
pub(crate) fn opening_tab(
    shown: Option<&Pick>,
    harnesses: &[HarnessChoice],
    live: Option<&ExecutorId>,
    home: bool,
) -> Tab {
    if let Some(live) = live {
        return Tab::Harness(live.clone());
    }
    match shown {
        Some(pick) if harnesses.iter().any(|h| h.id == pick.executor) => {
            Tab::Harness(pick.executor.clone())
        }
        _ if home => Tab::Auto,
        _ => Tab::Favourites,
    }
}

/// How well `label` matches `query`: 0 a prefix, 1 anywhere in it, `None` not at all. A blank
/// query matches everything at 1. Case does not matter.
pub(crate) fn match_rank(query: &str, label: &str) -> Option<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(1);
    }
    let label = label.to_lowercase();
    if label.starts_with(&query) {
        Some(0)
    } else if label.contains(&query) {
        Some(1)
    } else {
        None
    }
}

/// One model row of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) harness: ExecutorId,
    pub(crate) harness_name: String,
    /// A task sent to this harness would start (it is not signed out).
    pub(crate) usable: bool,
    pub(crate) model: ModelChoice,
    pub(crate) starred: bool,
}

/// What the list shows for a tab and a search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Listing {
    /// The viewed tab's rows.
    pub(crate) rows: Vec<Row>,
    /// With a query on a harness tab: up to [`OTHER_HARNESSES`] matches from the other harnesses.
    pub(crate) others: Vec<Row>,
}

/// The rows of one tab. THE QUERY NEVER LEAVES THE VIEWED TAB: on a harness tab it ranks that
/// harness's models, on Favourites the starred ones (zeron's `scoped_model_rows`); the matches
/// from elsewhere come back separately as [`Listing::others`]. A rank is: label prefix, label
/// substring, then a hit in the description (kept in the haystack so a provider named only in
/// the description still finds its models); stars, then the CLI's order, break ties. Without a
/// query a harness tab lists its models stars first and Favourites lists every star.
pub(crate) fn listing(
    query: &str,
    tab: &Tab,
    harnesses: &[HarnessChoice],
    is_favourite: impl Fn(&ExecutorId, &str) -> bool,
) -> Listing {
    let row = |h: &HarnessChoice, m: &ModelChoice| Row {
        harness: h.id.clone(),
        harness_name: h.name.clone(),
        usable: h.usable,
        model: m.clone(),
        starred: is_favourite(&h.id, &m.id),
    };
    let in_scope = |h: &HarnessChoice, m: &ModelChoice| match tab {
        Tab::Favourites => is_favourite(&h.id, &m.id),
        Tab::Harness(id) => &h.id == id,
        Tab::Auto => false,
    };
    let query = query.trim();
    if query.is_empty() {
        let mut rows: Vec<Row> = harnesses
            .iter()
            .flat_map(|h| h.models.iter().map(move |m| (h, m)))
            .filter(|(h, m)| in_scope(h, m))
            .map(|(h, m)| row(h, m))
            .collect();
        if matches!(tab, Tab::Harness(_)) {
            // Stable: the CLI's order within the stars and within the rest.
            rows.sort_by_key(|r| !r.starred);
        }
        return Listing {
            rows,
            others: Vec::new(),
        };
    }
    let rank_of = |m: &ModelChoice| {
        let by_label = match_rank(query, &m.name);
        let by_description = match_rank(
            query,
            &format!("{} {}", m.description.as_deref().unwrap_or(""), m.name),
        )
        .map(|rank| rank + 2);
        by_label.into_iter().chain(by_description).min()
    };
    let mut ranked: Vec<(usize, bool, usize, Row)> = Vec::new();
    let mut others: Vec<(usize, bool, usize, Row)> = Vec::new();
    let mut at = 0;
    for h in harnesses {
        for m in &h.models {
            at += 1;
            let Some(rank) = rank_of(m) else { continue };
            let entry = (rank, !is_favourite(&h.id, &m.id), at, row(h, m));
            if in_scope(h, m) {
                ranked.push(entry);
            } else if matches!(tab, Tab::Harness(_)) {
                others.push(entry);
            }
        }
    }
    let rows = |mut v: Vec<(usize, bool, usize, Row)>| {
        v.sort_by_key(|(rank, unstarred, at, _)| (*rank, *unstarred, *at));
        v.into_iter().map(|(.., row)| row).collect::<Vec<_>>()
    };
    let mut others = rows(others);
    others.truncate(OTHER_HARNESSES);
    Listing {
        rows: rows(ranked),
        others,
    }
}

// ---- effort -----------------------------------------------------------------------------------

/// A level as the picker words it ("X-High").
pub(crate) const fn effort_label(effort: Effort) -> &'static str {
    match effort {
        Effort::Minimal => "Minimal",
        Effort::Low => "Low",
        Effort::Medium => "Medium",
        Effort::High => "High",
        Effort::XHigh => "X-High",
        Effort::Max => "Max",
    }
}

/// The effort words beside "EFFORT" and on the chip: the level, or "Default · High" when none is
/// chosen and the model has a default, or "Default".
pub(crate) fn effort_words(chosen: Option<Effort>, default: Option<Effort>) -> String {
    match (chosen, default) {
        (Some(level), _) => effort_label(level).to_owned(),
        (None, Some(level)) => format!("Default \u{b7} {}", effort_label(level)),
        (None, None) => "Default".to_owned(),
    }
}

/// One cell of the effort track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EffortCell {
    pub(crate) effort: Effort,
    /// At or below the chosen level.
    pub(crate) filled: bool,
    /// The model's own level, ringed while none is chosen.
    pub(crate) default: bool,
    /// How strong the fill is, from 0.35 at the first cell to 1.0 at the last, so the track
    /// deepens toward the model's most.
    pub(crate) strength: f32,
}

/// The cells for a model's levels (least to most) and the chosen one (`None`: its default).
pub(crate) fn effort_cells(
    levels: &[Effort],
    chosen: Option<Effort>,
    default: Option<Effort>,
) -> Vec<EffortCell> {
    let last = levels.len().saturating_sub(1).max(1) as f32;
    levels
        .iter()
        .enumerate()
        .map(|(at, &effort)| EffortCell {
            effort,
            filled: chosen.is_some_and(|c| effort <= c),
            default: chosen.is_none() && default == Some(effort),
            strength: 0.35 + 0.65 * (at as f32 / last),
        })
        .collect()
}

/// The effort after clicking a cell: that level, or back to the default when it was chosen.
pub(crate) fn next_effort(chosen: Option<Effort>, clicked: Effort) -> Option<Effort> {
    (chosen != Some(clicked)).then_some(clicked)
}

/// Keeps `level` when the model lists it; otherwise `None`, which is the model's own default
/// (zeron's `clamp_reasoning`, except that Reno leaves the default to the CLI rather than
/// naming one it does not know).
pub(crate) fn clamp_effort(level: Option<Effort>, levels: &[Effort]) -> Option<Effort> {
    level.filter(|l| levels.contains(l))
}

/// The effort a model starts with when it is picked: the one remembered for it, else the one
/// carried over from the model before, each only when the new model lists it.
pub(crate) fn effort_on_pick(
    remembered: Option<Effort>,
    carried: Option<Effort>,
    levels: &[Effort],
) -> Option<Effort> {
    clamp_effort(remembered, levels).or_else(|| clamp_effort(carried, levels))
}

#[cfg(test)]
mod tests {
    use Effort::{High, Low, Max, Medium, XHigh};

    use super::*;

    fn model(id: &str, name: &str, about: Option<&str>) -> ModelChoice {
        ModelChoice {
            id: id.to_owned(),
            name: name.to_owned(),
            efforts: Vec::new(),
            default_effort: None,
            description: about.map(str::to_owned),
        }
    }

    fn harness(id: &str, usable: bool, models: Vec<ModelChoice>) -> HarnessChoice {
        HarnessChoice {
            id: id.parse().unwrap(),
            name: id.to_owned(),
            models,
            live: true,
            usable,
            switches: false,
            account: super::super::models::Account::NotReported,
        }
    }

    fn ids(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.model.id.as_str()).collect()
    }

    fn id(word: &str) -> ExecutorId {
        word.parse().unwrap()
    }

    #[test]
    fn the_rail_is_favourites_auto_and_each_harness_and_a_live_run_has_only_its_own() {
        let all = [
            harness("claude-code", true, vec![model("a", "A", None)]),
            harness("codex", false, vec![]),
        ];
        let keys = |v: Vec<TabView>| v.iter().map(|t| t.tab.key()).collect::<Vec<_>>();
        assert_eq!(
            keys(tabs(&all, None, true)),
            ["favourites", "auto", "claude-code", "codex"]
        );
        assert_eq!(
            keys(tabs(&all, None, false)),
            ["favourites", "claude-code", "codex"]
        );
        let live = tabs(&all, Some(&id("codex")), false);
        assert_eq!(keys(live.clone()), ["codex"]);
        assert!(
            live[0].signed_out && live[0].empty,
            "a dot and a dimmed logo"
        );
        assert!(!tabs(&all, None, true)[2].signed_out);
    }

    #[test]
    fn the_picker_opens_on_the_shown_harness_else_auto_else_favourites() {
        let all = [harness("claude-code", true, vec![])];
        let pick = Pick {
            executor: id("claude-code"),
            model: None,
            effort: None,
        };
        let open = |shown, live, home| opening_tab(shown, &all, live, home);
        assert_eq!(
            open(Some(&pick), None, true),
            Tab::Harness(id("claude-code"))
        );
        assert_eq!(open(None, None, true), Tab::Auto);
        assert_eq!(open(None, None, false), Tab::Favourites);
        assert_eq!(
            open(None, Some(&id("claude-code")), false),
            Tab::Harness(id("claude-code"))
        );
    }

    #[test]
    fn match_rank_kinds() {
        assert_eq!(match_rank("re", "release"), Some(0));
        assert_eq!(match_rank("LEASE", "release"), Some(1));
        assert_eq!(match_rank("x", "release"), None);
        assert_eq!(match_rank(" ", "anything"), Some(1));
    }

    fn fable_harnesses() -> [HarnessChoice; 2] {
        [
            harness("claude-code", true, vec![model("fable-5", "Fable 5", None)]),
            harness(
                "codex",
                true,
                vec![model("gpt-fable", "Fable (Codex)", None)],
            ),
        ]
    }

    #[test]
    fn tab_search_never_leaves_the_viewed_harness() {
        let all = fable_harnesses();
        let found = listing("fable", &Tab::Harness(id("claude-code")), &all, |_, _| {
            false
        });
        assert_eq!(ids(&found.rows), ["fable-5"]);
        assert_eq!(found.rows[0].harness, id("claude-code"));
        // The other harness's hit is offered apart, never among the tab's rows.
        assert_eq!(ids(&found.others), ["gpt-fable"]);
    }

    #[test]
    fn favourites_tab_search_ranks_only_starred_rows() {
        let all = fable_harnesses();
        let starred = |h: &ExecutorId, m: &str| h.as_str() == "codex" && m == "gpt-fable";
        let found = listing("fable", &Tab::Favourites, &all, starred);
        assert_eq!(ids(&found.rows), ["gpt-fable"]);
        assert!(found.others.is_empty());
        let found = listing("", &Tab::Favourites, &all, starred);
        assert_eq!(
            ids(&found.rows),
            ["gpt-fable"],
            "the starred set, nothing else"
        );
        assert!(found.rows[0].starred);
    }

    #[test]
    fn harness_tab_lists_stars_first_and_description_still_matches() {
        let all = [harness(
            "grok",
            true,
            vec![
                model("glm-5.2-a", "GLM-5.2", Some("Anthropic")),
                model("glm-5.2-b", "GLM-5.2", Some("Baseten")),
            ],
        )];
        let starred = |_: &ExecutorId, m: &str| m == "glm-5.2-b";
        let tab = Tab::Harness(id("grok"));
        let found = listing("", &tab, &all, starred);
        assert_eq!(
            ids(&found.rows),
            ["glm-5.2-b", "glm-5.2-a"],
            "the star floats up"
        );
        let found = listing("baseten", &tab, &all, starred);
        assert_eq!(
            ids(&found.rows),
            ["glm-5.2-b"],
            "the provider is searchable"
        );
    }

    #[test]
    fn a_search_ranks_prefix_then_substring_then_description_and_stars_break_ties() {
        let all = [harness(
            "grok",
            true,
            vec![
                model("d", "Other", Some("about grok")),
                model("s", "The Grok", None),
                model("p", "Grok 4", None),
                model("p2", "Grok mini", None),
            ],
        )];
        let tab = Tab::Harness(id("grok"));
        let found = listing("grok", &tab, &all, |_, m| m == "p2");
        assert_eq!(ids(&found.rows), ["p2", "p", "s", "d"]);
    }

    #[test]
    fn other_harness_matches_stop_at_three_and_a_blank_query_has_none() {
        let many = |id: &str| {
            harness(
                id,
                true,
                (0..5)
                    .map(|n| model(&format!("{id}-{n}"), &format!("Match {n}"), None))
                    .collect(),
            )
        };
        let all = [many("claude-code"), many("codex")];
        let tab = Tab::Harness(id("claude-code"));
        assert_eq!(
            listing("match", &tab, &all, |_, _| false).others.len(),
            OTHER_HARNESSES
        );
        assert!(listing("", &tab, &all, |_, _| false).others.is_empty());
        assert!(listing("zzz", &tab, &all, |_, _| false).rows.is_empty());
    }

    #[test]
    fn the_effort_track_fills_to_the_choice_and_deepens_toward_the_most() {
        let levels = [Low, Medium, High, Max];
        let filled = |cells: &[EffortCell]| cells.iter().map(|c| c.filled).collect::<Vec<_>>();
        let cells = effort_cells(&levels, Some(High), Some(Medium));
        assert_eq!(filled(&cells), [true, true, true, false]);
        assert!(cells.windows(2).all(|w| w[0].strength < w[1].strength));
        assert!((cells[3].strength - 1.0).abs() < f32::EPSILON);
        let none = effort_cells(&levels, None, Some(Medium));
        assert_eq!(filled(&none), [false; 4]);
        assert_eq!(
            none.iter().map(|c| c.default).collect::<Vec<_>>(),
            [false, true, false, false]
        );
        assert!(
            effort_cells(&levels, Some(Low), Some(Medium))
                .iter()
                .all(|c| !c.default)
        );
        assert_eq!(effort_cells(&[High], None, None)[0].strength, 0.35);
        assert_eq!(next_effort(None, High), Some(High));
        assert_eq!(next_effort(Some(Low), High), Some(High));
        assert_eq!(
            next_effort(Some(High), High),
            None,
            "the chosen cell again: the default"
        );
    }

    #[test]
    fn effort_words_name_the_level_or_the_default() {
        assert_eq!(effort_words(Some(XHigh), Some(High)), "X-High");
        assert_eq!(effort_words(None, Some(High)), "Default \u{b7} High");
        assert_eq!(effort_words(None, None), "Default");
    }

    #[test]
    fn a_level_the_model_does_not_list_heals_to_its_default() {
        let ladder = [Low, Medium, High, Max];
        assert_eq!(clamp_effort(Some(Max), &ladder), Some(Max));
        assert_eq!(
            clamp_effort(Some(XHigh), &ladder),
            None,
            "left to the model's default"
        );
        assert_eq!(clamp_effort(None, &ladder), None);
        assert_eq!(clamp_effort(Some(High), &[]), None);
    }

    #[test]
    fn a_picked_model_takes_its_remembered_effort_else_the_one_carried_over() {
        let ladder = [Low, Medium, High];
        // The model before was at Low; this one was last used at High.
        assert_eq!(effort_on_pick(Some(High), Some(Low), &ladder), Some(High));
        // Nothing remembered: the carried level, when this model lists it.
        assert_eq!(effort_on_pick(None, Some(Low), &ladder), Some(Low));
        assert_eq!(effort_on_pick(None, Some(Max), &ladder), None);
        // A remembered level the model no longer lists falls back to the carried one.
        assert_eq!(
            effort_on_pick(Some(Max), Some(Medium), &ladder),
            Some(Medium)
        );
        assert_eq!(effort_on_pick(Some(High), Some(Low), &[]), None);
    }
}

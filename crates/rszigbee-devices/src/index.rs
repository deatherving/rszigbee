//! The definition index: model string in, definition out.
//!
//! Resolution order reproduces zigbee-herdsman-converters `findDefinition`
//! (MIT, `src/index.ts:525`), reimplemented from its documented behaviour. The
//! order is not arbitrary and the shortcuts are not optimisations — each one
//! changes which definition a device gets, so [`DefinitionIndex::resolve`]
//! implements them exactly and the tests pin each in place.

use std::collections::HashMap;

use tracing::debug;

use crate::definition::Definition;
use crate::matcher::{DeviceMatch, Fingerprint, index_key, normalise_model};

/// Why a definition could not be added.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexError {
    /// The definition claims no model and no fingerprint, so nothing could ever
    /// find it.
    #[error("definition '{model}' has no model strings and no fingerprints, so it is unreachable")]
    Unreachable {
        /// Which definition.
        model: String,
    },
    /// A fingerprint constrains nothing, so it would claim every device that
    /// reaches it.
    #[error(
        "definition '{model}' has an empty fingerprint, which would claim every device \
         indexed under the same model"
    )]
    EmptyFingerprint {
        /// Which definition.
        model: String,
    },
}

/// Definitions, indexed by the model strings they can be found under.
#[derive(Debug, Default)]
pub struct DefinitionIndex {
    definitions: Vec<Definition>,
    /// Normalised, lowercased model string to candidate positions.
    ///
    /// Insertion order is preserved per key, because upstream's tie-breaks are
    /// first-wins and reordering would silently change which definition a
    /// device resolves to.
    by_model: HashMap<String, Vec<usize>>,
}

impl DefinitionIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many definitions are indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.definitions.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }

    /// Every definition, in insertion order.
    pub fn all(&self) -> impl Iterator<Item = &Definition> {
        self.definitions.iter()
    }

    /// How many definitions carry something the transcoder could not express.
    ///
    /// The coverage number, available at runtime rather than only in a build
    /// report, so an operator can see how much of their catalogue is complete.
    #[must_use]
    pub fn incomplete(&self) -> usize {
        self.definitions.iter().filter(|d| !d.is_complete()).count()
    }

    /// Adds a definition.
    ///
    /// # Errors
    ///
    /// Refuses a definition nothing could match, and one whose fingerprint
    /// constrains nothing. Both are transcoder bugs, and both are worse than
    /// useless if accepted: the first is dead weight, the second hijacks every
    /// device sharing its model string.
    pub fn insert(&mut self, definition: Definition) -> Result<(), IndexError> {
        let rules = &definition.match_rules;
        if rules.models.is_empty() && rules.fingerprints.is_empty() {
            return Err(IndexError::Unreachable {
                model: definition.model.clone(),
            });
        }
        if rules.fingerprints.iter().any(Fingerprint::is_empty) {
            return Err(IndexError::EmptyFingerprint {
                model: definition.model.clone(),
            });
        }

        let position = self.definitions.len();

        // Indexed under every model string it claims, plus every model a
        // fingerprint names. A fingerprint-only definition is unreachable
        // otherwise, and that is 21% of upstream's catalogue.
        //
        // Each string is indexed under *two* keys: lowercased as written, and
        // additionally normalised. Upstream keys only on the lowercased form
        // and falls back to a normalised lookup (`index.ts:217`), so two
        // definitions differing only in whitespace get separate buckets there.
        // Normalising on insert would merge them, and then a device reporting
        // the exact string of one would resolve by candidate order instead of
        // by what it actually reported. Two real definitions differ exactly
        // this way -- `NLG-RGBW light` and `NLG-RGBW light ` -- and the
        // differential test against upstream caught it.
        let mut keys = Vec::with_capacity(rules.models.len() * 2 + rules.fingerprints.len());
        let mut add = |model: &str| {
            keys.push(model.to_lowercase());
            keys.push(index_key(model));
        };
        for model in &rules.models {
            add(model);
        }
        for fingerprint in &rules.fingerprints {
            if let Some(model) = &fingerprint.model_id {
                add(model);
            }
        }
        keys.sort_unstable();
        keys.dedup();

        for key in keys {
            // Prepended, not appended, matching upstream's index builder
            // (`indexer.ts`, `splice(0, 0, index)`). Combined with the
            // first-wins tie-break in `resolve`, that makes the *latest*
            // definition win a tie. It makes no difference on today's
            // catalogue -- upstream rejects duplicate model strings outright,
            // so ties can only arise between equal-priority fingerprints, and
            // none exist -- but reproducing the order costs nothing and removes
            // a divergence that would otherwise appear the first time one does.
            self.by_model.entry(key).or_default().insert(0, position);
        }
        self.definitions.push(definition);
        Ok(())
    }

    /// Resolves the definition for a device.
    ///
    /// Returns `None` for a device with no model string, or one no definition
    /// claims. That is a normal outcome, not an error: an unknown device still
    /// produces raw events, which is what someone needs in order to write a
    /// definition for it.
    #[must_use]
    pub fn resolve(&self, device: &DeviceMatch) -> Option<&Definition> {
        let model = device.model_id.as_deref()?;
        let candidates = self.candidates(model)?;

        // Upstream shortcut (`index.ts:545`): a sole candidate that claims a
        // model string wins without its fingerprints being consulted. This is
        // load-bearing, not an optimisation — a definition whose fingerprint
        // requires a field the interview never learned still matches here, and
        // omitting the shortcut would fail to match devices upstream matches.
        if let [only] = candidates.as_slice()
            && self
                .definitions
                .get(*only)
                .is_some_and(|d| !d.match_rules.models.is_empty())
        {
            return self.definitions.get(*only);
        }

        debug!(
            model,
            candidates = candidates.len(),
            "resolving a device definition"
        );

        // Fingerprints first, highest priority winning. Strictly greater, so
        // equal priorities resolve to the earliest — upstream's behaviour, and
        // the reason insertion order is preserved.
        let mut best: Option<(i32, usize)> = None;
        for &position in candidates {
            let definition = self.definitions.get(position)?;
            for fingerprint in &definition.match_rules.fingerprints {
                if !fingerprint.matches(device) {
                    continue;
                }
                if best.is_none_or(|(priority, _)| fingerprint.priority > priority) {
                    best = Some((fingerprint.priority, position));
                }
            }
        }
        if let Some((_, position)) = best {
            return self.definitions.get(position);
        }

        // Then the model list. Compared against both the reported string and
        // its normalised form, because a candidate may have been reached by
        // either.
        let normalised = normalise_model(model);
        for &position in candidates {
            let definition = self.definitions.get(position)?;
            if definition
                .match_rules
                .models
                .iter()
                .any(|m| m == model || m == &normalised)
            {
                return self.definitions.get(position);
            }
        }
        None
    }

    /// Candidates for a model string: exact key first, then normalised.
    fn candidates(&self, model: &str) -> Option<&Vec<usize>> {
        let exact = model.to_lowercase();
        if let Some(found) = self.by_model.get(&exact) {
            return Some(found);
        }
        self.by_model.get(&index_key(model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{Definition, Extend};
    use crate::matcher::{Fingerprint, MatchRules};

    fn definition(model: &str, models: &[&str]) -> Definition {
        Definition {
            model: model.into(),
            match_rules: MatchRules {
                models: models.iter().map(|&m| m.into()).collect(),
                fingerprints: Vec::new(),
            },
            ..Definition::default()
        }
    }

    fn tuya(model: &str, manufacturer: &str, priority: i32) -> Definition {
        Definition {
            model: model.into(),
            match_rules: MatchRules {
                models: Vec::new(),
                fingerprints: vec![Fingerprint {
                    model_id: Some("TS0601".into()),
                    manufacturer_name: Some(manufacturer.into()),
                    priority,
                    ..Fingerprint::default()
                }],
            },
            ..Definition::default()
        }
    }

    fn seen(model: &str, manufacturer: Option<&str>) -> DeviceMatch {
        DeviceMatch {
            model_id: Some(model.into()),
            manufacturer_name: manufacturer.map(Into::into),
            ..DeviceMatch::default()
        }
    }

    #[test]
    fn a_device_with_no_model_resolves_to_nothing_rather_than_guessing() {
        let mut index = DefinitionIndex::new();
        index.insert(definition("a", &["X"])).expect("insert");
        assert!(index.resolve(&DeviceMatch::default()).is_none());
    }

    #[test]
    fn an_unknown_model_is_a_normal_outcome() {
        let mut index = DefinitionIndex::new();
        index.insert(definition("a", &["X"])).expect("insert");
        // Not an error: the runtime still emits raw events for it, which is how
        // someone gets what they need to write a definition.
        assert!(index.resolve(&seen("Y", None)).is_none());
    }

    #[test]
    fn a_nul_padded_model_still_resolves() {
        let mut index = DefinitionIndex::new();
        index
            .insert(definition("ht", &["lumi.sensor_ht"]))
            .expect("insert");
        let resolved = index
            .resolve(&seen("lumi.sensor_ht\0\0\0", None))
            .expect("a NUL-padded model is the same model");
        assert_eq!(resolved.model, "ht");
    }

    #[test]
    fn matching_is_case_insensitive_on_the_model_string() {
        let mut index = DefinitionIndex::new();
        index
            .insert(definition("ht", &["lumi.sensor_ht"]))
            .expect("insert");
        assert_eq!(
            index
                .resolve(&seen("LUMI.Sensor_HT", None))
                .map(|d| &*d.model),
            Some("ht")
        );
    }

    #[test]
    fn a_fingerprint_only_definition_is_reachable_by_its_model() {
        // 21% of upstream definitions match only by fingerprint. Indexing only
        // `models` would make all of them unreachable.
        let mut index = DefinitionIndex::new();
        index
            .insert(tuya("soil", "_TZE200_myd45weu", 0))
            .expect("insert");
        index
            .insert(tuya("valve", "_TZE200_other", 0))
            .expect("insert");

        assert_eq!(
            index
                .resolve(&seen("TS0601", Some("_TZE200_myd45weu")))
                .map(|d| &*d.model),
            Some("soil")
        );
        assert_eq!(
            index
                .resolve(&seen("TS0601", Some("_TZE200_other")))
                .map(|d| &*d.model),
            Some("valve")
        );
        // A Tuya device nobody has written a definition for.
        assert!(
            index
                .resolve(&seen("TS0601", Some("_TZE200_unknown")))
                .is_none()
        );
    }

    #[test]
    fn a_higher_priority_fingerprint_wins() {
        let mut index = DefinitionIndex::new();
        index
            .insert(tuya("generic", "_TZE200_shared", 0))
            .expect("insert");
        index
            .insert(tuya("specific", "_TZE200_shared", 10))
            .expect("insert");
        assert_eq!(
            index
                .resolve(&seen("TS0601", Some("_TZE200_shared")))
                .map(|d| &*d.model),
            Some("specific"),
            "priority exists precisely to override a broader match"
        );
    }

    #[test]
    fn equal_priorities_resolve_to_the_last_registered() {
        // Two upstream behaviours compose here. The index *prepends*, so the
        // last-registered definition comes first in the candidate list; and the
        // priority comparison is strictly greater, so the first matching
        // candidate keeps a tie. Net effect: the last one registered wins.
        //
        // Worth pinning even though no tie exists in today's catalogue: the
        // day one does, silently picking the other definition is a device that
        // behaves differently from upstream for no visible reason.
        let mut index = DefinitionIndex::new();
        index
            .insert(tuya("first", "_TZE200_shared", 5))
            .expect("insert");
        index
            .insert(tuya("second", "_TZE200_shared", 5))
            .expect("insert");
        assert_eq!(
            index
                .resolve(&seen("TS0601", Some("_TZE200_shared")))
                .map(|d| &*d.model),
            Some("second")
        );
    }

    #[test]
    fn a_sole_candidate_with_a_model_list_wins_without_its_fingerprints() {
        // The upstream shortcut, and it is load-bearing: this definition's
        // fingerprint requires an application version the interview never
        // learned, so a fingerprint-first implementation would resolve nothing.
        let mut index = DefinitionIndex::new();
        index
            .insert(Definition {
                model: "sole".into(),
                match_rules: MatchRules {
                    models: vec!["WIDGET".into()],
                    fingerprints: vec![Fingerprint {
                        application_version: Some(99),
                        ..Fingerprint::default()
                    }],
                },
                ..Definition::default()
            })
            .expect("insert");

        assert_eq!(
            index.resolve(&seen("WIDGET", None)).map(|d| &*d.model),
            Some("sole"),
            "a sole candidate claiming this model must win regardless of its fingerprint"
        );
    }

    #[test]
    fn with_several_candidates_a_fingerprint_beats_a_model_list() {
        let mut index = DefinitionIndex::new();
        index
            .insert(definition("broad", &["TS0601"]))
            .expect("insert");
        index
            .insert(tuya("narrow", "_TZE200_myd45weu", 0))
            .expect("insert");

        // The fingerprint pass runs first, so the specific definition wins for
        // the device it names...
        assert_eq!(
            index
                .resolve(&seen("TS0601", Some("_TZE200_myd45weu")))
                .map(|d| &*d.model),
            Some("narrow")
        );
        // ...and the broad one still catches everything else.
        assert_eq!(
            index
                .resolve(&seen("TS0601", Some("_TZE200_anything")))
                .map(|d| &*d.model),
            Some("broad")
        );
    }

    #[test]
    fn a_definition_nothing_could_match_is_refused() {
        let mut index = DefinitionIndex::new();
        let error = index
            .insert(Definition {
                model: "orphan".into(),
                ..Definition::default()
            })
            .expect_err("a definition with no match rules is unreachable");
        assert!(matches!(error, IndexError::Unreachable { .. }), "{error:?}");
        assert!(index.is_empty());
    }

    #[test]
    fn an_empty_fingerprint_is_refused_rather_than_hijacking_every_device() {
        let mut index = DefinitionIndex::new();
        let error = index
            .insert(Definition {
                model: "greedy".into(),
                match_rules: MatchRules {
                    models: vec!["TS0601".into()],
                    fingerprints: vec![Fingerprint::default()],
                },
                ..Definition::default()
            })
            .expect_err("an unconstrained fingerprint would claim every TS0601");
        assert!(
            matches!(error, IndexError::EmptyFingerprint { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_index_reports_how_many_definitions_are_incomplete() {
        let mut index = DefinitionIndex::new();
        index
            .insert(definition("complete", &["A"]))
            .expect("insert");
        let mut partial = definition("partial", &["B"]);
        partial.extend.push(Extend::Unsupported {
            helper: "tuya.valueConverter".into(),
            note: "needs a value converter".into(),
        });
        index.insert(partial).expect("insert");

        assert_eq!(index.len(), 2);
        // The coverage number, visible at runtime and not only in a build log.
        assert_eq!(index.incomplete(), 1);
    }
}

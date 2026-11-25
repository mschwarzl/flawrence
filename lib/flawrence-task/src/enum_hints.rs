//! Semantic tables behind the enum "did you mean" hint.
//!
//! Lexical distance alone cannot repair the enum slips that actually happen.
//! `moderate` is 6 edits from `medium`, while the wrong-but-tempting
//! `source` -> `sink` is only 5: no threshold separates the fix we want from
//! the mistake we must never make. Two hand-curated tables break the tie —
//! synonyms name the single value a word means, antonyms veto a value a word
//! must never be resolved to — and the invariant tests at the bottom of this
//! file keep future entries from colliding with each other or with the
//! lexical matcher.
//!
//! The tables are global: they are consulted for every schema, and the
//! caller's own enum decides which entries apply. That is why an entry may
//! only ever name one value per word, and why the tests are worth more than
//! the coverage.

/// The enum value sets that appear in `agents/*.md` output schemas. Only the
/// grouping matters: the invariant tests need to know which values compete
/// with each other, because a synonym is only dangerous next to the values it
/// could be confused with. Nothing at runtime reads this — the caller always
/// passes the enum it actually validated against — so it is compiled only for
/// the tests that police the tables.
#[cfg(test)]
const ENUM_GROUPS: &[&[&str]] = &[
    &["confirmed", "rejected"],
    &["duplicate", "distinct"],
    &["entrypoint", "propagation", "sink"],
    &[
        "authentication_level",
        "authorization_role",
        "user_interaction",
        "system_configuration",
        "network_routing",
        "environmental_dependency",
        "data_state",
        "timing_dependency",
        "third_party_dependency",
    ],
    &["informational", "low", "medium", "high", "critical"],
    &["low", "medium", "high", "critical"],
    &["low", "medium", "high"],
    &[
        "http_cross_tenant",
        "local_reproducer",
        "static_proof",
        "source_only",
    ],
    &["bypass_found", "patch_holds", "blocked"],
    &["passed", "failed", "blocked"],
    &["approved", "rejected"],
    &["true", "false", "unknown"],
];

/// Words that mean exactly one permitted value. Keys are normalized
/// (lowercase, trimmed) and matched exactly — no stemming, no fuzz — so that
/// the invariants below actually constrain the behaviour.
///
/// Several of these exist because the lexical matcher would otherwise get the
/// answer *inverted*: `not duplicate` contains `duplicate`. Containment is the
/// strongest lexical signal there is, and negation flips its meaning, so those
/// words need both a synonym entry and an antonym veto.
const SYNONYMS: &[(&str, &str)] = &[
    // Observed in run 9's rejected submissions.
    ("moderate", "medium"),
    ("very high", "high"),
    ("very_high", "high"),
    ("reproducer", "local_reproducer"),
    ("source", "entrypoint"),
    // Negated permitted values: lexically these resolve to their own
    // opposite.
    // `unconfirmed` is deliberately absent: it is 2 edits from `confirmed`,
    // inside the lexical matcher's own typo radius, so INV3 rules it out as a
    // synonym. Its antonym entry still stops it resolving to `confirmed`;
    // "no hint" is the correct outcome for a word that close to its opposite.
    ("not confirmed", "rejected"),
    ("not_confirmed", "rejected"),
    ("not duplicate", "distinct"),
    ("not_duplicate", "distinct"),
    ("unique", "distinct"),
    // A bare `bypass` in a red-team verdict reports that an attempt succeeded.
    ("bypass", "bypass_found"),
    // A reviewer writing `correct` about a patch means it passed review.
    ("correct", "approved"),
    ("success", "passed"),
    ("successful", "passed"),
    ("failure", "failed"),
    ("undetermined", "unknown"),
    ("indeterminate", "unknown"),
];

/// Words paired with a value they must NEVER be resolved to, whatever the
/// lexical score says. `source` -> `sink` is the motivating pair: five edits
/// apart, semantically opposite, and the most expensive wrong hint this code
/// can emit, because it sends the retry to the far end of the trace.
///
/// A key may legitimately be a synonym of one value and an antonym of
/// another; it may never be both for the same value.
const ANTONYMS: &[(&str, &str)] = &[
    ("source", "sink"),
    ("unconfirmed", "confirmed"),
    ("not confirmed", "confirmed"),
    ("not_confirmed", "confirmed"),
    ("not duplicate", "duplicate"),
    ("not_duplicate", "duplicate"),
];

/// Table lookups ignore case and surrounding whitespace, because that is the
/// only variation a model's own formatting introduces for free.
fn normalize(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

/// The permitted value that `submitted` means according to the synonym table,
/// or None when the table has no entry or its entry is irrelevant to this
/// enum.
///
/// The tables span every schema, so an entry only counts when the value it
/// names is actually permitted here: `source` means `entrypoint` in a trace,
/// and nothing at all in a reproduction method.
pub(crate) fn dictionary_suggestion(submitted: &str, permitted: &[&str]) -> Option<&'static str> {
    let needle = normalize(submitted);
    SYNONYMS
        .iter()
        .filter(|(key, _)| *key == needle.as_str())
        .map(|(_, value)| *value)
        .find(|value| permitted.iter().any(|p| p.eq_ignore_ascii_case(value)))
}

/// Negation morphemes, longest first so `not_a_` is stripped before `not_` and
/// `non` before `no`. Order is load-bearing: a shorter prefix would leave a
/// stem that no longer matches the value being negated.
const NEGATIONS: &[&str] = &[
    "not_a_", "not a ", "not-a-", "is_not_", "is not ", "isnt_", "isn't_", "isnt ", "not_", "not ",
    "not-", "non_", "non-", "non", "no_", "no ", "no-", "un_", "un-", "un",
];

/// What `submitted` negates, if it negates a permitted value.
///
/// Every spelling of a negation CONTAINS the value it negates -- `no_duplicate`
/// contains `duplicate`, `unconfirmed` contains `confirmed` -- so both
/// containment and a short edit distance point confidently at the affirmative,
/// which is the opposite verdict. Listing spellings in the synonym table is a
/// losing game (`no_`, `non`, `un`, `isnt_`, `not_a_`, with or without
/// separators), so the prefix is recognised structurally instead.
fn negated_value<'a>(submitted: &str, permitted: &[&'a str]) -> Option<&'a str> {
    let needle = normalize(submitted);
    let stem = NEGATIONS
        .iter()
        .find_map(|prefix| needle.strip_prefix(prefix))?
        .trim_start_matches(['_', '-', ' ']);
    if stem.is_empty() {
        return None;
    }
    permitted
        .iter()
        .copied()
        .find(|value| crate::r#loop::edit_distance(stem, &normalize(value)) <= 2)
}

/// The value a negation points to, or None when it cannot be inferred.
///
/// `not A` identifies `B` only when the domain is exactly {A, B} -- there the
/// complement is a tautology, not a guess. With a third option present (an
/// error or inconclusive state, say) the negation still proves the affirmative
/// is wrong, but not which of the remaining values was meant, so it yields no
/// hint rather than a plausible one.
pub(crate) fn negation_suggestion<'a>(submitted: &str, permitted: &[&'a str]) -> Option<&'a str> {
    let negated = negated_value(submitted, permitted)?;
    if permitted.len() != 2 {
        return None;
    }
    permitted
        .iter()
        .copied()
        .find(|value| !value.eq_ignore_ascii_case(negated))
}

/// Whether `submitted` negates `candidate`, which forbids the suggestion
/// however well it scores lexically. This is the veto half of the negation
/// rule and applies at any arity, unlike the complement inference.
pub(crate) fn negation_forbids(submitted: &str, candidate: &str, permitted: &[&str]) -> bool {
    negated_value(submitted, permitted)
        .is_some_and(|negated| negated.eq_ignore_ascii_case(candidate))
}

/// Whether suggesting `candidate` for `submitted` is forbidden outright.
/// Checked against every lexical candidate, so a high containment or a short
/// edit distance cannot resurrect a pair the antonym table has ruled out.
pub(crate) fn is_vetoed(submitted: &str, candidate: &str) -> bool {
    let needle = normalize(submitted);
    let candidate = normalize(candidate);
    ANTONYMS
        .iter()
        .any(|(key, forbidden)| *key == needle.as_str() && *forbidden == candidate.as_str())
}

#[cfg(test)]
mod fuzz {
    use super::ENUM_GROUPS;
    use crate::r#loop::nearest_enum_value_for_test as nearest;

    /// Every single-character corruption of every permitted value, across every
    /// enum the harness ships. A hint that names a DIFFERENT permitted value
    /// than the one that was corrupted is a flipped verdict, which on fields
    /// like `verdict` or `overall_severity` is worse than no hint at all.
    ///
    /// This caught a real regression: the red-team verdict used to offer both
    /// `bypass_found` and `no_bypass_found`, and because the former is a
    /// substring of the latter, 164 corruptions of the negative verdict
    /// resolved to the affirmative one through the containment branch. The
    /// enum has since been renamed to remove the nesting, and this test is
    /// what proves the replacement values are actually safe. Keep it —
    /// the failure mode is invisible to example-based tests, because it needs a
    /// nesting or near-distance relationship between two PERMITTED values that
    /// nobody thinks to write a case for.
    #[test]
    fn no_single_edit_corruption_ever_resolves_to_a_different_permitted_value() {
        const ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz_ -";
        let mut flips = Vec::new();

        for group in ENUM_GROUPS {
            for value in group.iter() {
                for typo in single_edits(value, ALPHABET) {
                    // Still valid, so it never reaches the matcher.
                    if group.iter().any(|v| v.eq_ignore_ascii_case(&typo)) {
                        continue;
                    }
                    if let Some(hint) = nearest(&typo, group)
                        && !hint.eq_ignore_ascii_case(value)
                    {
                        flips.push(format!("{value:?} typed as {typo:?} -> {hint:?}"));
                    }
                }
            }
        }

        assert!(
            flips.is_empty(),
            "{} flipped hint(s); first few:\n{}",
            flips.len(),
            flips.iter().take(8).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    /// Deletions, substitutions and insertions at every position.
    fn single_edits(value: &str, alphabet: &str) -> Vec<String> {
        let chars: Vec<char> = value.chars().collect();
        let mut out = Vec::new();
        for i in 0..chars.len() {
            let mut deleted = chars.clone();
            deleted.remove(i);
            out.push(deleted.into_iter().collect());
        }
        for i in 0..chars.len() {
            for replacement in alphabet.chars() {
                let mut swapped = chars.clone();
                swapped[i] = replacement;
                out.push(swapped.into_iter().collect());
            }
        }
        for i in 0..=chars.len() {
            for inserted in alphabet.chars() {
                let mut grown = chars.clone();
                grown.insert(i, inserted);
                out.push(grown.into_iter().collect());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{ANTONYMS, ENUM_GROUPS, SYNONYMS, dictionary_suggestion, is_vetoed};
    use crate::r#loop::edit_distance;

    /// The lexical matcher's own typo threshold. Anything this close is
    /// reachable by edit distance, so the tables must not put two different
    /// meanings inside that radius.
    const TOO_CLOSE: usize = 2;

    fn groups_containing(value: &str) -> impl Iterator<Item = &'static [&'static str]> {
        let value = value.to_string();
        ENUM_GROUPS
            .iter()
            .copied()
            .filter(move |group| group.contains(&value.as_str()))
    }

    #[test]
    fn table_keys_are_stored_normalized() {
        for (key, _) in SYNONYMS.iter().chain(ANTONYMS.iter()) {
            assert_eq!(
                *key,
                key.trim().to_ascii_lowercase(),
                "table key {key:?} is not normalized, so lookups will never hit it"
            );
        }
    }

    #[test]
    fn every_table_value_is_a_permitted_value_somewhere() {
        for (key, value) in SYNONYMS.iter().chain(ANTONYMS.iter()) {
            assert!(
                groups_containing(value).next().is_some(),
                "{key:?} targets {value:?}, which is not a value of any known enum"
            );
        }
    }

    /// INV1: a permitted value needs no synonym; mapping one to another value
    /// would rewrite a legal answer into a different legal answer.
    #[test]
    fn inv1_no_synonym_key_is_itself_permitted_in_a_targeted_enum() {
        for (key, value) in SYNONYMS {
            for group in groups_containing(value) {
                assert!(
                    !group.contains(key),
                    "synonym {key:?} -> {value:?} but {key:?} is itself permitted in {group:?}"
                );
            }
        }
    }

    /// INV2: a typo in one synonym must not land on a synonym that means
    /// something else in the same enum.
    #[test]
    fn inv2_synonym_keys_meaning_different_values_are_well_separated() {
        assert_keys_separated_from_each_other(SYNONYMS, "synonym");
    }

    /// INV3: if a synonym key is typo-close to some *other* permitted value,
    /// the lexical matcher could beat the dictionary to the wrong answer.
    #[test]
    fn inv3_synonym_keys_are_far_from_the_values_they_do_not_mean() {
        assert_keys_separated_from_permitted(SYNONYMS, "synonym");
    }

    /// INV4: the antonym table is consulted in the same lexical neighbourhood,
    /// so it carries the same separation obligations.
    #[test]
    fn inv4_antonym_keys_obey_the_same_separation_rules() {
        assert_keys_separated_from_each_other(ANTONYMS, "antonym");
        assert_keys_separated_from_permitted(ANTONYMS, "antonym");
    }

    fn assert_keys_separated_from_each_other(table: &[(&str, &str)], label: &str) {
        for (i, (key, value)) in table.iter().enumerate() {
            for (other_key, other_value) in &table[i + 1..] {
                if value == other_value {
                    continue;
                }
                let shares_enum = groups_containing(value).any(|group| group.contains(other_value));
                if !shares_enum {
                    continue;
                }
                let distance = edit_distance(key, other_key);
                assert!(
                    distance > TOO_CLOSE,
                    "{label} keys {key:?} ({value:?}) and {other_key:?} ({other_value:?}) \
                     are {distance} edits apart in the same enum; a typo in one lands on the other"
                );
            }
        }
    }

    fn assert_keys_separated_from_permitted(table: &[(&str, &str)], label: &str) {
        for (key, value) in table {
            for group in groups_containing(value) {
                for permitted in group.iter().filter(|p| *p != value) {
                    let distance = edit_distance(key, permitted);
                    assert!(
                        distance > TOO_CLOSE,
                        "{label} key {key:?} means {value:?} but is only {distance} edits from \
                         {permitted:?} in the same enum; the lexical matcher would win"
                    );
                }
            }
        }
    }

    /// INV5: a word cannot both mean a value and be forbidden from meaning it.
    #[test]
    fn inv5_no_word_is_both_synonym_and_antonym_of_the_same_value() {
        for (key, value) in SYNONYMS {
            assert!(
                !ANTONYMS
                    .iter()
                    .any(|(other, forbidden)| other == key && forbidden == value),
                "{key:?} is both a synonym and an antonym of {value:?}"
            );
        }
    }

    /// INV6: being a synonym of one value and an antonym of another is the
    /// whole point. `source` means `entrypoint` and must never become `sink`;
    /// rejecting that combination would throw away the case the dictionary
    /// exists for.
    #[test]
    fn inv6_a_word_may_mean_one_value_and_be_vetoed_from_another() {
        let trace = ["entrypoint", "propagation", "sink"];
        assert_eq!(dictionary_suggestion("source", &trace), Some("entrypoint"));
        assert!(is_vetoed("source", "sink"));
        assert!(!is_vetoed("source", "entrypoint"));

        let overlaps: Vec<_> = SYNONYMS
            .iter()
            .filter(|(key, value)| {
                ANTONYMS
                    .iter()
                    .any(|(other, forbidden)| other == key && forbidden != value)
            })
            .collect();
        assert!(
            !overlaps.is_empty(),
            "the synonym/antonym overlap is the mechanism, not an accident"
        );
    }

    #[test]
    fn lookups_ignore_case_and_surrounding_whitespace() {
        let severity = ["informational", "low", "medium", "high", "critical"];
        assert_eq!(
            dictionary_suggestion("  Moderate\n", &severity),
            Some("medium")
        );
        assert!(is_vetoed(" SOURCE ", "Sink"));
    }

    /// The tables are global, so an entry must stay silent for enums that do
    /// not offer its value.
    #[test]
    fn declines_when_the_meant_value_is_not_permitted_here() {
        assert_eq!(
            dictionary_suggestion(
                "source",
                &[
                    "http_cross_tenant",
                    "local_reproducer",
                    "static_proof",
                    "source_only"
                ]
            ),
            None
        );
        assert_eq!(
            dictionary_suggestion("moderate", &["confirmed", "rejected"]),
            None
        );
    }

    /// `code_access` was invented by a model for `conditions.kind`. No single
    /// permitted value covers it, so it stays unmapped: a wrong hint costs
    /// more than the raw list of options.
    #[test]
    fn leaves_words_with_no_defensible_target_unmapped() {
        assert_eq!(
            dictionary_suggestion(
                "code_access",
                &["authentication_level", "authorization_role", "data_state"]
            ),
            None
        );
        // `auth` cannot be resolved either: it fits authentication and
        // authorization equally.
        assert_eq!(
            dictionary_suggestion("auth", &["authentication_level", "authorization_role"]),
            None
        );
    }
}

//! Precision corpus and per-sample latency budget (#32).
//!
//! Operationalizes the two `.claude/OUTCOME.md` Indicators that were declared
//! but never measured:
//!   - Precision (keep the false-positive rate low): each rule has a `.fire.txt`
//!     fixture that should be flagged and a `.clean.txt` fixture that should
//!     stay silent. A clean fixture that gets flagged is a false positive.
//!   - Time (keep scanning at millisecond scale): every fixture is scanned 50
//!     times and the median latency is asserted under 10ms/file.
//!
//! The corpus drives `analyze_source` (the production analysis pass), not a
//! copied rule list, so the metrics track exactly what litmus emits. Fixtures
//! use the `.txt` extension so litmus' own `**/*.test.ts` scan never sees them.
//!
//! No serde or other dependency is added (OUTCOME constraint: keep the hook
//! path startup cost). Metrics are written to stderr as a plain table; this
//! slice does not add machine-readable output or a CI delta gate.

use crate::analyze_source;
use crate::rules::{Issue, RULE_CATALOG};
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expectation {
    Fire,
    Clean,
}

struct CorpusSample {
    rule: &'static str,
    expectation: Expectation,
    // Virtual filename. Only its extension matters: it selects the parser
    // source type. All fixtures are plain TypeScript, so `.test.ts`.
    path: &'static str,
    content: &'static str,
}

// Binds a fire/clean fixture pair per rule at compile time. include_str! is
// relative to this file, so the fixtures live in src/precision/fixtures/.
macro_rules! corpus {
    ($($rule:literal),* $(,)?) => {
        &[
            $(
                CorpusSample {
                    rule: $rule,
                    expectation: Expectation::Fire,
                    path: concat!($rule, ".fire.test.ts"),
                    content: include_str!(concat!("precision/fixtures/", $rule, ".fire.txt")),
                },
                CorpusSample {
                    rule: $rule,
                    expectation: Expectation::Clean,
                    path: concat!($rule, ".clean.test.ts"),
                    content: include_str!(concat!("precision/fixtures/", $rule, ".clean.txt")),
                },
            )*
        ]
    };
}

const CORPUS: &[CorpusSample] = corpus![
    "catch-masks-assertion",
    "catch-only-assertion",
    "catch-swallow",
    "conditional-assertion",
    "dummy-data",
    "empty-test",
    "missing-act",
    "mock-only",
    "mock-overuse",
    "skipped-test",
    "snapshot-external",
    "tautological",
    "test-name-quality",
    "weak-assertion",
];

// Scans one fixture through the production analysis pass. A parse failure is a
// loud panic, not a skip: a fire fixture that silently stops parsing would be
// counted as a false negative and hide the regression it exists to catch.
fn scan(sample: &CorpusSample) -> Vec<Issue> {
    analyze_source(sample.content, Path::new(sample.path))
        .unwrap_or_else(|e| panic!("fixture {} failed to parse: {e}", sample.path))
}

const LATENCY_ITERATIONS: usize = 50;
// Per-file scan budget at the median. The slowest fixture measures ~66us/file
// (debug median, audit 2026-06-20) and ~35us on a 2026-06-21 recheck, so this
// budget sits within an order of magnitude of the measurement. A 2-digit (>=10x)
// latency regression therefore breaches it. The prior 10_000us budget was ~150x
// the measurement, wide enough that a 2-digit regression stayed green and the
// Time indicator's drift detector was effectively dead.
const LATENCY_BUDGET_US: u128 = 500;
// Slowest fixture median observed in the audit (debug build); the regression
// guard below pins the budget to within an order of magnitude of it.
const OBSERVED_SLOWEST_US: u128 = 66;

// Median scan latency in microseconds over LATENCY_ITERATIONS runs. Median
// rather than mean so a single scheduler stall does not dominate the budget.
fn median_latency_us(sample: &CorpusSample) -> u128 {
    let mut measured = Vec::with_capacity(LATENCY_ITERATIONS);
    for _ in 0..LATENCY_ITERATIONS {
        let start = Instant::now();
        let _ = scan(sample);
        measured.push(start.elapsed().as_micros());
    }
    measured.sort_unstable();
    measured[LATENCY_ITERATIONS / 2]
}

#[derive(Default)]
struct Tally {
    tp: u32,
    fn_count: u32,
    fp: u32,
    tn: u32,
}

// num / denom, returning 1.0 when denom is 0 (no sample of that kind is a
// perfect score, not a divide-by-zero). u32 -> f64 is a widening cast.
fn ratio(num: u32, denom: u32) -> f64 {
    if denom == 0 {
        1.0
    } else {
        f64::from(num) / f64::from(denom)
    }
}

fn tally_corpus() -> Tally {
    let mut tally = Tally::default();
    for sample in CORPUS {
        let issues = scan(sample);
        let target_fired = issues.iter().any(|i| i.rule == sample.rule);
        let any_fired = !issues.is_empty();
        match sample.expectation {
            Expectation::Fire => {
                if target_fired {
                    tally.tp += 1;
                } else {
                    tally.fn_count += 1;
                }
            }
            Expectation::Clean => {
                if any_fired {
                    tally.fp += 1;
                } else {
                    tally.tn += 1;
                }
            }
        }
    }
    tally
}

// T-021: every corpus sample names a rule that exists in RULE_CATALOG, so a
// typo'd fixture rule cannot silently escape the coverage gate.
#[test]
fn corpus_sample_rules_exist_in_catalog() {
    for sample in CORPUS {
        assert!(
            RULE_CATALOG.contains(&sample.rule),
            "corpus sample rule {} is not in RULE_CATALOG",
            sample.rule
        );
    }
}

// T-020: every catalog rule has both a fire and a clean fixture, so a new rule
// cannot be added to the catalog without a precision sample pair.
#[test]
fn corpus_covers_every_rule_with_fire_and_clean() {
    for rule in RULE_CATALOG {
        let has_fire = CORPUS
            .iter()
            .any(|s| s.rule == *rule && s.expectation == Expectation::Fire);
        let has_clean = CORPUS
            .iter()
            .any(|s| s.rule == *rule && s.expectation == Expectation::Clean);
        assert!(has_fire, "rule {rule} has no fire fixture");
        assert!(has_clean, "rule {rule} has no clean fixture");
    }
}

// T-022: each fire fixture triggers its own rule. This is the recall guard:
// the bad-test example for a rule must still be caught.
#[test]
fn fire_fixtures_trigger_their_rule() {
    for sample in CORPUS {
        if sample.expectation != Expectation::Fire {
            continue;
        }
        let issues = scan(sample);
        assert!(
            issues.iter().any(|i| i.rule == sample.rule),
            "fire fixture {} did not trigger {}; fired: {:?}",
            sample.path,
            sample.rule,
            issues.iter().map(|i| i.rule).collect::<Vec<_>>()
        );
    }
}

// T-023: each clean fixture stays silent across every rule. This is the
// precision guard: a well-written test must produce zero findings, so any
// finding is a false positive.
#[test]
fn clean_fixtures_stay_silent() {
    for sample in CORPUS {
        if sample.expectation != Expectation::Clean {
            continue;
        }
        let issues = scan(sample);
        assert!(
            issues.is_empty(),
            "clean fixture {} was flagged (false positive): {:?}",
            sample.path,
            issues.iter().map(|i| i.rule).collect::<Vec<_>>()
        );
    }
}

// T-024: every fixture scans under the per-file latency budget at the median,
// and the slowest sample is reported to stderr for the Time indicator.
#[test]
fn every_sample_scans_under_budget() {
    let mut slowest_us = 0;
    let mut slowest_path = "";
    for sample in CORPUS {
        let median = median_latency_us(sample);
        assert!(
            median < LATENCY_BUDGET_US,
            "NFR Time: {} scanned at {median}us/file (budget {LATENCY_BUDGET_US}us)",
            sample.path
        );
        if median > slowest_us {
            slowest_us = median;
            slowest_path = sample.path;
        }
    }
    eprintln!(
        "NFR Time: slowest fixture {slowest_path} at {slowest_us}us/file median \
         over {LATENCY_ITERATIONS} iterations (budget {LATENCY_BUDGET_US}us)"
    );
}

// T-026: the latency budget stays within an order of magnitude of the measured
// slowest median, so a 2-digit (>=10x) regression breaches it instead of
// passing green. Guards against re-widening the budget back to a dead detector
// (the prior 10_000us was ~150x the measurement).
#[test]
fn latency_budget_catches_two_digit_regression() {
    assert!(
        LATENCY_BUDGET_US <= OBSERVED_SLOWEST_US * 10,
        "budget {LATENCY_BUDGET_US}us exceeds 10x the observed {OBSERVED_SLOWEST_US}us \
         slowest median; a 2-digit latency regression would pass undetected"
    );
}

// T-025: prints the precision corpus confusion matrix and derived precision /
// recall to stderr so the Precision indicator is observable. tp+fp+fn+tn must
// equal the corpus size; precision and recall are 1.0 when the fixtures hold.
#[test]
fn precision_corpus_metrics_snapshot() {
    let tally = tally_corpus();
    let precision = ratio(tally.tp, tally.tp + tally.fp);
    let recall = ratio(tally.tp, tally.tp + tally.fn_count);
    eprintln!(
        "Precision corpus: tp={} fn={} fp={} tn={} | precision={precision:.3} recall={recall:.3}",
        tally.tp, tally.fn_count, tally.fp, tally.tn
    );
    let total = tally.tp + tally.fn_count + tally.fp + tally.tn;
    assert_eq!(
        usize::try_from(total).expect("tally fits usize"),
        CORPUS.len(),
        "tally did not classify every sample"
    );
    assert_eq!(tally.fp, 0, "precision corpus has false positives");
    assert_eq!(tally.fn_count, 0, "precision corpus has false negatives");
}

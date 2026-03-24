//! Regex to n-gram query plan decomposition.
//!
//! Parses a regex pattern into an HIR, extracts literal strings, converts
//! each literal into trigram lookups, and combines them into a boolean
//! query plan (AND/OR).

use anyhow::Result;
use regex_syntax::hir::{Hir, HirKind};

use crate::ngram::hash_ngram;

/// A query plan that describes which n-gram lookups to perform and how to
/// combine them.
#[derive(Debug, Clone)]
pub enum QueryPlan {
    /// Look up a single n-gram hash, with the original trigram bytes for display.
    Lookup {
        hash: u64,
        trigram: Vec<u8>,
    },
    /// ALL sub-plans must match (intersection of posting lists).
    And(Vec<QueryPlan>),
    /// ANY sub-plan must match (union of posting lists).
    Or(Vec<QueryPlan>),
    /// No filtering possible — must scan all files.
    FullScan,
}

/// An intermediate representation of literals extracted from the HIR, before
/// they are converted into n-gram lookups.
enum LiteralExpr {
    /// A single literal byte sequence.
    Lit(Vec<u8>),
    /// All sub-expressions must be present (from Concat).
    And(Vec<LiteralExpr>),
    /// Any sub-expression can match (from Alternation).
    Or(Vec<LiteralExpr>),
}

/// Build a query plan from a regex pattern.
///
/// If `case_insensitive` is true, the regex is parsed with case insensitivity
/// and n-grams are generated for lowercase.
pub fn build_query_plan(pattern: &str, case_insensitive: bool) -> Result<QueryPlan> {
    let hir = regex_syntax::ParserBuilder::new()
        .build()
        .parse(pattern)?;

    let expr = extract_literal_expr(&hir);
    let plan = literal_expr_to_plan(expr, case_insensitive);
    Ok(simplify(plan))
}

/// Recursively extract literal expressions from the HIR tree.
///
/// Unlike the built-in `Extractor` which only pulls prefixes/suffixes, this
/// walks the full HIR and extracts every literal segment individually,
/// preserving the AND/OR structure from Concat/Alternation.
fn extract_literal_expr(hir: &Hir) -> LiteralExpr {
    match hir.kind() {
        HirKind::Literal(lit) => LiteralExpr::Lit(lit.0.to_vec()),
        HirKind::Concat(parts) => {
            let subs: Vec<LiteralExpr> = parts
                .iter()
                .map(extract_literal_expr)
                .collect();
            LiteralExpr::And(subs)
        }
        HirKind::Capture(cap) => extract_literal_expr(&cap.sub),
        HirKind::Alternation(alts) => {
            let subs: Vec<LiteralExpr> = alts
                .iter()
                .map(extract_literal_expr)
                .collect();
            LiteralExpr::Or(subs)
        }
        // Class, Repetition, Look, Empty — no literals extractable.
        _ => LiteralExpr::And(Vec::new()),
    }
}

/// Convert a `LiteralExpr` tree into a `QueryPlan`.
fn literal_expr_to_plan(expr: LiteralExpr, case_insensitive: bool) -> QueryPlan {
    match expr {
        LiteralExpr::Lit(bytes) => literal_to_plan(&bytes, case_insensitive),
        LiteralExpr::And(subs) => {
            let plans: Vec<QueryPlan> = subs
                .into_iter()
                .map(|s| literal_expr_to_plan(s, case_insensitive))
                .collect();
            QueryPlan::And(plans)
        }
        LiteralExpr::Or(subs) => {
            let plans: Vec<QueryPlan> = subs
                .into_iter()
                .map(|s| literal_expr_to_plan(s, case_insensitive))
                .collect();
            QueryPlan::Or(plans)
        }
    }
}

/// Convert a single literal byte string into a query plan.
///
/// Decomposes the literal into trigrams (sliding window), then ANDs the
/// lookups together.
///
/// Literals shorter than 3 bytes cannot produce trigrams → FullScan.
fn literal_to_plan(bytes: &[u8], _case_insensitive: bool) -> QueryPlan {
    if bytes.len() < 3 {
        return QueryPlan::FullScan;
    }

    // Lowercase for consistent hashing with the index.
    let lower: Vec<u8> = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();

    let num_trigrams = lower.len() - 2;
    if num_trigrams == 0 {
        return QueryPlan::FullScan;
    }

    // Deduplicate hashes — the same hash can appear from overlapping grams.
    // We keep the FIRST occurrence (lowest offset) for each unique hash.
    let mut seen = std::collections::HashSet::new();
    let lookups: Vec<QueryPlan> = (0..num_trigrams)
        .filter_map(|i| {
            let trigram = &lower[i..i + 3];
            let h = hash_ngram(trigram);
            if !seen.insert(h) {
                return None;
            }
            Some(QueryPlan::Lookup {
                hash: h,
                trigram: trigram.to_vec(),
            })
        })
        .collect();

    if lookups.is_empty() {
        QueryPlan::FullScan
    } else if lookups.len() == 1 {
        lookups.into_iter().next().unwrap()
    } else {
        QueryPlan::And(lookups)
    }
}

/// Simplify a query plan by flattening trivial nodes.
fn simplify(plan: QueryPlan) -> QueryPlan {
    match plan {
        QueryPlan::And(subs) => {
            // Recursively simplify children first.
            let simplified: Vec<QueryPlan> = subs
                .into_iter()
                .map(simplify)
                .filter(|p| !matches!(p, QueryPlan::And(v) if v.is_empty()))
                .collect();

            // If any child is FullScan in an OR → FullScan.
            // In AND: drop FullScans (they don't constrain), unless all are FullScan.
            let non_fullscan: Vec<QueryPlan> = simplified
                .into_iter()
                .filter(|p| !matches!(p, QueryPlan::FullScan))
                .collect();

            if non_fullscan.is_empty() {
                QueryPlan::FullScan
            } else if non_fullscan.len() == 1 {
                non_fullscan.into_iter().next().unwrap()
            } else {
                QueryPlan::And(non_fullscan)
            }
        }
        QueryPlan::Or(subs) => {
            let simplified: Vec<QueryPlan> = subs
                .into_iter()
                .map(simplify)
                .collect();

            // If any child is FullScan in OR → FullScan.
            if simplified.iter().any(|p| matches!(p, QueryPlan::FullScan)) {
                return QueryPlan::FullScan;
            }

            // Filter empty ANDs.
            let filtered: Vec<QueryPlan> = simplified
                .into_iter()
                .filter(|p| !matches!(p, QueryPlan::And(v) if v.is_empty()))
                .collect();

            if filtered.is_empty() {
                QueryPlan::FullScan
            } else if filtered.len() == 1 {
                filtered.into_iter().next().unwrap()
            } else {
                QueryPlan::Or(filtered)
            }
        }
        other => other,
    }
}

/// Return a human-readable explanation of the query plan.
pub fn explain_plan(plan: &QueryPlan) -> String {
    explain_inner(plan, 0)
}

fn explain_inner(plan: &QueryPlan, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    match plan {
        QueryPlan::Lookup { hash, trigram, .. } => {
            let text = String::from_utf8_lossy(trigram);
            format!("{pad}lookup \"{text}\" → {hash:#018x}")
        }
        QueryPlan::And(subs) => {
            let mut out = format!("{pad}AND(\n");
            for s in subs {
                out.push_str(&explain_inner(s, indent + 1));
                out.push('\n');
            }
            out.push_str(&format!("{pad})"));
            out
        }
        QueryPlan::Or(subs) => {
            let mut out = format!("{pad}OR(\n");
            for s in subs {
                out.push_str(&explain_inner(s, indent + 1));
                out.push('\n');
            }
            out.push_str(&format!("{pad})"));
            out
        }
        QueryPlan::FullScan => format!("{pad}FULL_SCAN"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: assert the plan is not FullScan.
    fn assert_not_fullscan(plan: &QueryPlan) {
        assert!(
            !matches!(plan, QueryPlan::FullScan),
            "expected a non-FullScan plan, got FullScan"
        );
    }

    /// Helper: count total Lookup nodes in a plan.
    fn count_lookups(plan: &QueryPlan) -> usize {
        match plan {
            QueryPlan::Lookup { .. } => 1,
            QueryPlan::And(subs) | QueryPlan::Or(subs) => {
                subs.iter().map(count_lookups).sum()
            }
            QueryPlan::FullScan => 0,
        }
    }

    /// Helper: check if plan is an And at the top level.
    fn is_and(plan: &QueryPlan) -> bool {
        matches!(plan, QueryPlan::And(_))
    }

    /// Helper: check if plan is an Or at the top level.
    fn is_or(plan: &QueryPlan) -> bool {
        matches!(plan, QueryPlan::Or(_))
    }

    // 1. Short literal — "hello" (5 chars) → trigram lookups
    #[test]
    fn short_literal_produces_lookups() {
        let plan = build_query_plan("hello", false).unwrap();
        assert_not_fullscan(&plan);
        // "hello" → 3 trigrams: "hel", "ell", "llo"
        assert_eq!(count_lookups(&plan), 3);
    }

    // 1b. Long literal → AND of stable interior n-gram lookups
    #[test]
    fn long_literal_produces_lookups() {
        let plan = build_query_plan("parse_token_stream", false).unwrap();
        assert_not_fullscan(&plan);
        assert!(count_lookups(&plan) >= 1);
    }

    // 2. Alternation of short literals — "foo|bar" → OR of lookups
    #[test]
    fn alternation_short_produces_or() {
        let plan = build_query_plan("foo|bar", false).unwrap();
        assert_not_fullscan(&plan);
        assert!(is_or(&plan), "expected OR at top level, got: {plan:?}");
    }

    // 2b. Alternation of long literals → OR
    #[test]
    fn alternation_long_literals() {
        let plan = build_query_plan("parse_token_stream|compile_ast_node", false).unwrap();
        assert_not_fullscan(&plan);
        assert!(is_or(&plan), "expected OR at top level, got: {plan:?}");
    }

    // 3. Concatenation with wildcards and short literals → AND of lookups
    #[test]
    fn concat_short_literals_produces_and() {
        let plan = build_query_plan("foo.*bar", false).unwrap();
        assert_not_fullscan(&plan);
        // "foo" → 1 trigram, "bar" → 1 trigram → AND of 2 lookups
        assert!(is_and(&plan), "expected AND at top level, got: {plan:?}");
        assert_eq!(count_lookups(&plan), 2);
    }

    // 3b. Concatenation with long literals → AND of lookups
    #[test]
    fn concat_long_literals() {
        let plan = build_query_plan("parse_token.*compile_ast", false).unwrap();
        assert_not_fullscan(&plan);
        assert!(is_and(&plan), "expected AND at top level, got: {plan:?}");
        assert!(count_lookups(&plan) >= 2);
    }

    // 4. Pure wildcard — ".*" → FullScan
    #[test]
    fn pure_wildcard() {
        let plan = build_query_plan(".*", false).unwrap();
        assert!(
            matches!(plan, QueryPlan::FullScan),
            "expected FullScan for '.*', got: {plan:?}"
        );
    }

    // 5. Case insensitive — both produce the same trigram lookups
    #[test]
    fn case_insensitive() {
        let plan_ci = build_query_plan("Hello", true).unwrap();
        let plan_lower = build_query_plan("hello", false).unwrap();
        // Both produce identical lowercased trigram lookups
        let explain_ci = explain_plan(&plan_ci);
        let explain_lower = explain_plan(&plan_lower);
        assert_eq!(explain_ci, explain_lower);
    }

    // 5b. Case insensitive long literal — produces lookups
    #[test]
    fn case_insensitive_long() {
        let plan_ci = build_query_plan("ParseTokenStream", true).unwrap();
        let plan_lower = build_query_plan("parsetokenstream", false).unwrap();
        assert_not_fullscan(&plan_ci);
        assert_eq!(explain_plan(&plan_ci), explain_plan(&plan_lower));
    }

    // 6. Character class — "[abc]def" → "def" is 3 chars → 1 trigram lookup
    #[test]
    fn character_class_with_short_literal() {
        let plan = build_query_plan("[abc]def", false).unwrap();
        assert_not_fullscan(&plan);
        assert!(count_lookups(&plan) >= 1);
    }

    // 7. Complex regex — `fn\s+parse_(\w+)` → "fn" is 2 bytes (below min_len=3,
    //    no n-gram), only "parse_" produces lookups.
    #[test]
    fn complex_regex() {
        let plan = build_query_plan(r"fn\s+parse_(\w+)", false).unwrap();
        assert_not_fullscan(&plan);
        assert!(
            count_lookups(&plan) >= 1,
            "expected at least 1 lookup from 'parse_', got: {}",
            count_lookups(&plan)
        );
    }

    // 8. explain_plan output — verify it produces readable output (long literals)
    #[test]
    fn explain_plan_output() {
        let plan = build_query_plan("parse_token.*compile_ast", false).unwrap();
        let explanation = explain_plan(&plan);
        assert!(explanation.contains("AND(") || explanation.contains("lookup"));
        assert!(explanation.contains("0x"), "should contain hex hashes");
    }

    #[test]
    fn empty_pattern_fullscan() {
        let plan = build_query_plan("", false).unwrap();
        assert!(matches!(plan, QueryPlan::FullScan));
    }

    #[test]
    fn single_char_class_fullscan() {
        let plan = build_query_plan("[a-z]", false).unwrap();
        assert!(matches!(plan, QueryPlan::FullScan));
    }

    #[test]
    fn explain_fullscan() {
        let plan = QueryPlan::FullScan;
        assert_eq!(explain_plan(&plan), "FULL_SCAN");
    }

    #[test]
    fn explain_single_lookup() {
        let plan = QueryPlan::Lookup {
            hash: 0xdeadbeef,
            trigram: b"abc".to_vec(),
        };
        let out = explain_plan(&plan);
        assert!(out.contains("0x00000000deadbeef"));
        assert!(out.contains("\"abc\""));
    }

    #[test]
    fn deterministic_plans() {
        let a = build_query_plan("foo.*bar", false).unwrap();
        let b = build_query_plan("foo.*bar", false).unwrap();
        assert_eq!(explain_plan(&a), explain_plan(&b));
    }

    // "fn" is only 2 bytes — below MIN_NGRAM_LEN (3), so it can't produce
    // n-gram lookups.  "parse_" (6 bytes) should still produce lookups.
    #[test]
    fn short_literal_handled() {
        let plan = build_query_plan("fn", false).unwrap();
        // "fn" is 2 bytes, below min n-gram length → FullScan
        assert!(matches!(plan, QueryPlan::FullScan));
    }

    #[test]
    fn long_literal() {
        let plan =
            build_query_plan("this_is_a_very_long_literal_string", false).unwrap();
        assert_not_fullscan(&plan);
        assert!(count_lookups(&plan) >= 2);
    }

    #[test]
    fn nested_groups_short_produces_lookups() {
        // "foo" and "bar" each produce 1 trigram → AND of 2 lookups
        let plan = build_query_plan("(foo)(bar)", false).unwrap();
        assert_not_fullscan(&plan);
        assert_eq!(count_lookups(&plan), 2);
    }

    #[test]
    fn nested_groups_long() {
        let plan = build_query_plan("(parse_token)(compile_ast)", false).unwrap();
        assert_not_fullscan(&plan);
        assert!(count_lookups(&plan) >= 2);
    }

    #[test]
    fn alternation_with_shared_suffix_short_produces_lookups() {
        // Each 3-char literal produces 1 trigram → AND(OR(foo, bar), baz)
        let plan = build_query_plan("(foo|bar)baz", false).unwrap();
        assert_not_fullscan(&plan);
        assert!(count_lookups(&plan) >= 2);
    }
}

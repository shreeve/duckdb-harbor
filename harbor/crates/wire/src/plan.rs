//! An EXPLAIN's answer: how a client knows one, and the text it reads as.
//!
//! The engine answers EXPLAIN with rows of a drawing, one plan to a row. In a
//! grid cell a drawing is a row of `\n`s cut off at the column's edge, so the
//! harbor CLI and DuckTable both show a plan as text, the way DuckDB's own
//! shell prints it.

/// Whether a result is an EXPLAIN: exactly the two text columns the engine
/// names `explain_key` and `explain_value`, which nothing else produces. A
/// client sees the schema, not the statement, and the schema is signature
/// enough.
pub fn is_plan<S: AsRef<str>>(columns: &[S]) -> bool {
    matches!(columns, [key, value]
        if key.as_ref().eq_ignore_ascii_case("explain_key")
            && value.as_ref().eq_ignore_ascii_case("explain_value"))
}

/// The text an EXPLAIN's rows (key, plan) read as: each plan verbatim,
/// ending in a newline. One plan reads bare, as DuckDB's shell prints it.
/// Several — the logical and physical plans under `explain_output = 'all'`,
/// or an analyzed plan beside its physical one — each get a one-line label
/// in the shell's words, since the drawings do not say which is which. None
/// when the rows are not plans.
pub fn text<S: AsRef<str>>(rows: &[Vec<S>]) -> Option<String> {
    if rows.is_empty() || rows.iter().any(|r| r.len() != 2) {
        return None;
    }
    let mut out = String::new();
    for row in rows {
        let (key, plan) = (row[0].as_ref(), row[1].as_ref());
        if rows.len() > 1 {
            out.push_str(match key {
                "logical_plan" => "Unoptimized Logical Plan",
                "logical_opt" => "Optimized Logical Plan",
                "physical_plan" => "Physical Plan",
                "analyzed_plan" => "Analyzed Plan",
                other => other,
            });
            out.push('\n');
        }
        out.push_str(plan);
        if !plan.ends_with('\n') {
            out.push('\n');
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_is_known_by_its_two_columns() {
        assert!(is_plan(&["explain_key", "explain_value"]));
        assert!(is_plan(&["EXPLAIN_KEY", "Explain_Value"]));
        assert!(!is_plan(&["explain_key"]));
        assert!(!is_plan(&["explain_key", "explain_value", "x"]));
        assert!(!is_plan(&["k", "v"]));
    }

    #[test]
    fn a_plan_reads_whole_and_only_a_set_is_labelled() {
        let plan = "╭─ Projection ───╮\n│ Projections: a │\n╰────────────────╯";
        assert_eq!(text(&[vec!["physical_plan", plan]]).unwrap(), format!("{plan}\n"));
        let two = text(&[vec!["logical_opt", "L"], vec!["physical_plan", "P\n"]]).unwrap();
        assert_eq!(two, "Optimized Logical Plan\nL\nPhysical Plan\nP\n");
        assert_eq!(text::<&str>(&[]), None);
        assert_eq!(text(&[vec!["only one cell"]]), None);
    }
}

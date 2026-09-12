//! Translating a [`ScanRequest`] into SQL.
//!
//! This module is the second of the three oracles plan §6 asks for: the same
//! query, answered by SQLite instead of by the incremental graph. That only
//! proves anything if the translation is *exact*, so most of what follows is
//! about the places where SQL's obvious spelling and the engine's semantics
//! quietly disagree.
//!
//! # Columns are declared with no type, on purpose
//!
//! SQLite applies **type affinity** before comparing: in a column declared
//! `TEXT`, the predicate `c > 5` converts `5` to `'5'` and compares as text.
//! The engine has no such rule — [`Value::sql_cmp`] compares by storage class
//! and then by value, full stop.
//!
//! A column declared with no type at all has BLOB affinity, which SQLite
//! documents as "no affinity": values are stored exactly as supplied and
//! comparisons convert nothing. That makes SQLite's comparison and `sql_cmp`
//! the same function, which is the only reason this oracle is worth having.
//! See [`crate::ddl`].
//!
//! # Where the spellings diverge
//!
//! * **`LIKE` is not `LikePrefix`.** SQLite's `LIKE` is case-insensitive for
//!   ASCII and coerces numbers to text; the engine's is a byte-wise
//!   `starts_with` that returns *false*, not a match, for a non-text operand.
//!   So this emits a `substr` over the value cast to `BLOB`, which compares
//!   bytes — and bytes, not characters, is what `starts_with` compares.
//! * **`IN ()` is a syntax error** in SQLite, while the engine answers an empty
//!   list with false (or unknown, for a NULL left-hand side). Spelled out as a
//!   `CASE`.
//! * **Cursor comparison has to be NULL-aware.** `c > ?` is unknown when either
//!   side is NULL, so the natural keyset-pagination chain silently drops every
//!   row with a NULL sort key. See [`cursor_sql`].
//!
//! Everything else lines up without help: SQL's three-valued `AND`/`OR`/`NOT`
//! matches [`Tri`], a `WHERE` that evaluates to NULL excludes the row just as
//! `Tri::is_true` does, and `ORDER BY ... ASC` puts NULLs first exactly like
//! the engine's sort.
//!
//! [`Tri`]: solstice_ivm::Tri

use solstice_ivm::order::Cursor;
use solstice_ivm::{CmpOp, ColId, Dir, Expr, Params, Predicate, ScanRequest, Schema, Value};

/// A generated statement and the values to bind to it, positionally.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlScan {
    pub sql: String,
    pub binds: Vec<Value>,
}

/// Build the `SELECT` that answers `req` against `schema`.
pub fn scan_sql(schema: &Schema, req: &ScanRequest) -> SqlScan {
    let mut b = Builder::default();

    let cols = (0..schema.arity())
        .map(|i| quote_ident(&schema.columns[i].name))
        .collect::<Vec<_>>()
        .join(", ");

    let mut sql = format!("SELECT {cols} FROM {}", quote_ident(&schema.name));

    let mut conds: Vec<String> = Vec::new();
    if let Some(filter) = &req.filter {
        conds.push(predicate_sql(schema, filter, &req.params, &mut b));
    }
    if let Some(cursor) = &req.after {
        conds.push(cursor_sql(schema, cursor, &req.order, &mut b));
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }

    sql.push_str(" ORDER BY ");
    sql.push_str(&order_sql(schema, &req.order));

    // SQLite reads a negative `LIMIT` as *no* bound, and that is the only way
    // to say "unbounded" in a statement whose limit is a bind. A `Source`
    // hydrating a whole table asks for `usize::MAX`, which is exactly that
    // case. Written as a conversion rather than left to `as i64` — which lands
    // on -1 by arithmetic accident — so the intent survives the next edit.
    let limit = i64::try_from(req.limit).unwrap_or(-1);
    sql.push_str(" LIMIT ?");
    b.binds.push(Value::Int(limit));

    SqlScan {
        sql,
        binds: b.binds,
    }
}

/// Accumulates bind values as the SQL is assembled, so the two can never drift
/// out of step.
#[derive(Default)]
struct Builder {
    binds: Vec<Value>,
}

impl Builder {
    /// Emit a placeholder and remember what goes in it.
    fn bind(&mut self, v: Value) -> String {
        self.binds.push(v);
        "?".to_string()
    }
}

/// `ORDER BY <sort columns>, <pk> ASC`.
///
/// The trailing primary key is not decoration: sort keys tie, and a scan whose
/// order disagrees with the engine's [`cmp_entry`] by even one tied pair will
/// hand a cursor-resumed refill either a duplicate row or a missing one.
///
/// NULL placement needs no `NULLS FIRST`/`NULLS LAST` clause, because SQLite's
/// default — NULLs first ascending, last descending — is already what the
/// engine's sort does.
///
/// [`cmp_entry`]: solstice_ivm::order::cmp_entry
fn order_sql(schema: &Schema, order: &[(ColId, Dir)]) -> String {
    let mut parts: Vec<String> = order
        .iter()
        .map(|(col, dir)| {
            let dir = match dir {
                Dir::Asc => "ASC",
                Dir::Desc => "DESC",
            };
            format!("{} {dir}", col_sql(schema, *col))
        })
        .collect();
    parts.push(format!("{} ASC", col_sql(schema, schema.pk)));
    parts.join(", ")
}

/// Rows strictly after `cursor`, under the same total order as [`order_sql`].
///
/// The shape is the usual keyset-pagination chain — first column strictly
/// after, or equal and the second strictly after, and so on, with the primary
/// key breaking the final tie — but every comparison in it has to be spelled
/// NULL-aware.
///
/// The naive `c > ?` is unknown whenever either side is NULL, so a plain chain
/// excludes every row with a NULL sort key. That is not a rare edge: a nullable
/// `assigned_at` sorts a whole column of NULLs together, and they would vanish
/// from page two onwards while page one showed them. Instead each comparison
/// branches on whether the *cursor's* value is NULL, which is known here at
/// build time, and asks SQLite only about the column.
fn cursor_sql(schema: &Schema, cursor: &Cursor, order: &[(ColId, Dir)], b: &mut Builder) -> String {
    let mut alternatives: Vec<String> = Vec::new();

    for i in 0..order.len() {
        let mut conj: Vec<String> = Vec::new();
        // Tied on every earlier column...
        for (j, (col, _)) in order.iter().enumerate().take(i) {
            conj.push(same_position(schema, *col, cursor_value(cursor, j), b));
        }
        // ...and strictly past on this one.
        let (col, dir) = order[i];
        conj.push(strictly_after(schema, col, dir, cursor_value(cursor, i), b));
        alternatives.push(format!("({})", conj.join(" AND ")));
    }

    // Tied the whole way down: the primary key decides, always ascending.
    let mut conj: Vec<String> = Vec::new();
    for (j, (col, _)) in order.iter().enumerate() {
        conj.push(same_position(schema, *col, cursor_value(cursor, j), b));
    }
    let pk = col_sql(schema, schema.pk);
    let bind = b.bind(cursor.pk.value().clone());
    conj.push(format!("{pk} > {bind}"));
    alternatives.push(format!("({})", conj.join(" AND ")));

    format!("({})", alternatives.join(" OR "))
}

/// A cursor may carry fewer sort values than the order has columns; the engine
/// reads the missing ones as NULL, so this does too.
fn cursor_value(cursor: &Cursor, i: usize) -> &Value {
    cursor.sort_key.get(i).unwrap_or(&Value::Null)
}

/// "Occupies the same position in the sort order as `v`" — NULL included.
///
/// `IS` rather than `=` because two NULLs tie in the engine's sort, while
/// `NULL = NULL` is unknown. On non-NULL operands `IS` behaves exactly like
/// `=`, including comparing `1` equal to `1.0`.
fn same_position(schema: &Schema, col: ColId, v: &Value, b: &mut Builder) -> String {
    let c = col_sql(schema, col);
    let bind = b.bind(v.clone());
    format!("{c} IS {bind}")
}

/// "Sorts strictly after `v`", under `dir`, with NULLs where the engine puts
/// them: first ascending, last descending.
fn strictly_after(schema: &Schema, col: ColId, dir: Dir, v: &Value, b: &mut Builder) -> String {
    let c = col_sql(schema, col);
    match (dir, v.is_null()) {
        // Ascending, cursor at NULL: NULLs come first, so anything non-NULL is
        // past it — and another NULL ties rather than following.
        (Dir::Asc, true) => format!("{c} IS NOT NULL"),
        (Dir::Asc, false) => {
            let bind = b.bind(v.clone());
            format!("({c} IS NOT NULL AND {c} > {bind})")
        }
        // Descending, cursor at NULL: NULLs come last, so nothing follows.
        (Dir::Desc, true) => "0".to_string(),
        (Dir::Desc, false) => {
            let bind = b.bind(v.clone());
            format!("({c} IS NULL OR {c} < {bind})")
        }
    }
}

/// A column reference, or `NULL` for a column the table does not have.
///
/// Out-of-range reads are legal: a projection can shrink a row and leave a
/// predicate pointing past the end, and [`Row::get`] answers NULL rather than
/// panicking. The SQL has to answer NULL too, or the oracle would disagree with
/// the engine on exactly the inputs a fuzzer finds first.
///
/// [`Row::get`]: solstice_ivm::Row::get
fn col_sql(schema: &Schema, col: ColId) -> String {
    match schema.columns.get(col as usize) {
        Some(c) => quote_ident(&c.name),
        None => "NULL".to_string(),
    }
}

fn expr_sql(schema: &Schema, e: &Expr, params: &Params, b: &mut Builder) -> String {
    match e {
        Expr::Col(c) => col_sql(schema, *c),
        Expr::Lit(v) => b.bind(v.clone()),
        // Parameters are bound by the time a scan runs, so they become ordinary
        // binds. An out-of-range one reads NULL, matching `Params::get`.
        Expr::Param(i) => b.bind(params.get(*i).clone()),
    }
}

fn cmp_op_sql(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "<>",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

/// Translate a predicate, preserving three-valued logic.
///
/// SQL is already three-valued and `WHERE` already drops unknown rows, so
/// `AND`, `OR`, `NOT` and the comparisons map straight across. The three
/// exceptions are called out in the module docs.
fn predicate_sql(schema: &Schema, p: &Predicate, params: &Params, b: &mut Builder) -> String {
    match p {
        Predicate::True => "1".to_string(),

        Predicate::Cmp { lhs, op, rhs } => {
            let l = expr_sql(schema, lhs, params, b);
            let r = expr_sql(schema, rhs, params, b);
            format!("({l} {} {r})", cmp_op_sql(*op))
        }

        Predicate::In { lhs, list } => {
            let l = expr_sql(schema, lhs, params, b);
            if list.is_empty() {
                // `x IN ()` will not parse. The engine answers false, or
                // unknown when the left side is NULL, so say that directly.
                return format!("(CASE WHEN {l} IS NULL THEN NULL ELSE 0 END)");
            }
            let items = list
                .iter()
                .map(|v| b.bind(v.clone()))
                .collect::<Vec<_>>()
                .join(", ");
            format!("({l} IN ({items}))")
        }

        Predicate::Between { lhs, low, high } => {
            // Spelled out rather than using SQL `BETWEEN` so the left-hand
            // expression's binds appear in the order the builder emits them.
            let l1 = expr_sql(schema, lhs, params, b);
            let lo = expr_sql(schema, low, params, b);
            let l2 = expr_sql(schema, lhs, params, b);
            let hi = expr_sql(schema, high, params, b);
            format!("(({l1} >= {lo}) AND ({l2} <= {hi}))")
        }

        Predicate::IsNull(e) => {
            let x = expr_sql(schema, e, params, b);
            format!("({x} IS NULL)")
        }
        Predicate::IsNotNull(e) => {
            let x = expr_sql(schema, e, params, b);
            format!("({x} IS NOT NULL)")
        }

        Predicate::LikePrefix { lhs, prefix } => {
            // Not `LIKE`: that is case-insensitive for ASCII and coerces
            // numbers to text. The engine does a byte-wise `starts_with` on
            // text and returns false for anything else, so compare the leading
            // bytes of the value cast to a blob.
            //
            // The left-hand side appears several times, so it is *built* that
            // many times. Re-using one string would emit several `?`
            // placeholders behind a single bind whenever the operand is a
            // literal or a parameter, and every later bind in the statement
            // would shift — silently answering a different question. Column
            // operands never bind, so the repetition costs nothing in practice.
            let l1 = expr_sql(schema, lhs, params, b);
            let l2 = expr_sql(schema, lhs, params, b);

            if prefix.is_empty() {
                // `starts_with("")` is true for every string, and `substr`
                // below cannot say so — see the length guard. Nothing but text
                // matches, still: a number is false, not coerced.
                return format!(
                    "(CASE WHEN {l1} IS NULL THEN NULL ELSE typeof({l2}) = 'text' END)"
                );
            }

            // `substr` returns NULL, not an empty blob, when handed a
            // zero-length blob — so a text value of `""` would come back
            // unknown and drop out of the view, where the engine says a plain
            // false. Any value shorter than the prefix cannot match anyway, so
            // ruling those out first both fixes the NULL and skips the work.
            let l3 = expr_sql(schema, lhs, params, b);
            let len1 = b.bind(Value::Int(prefix.len() as i64));
            let l4 = expr_sql(schema, lhs, params, b);
            let len2 = b.bind(Value::Int(prefix.len() as i64));
            let pat = b.bind(Value::text(&**prefix));
            format!(
                "(CASE WHEN {l1} IS NULL THEN NULL \
                 WHEN typeof({l2}) <> 'text' THEN 0 \
                 WHEN length(CAST({l3} AS BLOB)) < {len1} THEN 0 \
                 ELSE substr(CAST({l4} AS BLOB), 1, {len2}) = CAST({pat} AS BLOB) END)"
            )
        }

        Predicate::And(preds) => {
            if preds.is_empty() {
                return "1".to_string();
            }
            let parts = preds
                .iter()
                .map(|p| predicate_sql(schema, p, params, b))
                .collect::<Vec<_>>()
                .join(" AND ");
            format!("({parts})")
        }

        Predicate::Or(preds) => {
            if preds.is_empty() {
                return "0".to_string();
            }
            let parts = preds
                .iter()
                .map(|p| predicate_sql(schema, p, params, b))
                .collect::<Vec<_>>()
                .join(" OR ");
            format!("({parts})")
        }

        Predicate::Not(inner) => {
            let x = predicate_sql(schema, inner, params, b);
            format!("(NOT {x})")
        }
    }
}

/// Quote an identifier for SQLite, doubling any embedded quote.
///
/// M0's schema is hardcoded, but the schema DSL in M1 takes names from user
/// input and this is the boundary they cross.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solstice_ivm::{Column, ValueType};

    fn schema() -> Schema {
        Schema::new(
            1,
            "issues",
            vec![
                Column::new("id", ValueType::Int),
                Column::new("priority", ValueType::Int).nullable(),
                Column::new("title", ValueType::Text),
            ],
            0,
        )
    }

    fn req() -> ScanRequest {
        ScanRequest {
            table: 1,
            order: vec![(1, Dir::Desc)],
            after: None,
            filter: None,
            params: Params::empty(),
            limit: 10,
        }
    }

    fn pred(p: Predicate) -> SqlScan {
        scan_sql(
            &schema(),
            &ScanRequest {
                filter: Some(p),
                ..req()
            },
        )
    }

    #[test]
    fn the_primary_key_always_breaks_the_sort_tie_ascending() {
        let out = scan_sql(&schema(), &req());
        assert!(
            out.sql.contains(r#"ORDER BY "priority" DESC, "id" ASC"#),
            "{}",
            out.sql
        );
    }

    #[test]
    fn an_empty_in_list_becomes_a_case_rather_than_invalid_sql() {
        let out = pred(Predicate::In {
            lhs: Expr::Col(1),
            list: Vec::new(),
        });
        assert!(out.sql.contains("CASE WHEN"), "{}", out.sql);
        assert!(!out.sql.contains("IN ()"), "{}", out.sql);
    }

    #[test]
    fn a_prefix_match_compares_bytes_not_a_like_pattern() {
        let out = pred(Predicate::LikePrefix {
            lhs: Expr::Col(2),
            prefix: "ab".into(),
        });
        assert!(!out.sql.to_uppercase().contains("LIKE"), "{}", out.sql);
        assert!(out.sql.contains("CAST"), "{}", out.sql);
        // Two length binds — one for the guard, one for the `substr` — then the
        // pattern, then the scan's own limit.
        assert_eq!(
            out.binds,
            vec![
                Value::Int(2),
                Value::Int(2),
                Value::text("ab"),
                Value::Int(10)
            ]
        );
    }

    /// `substr` answers NULL, not an empty blob, when its input blob is
    /// zero-length. Without a guard that turns a text value of `""` into
    /// *unknown*, and a row the engine plainly rejects instead vanishes from a
    /// `NOT` — found by `tests/oracle.rs`, not by reading the documentation.
    #[test]
    fn a_value_shorter_than_the_prefix_is_false_rather_than_unknown() {
        let out = pred(Predicate::LikePrefix {
            lhs: Expr::Col(2),
            prefix: "ab".into(),
        });
        assert!(out.sql.contains("length(CAST"), "{}", out.sql);
    }

    /// Every string starts with the empty prefix, and `substr` cannot say so.
    #[test]
    fn an_empty_prefix_matches_every_text_value_and_nothing_else() {
        let out = pred(Predicate::LikePrefix {
            lhs: Expr::Col(2),
            prefix: "".into(),
        });
        assert!(out.sql.contains("typeof"), "{}", out.sql);
        assert!(!out.sql.contains("substr"), "{}", out.sql);
    }

    /// A literal on the left binds once per occurrence. Reusing one built
    /// string would emit more placeholders than binds and shift every later
    /// value in the statement.
    #[test]
    fn a_prefix_match_on_a_literal_binds_once_per_placeholder() {
        let out = pred(Predicate::LikePrefix {
            lhs: Expr::Lit(Value::text("abc")),
            prefix: "ab".into(),
        });
        assert_eq!(out.sql.matches('?').count(), out.binds.len(), "{}", out.sql);
    }

    #[test]
    fn an_unbounded_limit_becomes_a_negative_limit() {
        let out = scan_sql(
            &schema(),
            &ScanRequest {
                limit: usize::MAX,
                ..req()
            },
        );
        assert_eq!(out.binds.last(), Some(&Value::Int(-1)));
    }

    #[test]
    fn a_column_the_table_does_not_have_reads_as_null() {
        let out = pred(Predicate::IsNull(Expr::Col(99)));
        assert!(out.sql.contains("(NULL IS NULL)"), "{}", out.sql);
    }

    #[test]
    fn binds_come_out_in_the_order_the_placeholders_appear() {
        let out = pred(Predicate::And(vec![
            Predicate::eq(0, 1i64),
            Predicate::eq(1, 2i64),
        ]));
        assert_eq!(
            out.binds,
            vec![Value::Int(1), Value::Int(2), Value::Int(10)],
            "{}",
            out.sql
        );
    }

    #[test]
    fn a_descending_cursor_at_null_admits_nothing() {
        // NULLs sort last descending, so there is no row after one.
        let out = scan_sql(
            &schema(),
            &ScanRequest {
                after: Some(Cursor {
                    sort_key: vec![Value::Null],
                    pk: solstice_ivm::RowKey::from(5),
                }),
                ..req()
            },
        );
        assert!(out.sql.contains("(0)"), "{}", out.sql);
    }

    #[test]
    fn an_ascending_cursor_at_null_admits_every_non_null() {
        let out = scan_sql(
            &schema(),
            &ScanRequest {
                order: vec![(1, Dir::Asc)],
                after: Some(Cursor {
                    sort_key: vec![Value::Null],
                    pk: solstice_ivm::RowKey::from(5),
                }),
                ..req()
            },
        );
        assert!(out.sql.contains(r#""priority" IS NOT NULL"#), "{}", out.sql);
    }

    #[test]
    fn a_tie_on_the_sort_key_falls_through_to_the_primary_key() {
        let out = scan_sql(
            &schema(),
            &ScanRequest {
                after: Some(Cursor {
                    sort_key: vec![Value::Int(3)],
                    pk: solstice_ivm::RowKey::from(5),
                }),
                ..req()
            },
        );
        assert!(
            out.sql.contains(r#""priority" IS ? AND "id" > ?"#),
            "{}",
            out.sql
        );
    }

    #[test]
    fn identifiers_with_quotes_are_escaped() {
        assert_eq!(quote_ident(r#"we"ird"#), r#""we""ird""#);
    }
}

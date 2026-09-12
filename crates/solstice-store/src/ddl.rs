//! Creating tables for a [`Schema`].
//!
//! # Columns are declared with no type
//!
//! The generated DDL is `CREATE TABLE "issues" ("id", "priority", "title", ...)`
//! — names only. That looks like an oversight and is the opposite.
//!
//! A SQLite column's declared type gives it an *affinity*, and affinity rewrites
//! values on the way in and converts operands before comparing. In a `TEXT`
//! column, `WHERE c > 5` compares against `'5'`; in an `INTEGER` column, the
//! string `'7'` is stored as the number 7. The engine does neither: it compares
//! by storage class and then by value, and it stores exactly what it was given.
//!
//! A column with no declared type has BLOB affinity, which SQLite's
//! documentation calls "no affinity" — nothing is converted, on either path. So
//! the two agree by construction rather than by testing, and the differential
//! test in `tests/oracle.rs` is checking the translation rather than papering
//! over a semantic gap.
//!
//! The cost is that SQLite will not compact a small integer into a column's
//! declared type, and that `INTEGER PRIMARY KEY` rowid aliasing is unavailable.
//! Both are M0-acceptable; see [`create_table`] for the index story, which is
//! what actually decides whether a refill is O(limit) or O(table).

use crate::sql::quote_ident;
use solstice_ivm::{ColId, Dir, Schema};

/// `CREATE TABLE` for `schema`, with no column affinities.
pub fn create_table(schema: &Schema) -> String {
    let cols = schema
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let name = quote_ident(&c.name);
            // The primary key is declared so SQLite enforces uniqueness and
            // gives us the index a pk tie-break needs. It is deliberately not
            // `INTEGER PRIMARY KEY`: that would make the column a rowid alias,
            // which silently coerces every value to an integer and would undo
            // the whole point of leaving affinities off.
            if i as ColId == schema.pk {
                format!("{name} PRIMARY KEY")
            } else {
                name
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "CREATE TABLE IF NOT EXISTS {} ({cols}) WITHOUT ROWID",
        quote_ident(&schema.name)
    )
}

/// The index that makes a `TopK` refill bounded.
///
/// A refill is `WHERE <filter> AND <cursor> ORDER BY <sort> LIMIT <slack>`. With
/// an index on exactly `(sort columns…, pk)` in the scan's directions, SQLite
/// walks the index from the cursor and stops at the limit. Without it, that
/// same statement sorts the whole table to return three rows — which is
/// precisely the "refill degenerates into unbounded requery" failure plan §7
/// names, arriving through the store rather than through the operator.
///
/// The trailing primary key matters for the same reason it is in a [`Cursor`]:
/// the index has to define the same total order the cursor is compared against,
/// or SQLite will sort after seeking and the bound is lost.
///
/// [`Cursor`]: solstice_ivm::order::Cursor
pub fn create_order_index(schema: &Schema, order: &[(ColId, Dir)]) -> Option<String> {
    if order.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut tag = String::new();
    for (col, dir) in order {
        let c = schema.columns.get(*col as usize)?;
        let dir_sql = match dir {
            Dir::Asc => "ASC",
            Dir::Desc => "DESC",
        };
        parts.push(format!("{} {dir_sql}", quote_ident(&c.name)));
        tag.push_str(&c.name);
        tag.push('_');
        tag.push_str(if matches!(dir, Dir::Asc) { "a" } else { "d" });
        tag.push('_');
    }
    parts.push(format!(
        "{} ASC",
        quote_ident(&schema.columns[schema.pk as usize].name)
    ));

    let name = quote_ident(&format!("idx_{}_{tag}pk", schema.name));
    Some(format!(
        "CREATE INDEX IF NOT EXISTS {name} ON {} ({})",
        quote_ident(&schema.name),
        parts.join(", ")
    ))
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
            ],
            0,
        )
    }

    #[test]
    fn columns_are_declared_without_a_type() {
        let sql = create_table(&schema());
        for ty in ["INTEGER", "TEXT", "REAL", "BLOB", "NUMERIC"] {
            assert!(!sql.contains(ty), "{sql} must not give columns an affinity");
        }
    }

    #[test]
    fn the_primary_key_is_not_a_rowid_alias() {
        let sql = create_table(&schema());
        assert!(sql.contains(r#""id" PRIMARY KEY"#), "{sql}");
        assert!(!sql.contains("INTEGER PRIMARY KEY"), "{sql}");
        assert!(sql.contains("WITHOUT ROWID"), "{sql}");
    }

    #[test]
    fn an_order_index_ends_with_the_primary_key() {
        let sql = create_order_index(&schema(), &[(1, Dir::Desc)]).unwrap();
        assert!(sql.contains(r#"("priority" DESC, "id" ASC)"#), "{sql}");
    }

    #[test]
    fn an_empty_order_needs_no_index() {
        assert_eq!(create_order_index(&schema(), &[]), None);
    }
}

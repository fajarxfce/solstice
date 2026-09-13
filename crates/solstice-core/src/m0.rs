//! The hardcoded M0 world: issues, comments, and the one query that matters.
//!
//! Plan §5.1 fakes everything in the spike except the risk, so there is no
//! schema DSL and no codegen here — two tables written out by hand, and the
//! query from plan §7 that names the project's most likely way to fail:
//!
//! > the top 50 issues by priority in my project, each with its 3 latest
//! > comments
//!
//! It appears twice in this module, and that is deliberate. [`query`] is the
//! protobuf IR a host would send; [`issue_filter`], [`issue_order`] and
//! [`comment_order`] are what it must compile down to. Writing both and then
//! asserting they agree (see this module's tests) is what makes
//! [`compile`](crate::compile) answerable to something other than itself — a
//! compiler checked only against its own output is a compiler that cannot be
//! wrong.

use solstice_ivm::{CmpOp, ColId, Column, Dir, Expr, Predicate, Schema, TableId, ValueType};
use solstice_proto::query as ir;
use solstice_store::{SqliteStore, StoreError};

use crate::catalog::{Catalog, Rel};

pub const ISSUES: TableId = 1;
pub const COMMENTS: TableId = 2;

/// Column ids for `issues`. Named because `row.get(2)` at a call site is a bug
/// waiting for the day someone adds a column in the middle.
pub mod issue {
    use solstice_ivm::ColId;
    pub const ID: ColId = 0;
    pub const PROJECT: ColId = 1;
    pub const PRIORITY: ColId = 2;
    pub const CLOSED: ColId = 3;
    pub const TITLE: ColId = 4;
    pub const UPDATED_AT: ColId = 5;
}

/// Column ids for `comments`.
pub mod comment {
    use solstice_ivm::ColId;
    pub const ID: ColId = 0;
    pub const ISSUE: ColId = 1;
    pub const CREATED_AT: ColId = 2;
    pub const AUTHOR: ColId = 3;
    pub const BODY: ColId = 4;
}

/// The relation name a query traverses: `related("comments")`.
pub const COMMENTS_REL: &str = "comments";

pub fn schemas() -> Vec<Schema> {
    vec![
        Schema::new(
            ISSUES,
            "issues",
            vec![
                Column::new("id", ValueType::Int),
                Column::new("project_id", ValueType::Int),
                Column::new("priority", ValueType::Int).nullable(),
                Column::new("closed", ValueType::Int),
                Column::new("title", ValueType::Text),
                Column::new("updated_at", ValueType::Int),
            ],
            issue::ID,
        ),
        Schema::new(
            COMMENTS,
            "comments",
            vec![
                Column::new("id", ValueType::Int),
                Column::new("issue_id", ValueType::Int),
                Column::new("created_at", ValueType::Int),
                Column::new("author", ValueType::Text),
                Column::new("body", ValueType::Text),
            ],
            comment::ID,
        ),
    ]
}

pub fn catalog() -> Catalog {
    Catalog::new(
        schemas(),
        vec![Rel {
            name: COMMENTS_REL.to_string(),
            parent: ISSUES,
            child: COMMENTS,
            parent_col: issue::ID,
            child_col: comment::ISSUE,
        }],
    )
}

/// Plan §7's query, as the IR a host sends across the byte ABI.
///
/// The project is a **parameter** rather than a literal because plan §1.2 hashes
/// the IR twice — without params for the `PipelineId`, with them for the
/// `ViewId` — so that N users running this query for N different projects share
/// one dataflow pipeline on the server. Folding the project in as a literal
/// would describe a query shape the real system never runs.
pub fn query(project: i64, k: u32, comments: u32) -> ir::Query {
    ir::Query {
        table: "issues".to_string(),
        filter: Some(ir::Predicate::And(vec![
            ir::Predicate::Cmp {
                lhs: ir::Expr::Col("project_id".to_string()),
                op: ir::CmpOp::Eq,
                rhs: ir::Expr::Param(0),
            },
            ir::Predicate::Cmp {
                lhs: ir::Expr::Col("closed".to_string()),
                op: ir::CmpOp::Eq,
                rhs: ir::Expr::Lit(solstice_ivm::Value::Int(0)),
            },
        ])),
        order_by: vec![ir::Order {
            col: "priority".to_string(),
            desc: true,
        }],
        limit: k,
        related: vec![ir::Related {
            rel_name: COMMENTS_REL.to_string(),
            sub: ir::Query {
                table: "comments".to_string(),
                order_by: vec![ir::Order {
                    col: "created_at".to_string(),
                    desc: true,
                }],
                limit: comments,
                ..ir::Query::default()
            },
            as_name: COMMENTS_REL.to_string(),
        }],
        allow_unbounded: false,
        params: vec![solstice_ivm::Value::Int(project)],
    }
}

/// `project_id = $0 AND closed = 0`, resolved to column ids.
pub fn issue_filter() -> Predicate {
    Predicate::and([
        Predicate::Cmp {
            lhs: Expr::Col(issue::PROJECT),
            op: CmpOp::Eq,
            rhs: Expr::Param(0),
        },
        Predicate::eq(issue::CLOSED, 0i64),
    ])
}

pub fn issue_order() -> Vec<(ColId, Dir)> {
    vec![(issue::PRIORITY, Dir::Desc)]
}

pub fn comment_order() -> Vec<(ColId, Dir)> {
    vec![(comment::CREATED_AT, Dir::Desc)]
}

/// The indexes [`query`] needs to be a seek rather than a walk.
///
/// Equality columns lead the sort columns, because both the source scan and
/// every per-parent refill pin some columns by equality before ordering by the
/// rest. See [`solstice_store::create_seek_index`] for why the order of the
/// index columns is the whole point.
pub fn index(store: &mut SqliteStore) -> Result<(), StoreError> {
    // `WHERE project_id = ? AND closed = 0 ORDER BY priority DESC`.
    store.index_seek(
        ISSUES,
        &[issue::PROJECT, issue::CLOSED],
        &[(issue::PRIORITY, Dir::Desc)],
    )?;
    // `WHERE issue_id = ? ORDER BY created_at DESC`, once per parent.
    store.index_seek(
        COMMENTS,
        &[comment::ISSUE],
        &[(comment::CREATED_AT, Dir::Desc)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;

    #[test]
    fn the_ir_compiles_to_the_predicate_written_by_hand() {
        // The two halves of this module, checked against each other. If the
        // compiler resolved `project_id` to the wrong column, both the view and
        // the benchmark would be wrong together and neither would say so.
        let c = compile(&catalog(), &query(7, 50, 3)).expect("the M0 query compiles");
        assert_eq!(c.filter, issue_filter());
        assert_eq!(c.order, issue_order());
        assert_eq!(c.children[0].order, comment_order());
        assert_eq!(c.params.get(0), &solstice_ivm::Value::Int(7));
    }

    #[test]
    fn the_catalog_declares_the_relation_the_query_traverses() {
        let cat = catalog();
        let rel = cat.rel(ISSUES, COMMENTS_REL).expect("comments is declared");
        assert_eq!(rel.child, COMMENTS);
        assert_eq!(rel.parent_col, issue::ID);
        assert_eq!(rel.child_col, comment::ISSUE);
    }

    #[test]
    fn every_column_id_matches_its_position_in_the_schema() {
        // These constants and the `Vec<Column>` above are two statements of one
        // fact. Nothing but this test stops a column inserted in the middle
        // from silently renumbering every call site.
        let s = schemas();
        let issues = &s[0];
        for (name, id) in [
            ("id", issue::ID),
            ("project_id", issue::PROJECT),
            ("priority", issue::PRIORITY),
            ("closed", issue::CLOSED),
            ("title", issue::TITLE),
            ("updated_at", issue::UPDATED_AT),
        ] {
            assert_eq!(issues.col(name), Some(id), "issues.{name}");
        }
        let comments = &s[1];
        for (name, id) in [
            ("id", comment::ID),
            ("issue_id", comment::ISSUE),
            ("created_at", comment::CREATED_AT),
            ("author", comment::AUTHOR),
            ("body", comment::BODY),
        ] {
            assert_eq!(comments.col(name), Some(id), "comments.{name}");
        }
    }
}

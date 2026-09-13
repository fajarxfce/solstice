//! What the engine knows about the application's tables.
//!
//! A [`Catalog`] is the thing that turns a name on the wire into an id in the
//! graph, and it is the only place that mapping exists. Plan §4.1 makes the FFI
//! ABI byte-based precisely so that it does not depend on the application's
//! schema; the price of that is that every query arriving from the host is
//! written in names, and something has to resolve them. This is that something,
//! and it runs once per subscribe rather than once per row.
//!
//! M0 builds its catalog by hand (see [`crate::m0`]). M1 generates it from the
//! schema DSL, and nothing above this module changes when it does.

use solstice_ivm::{ColId, Schema, TableId};

/// A declared 1:N relationship — the only kind of join DQL has (plan §1.1).
///
/// Named `Rel` rather than `Relation` because [`solstice_ivm::Relation`] is a
/// keyed set of rows. Two types called `Relation` in one engine is a diff
/// waiting to be misread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rel {
    /// What the query says: `related("comments")`.
    pub name: String,
    pub parent: TableId,
    pub child: TableId,
    /// Column of the parent that children point at. The primary key, in every
    /// case M0 has, but the operator does not require that.
    pub parent_col: ColId,
    /// Foreign-key column of the child.
    pub child_col: ColId,
}

/// Tables and relations, addressable by the names a query uses.
///
/// Lookups are linear scans. That is not an oversight: a catalog has tens of
/// entries, a lookup happens once per subscribe, and a `BTreeMap` here would
/// cost more in allocation than it ever saved in comparisons.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    tables: Vec<Schema>,
    rels: Vec<Rel>,
}

impl Catalog {
    pub fn new(tables: Vec<Schema>, rels: Vec<Rel>) -> Catalog {
        Catalog { tables, rels }
    }

    pub fn tables(&self) -> &[Schema] {
        &self.tables
    }

    pub fn rels(&self) -> &[Rel] {
        &self.rels
    }

    /// The schemas a store needs to create its tables.
    pub fn schemas(&self) -> Vec<Schema> {
        self.tables.clone()
    }

    /// By the name a query writes.
    pub fn table(&self, name: &str) -> Option<&Schema> {
        self.tables.iter().find(|s| s.name == name)
    }

    /// By the id the graph carries.
    pub fn by_id(&self, table: TableId) -> Option<&Schema> {
        self.tables.iter().find(|s| s.table == table)
    }

    /// A relation is addressed from its parent, because that is where a query
    /// traverses it from and because two tables may both have a `comments`.
    pub fn rel(&self, parent: TableId, name: &str) -> Option<&Rel> {
        self.rels
            .iter()
            .find(|r| r.parent == parent && r.name == name)
    }
}

//! Table schemas.
//!
//! Deliberately minimal for M0: the spike hardcodes an issues/comments schema
//! (plan §5.1) and the real schema DSL plus codegen lands in M1.

use crate::value::ColId;

pub type TableId = u16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueType {
    Int,
    Real,
    Text,
    Blob,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub ty: ValueType,
    pub nullable: bool,
}

impl Column {
    pub fn new(name: impl Into<String>, ty: ValueType) -> Self {
        Column {
            name: name.into(),
            ty,
            nullable: false,
        }
    }

    pub fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub table: TableId,
    pub name: String,
    pub columns: Vec<Column>,
    /// Index of the primary-key column. v1 is single-column (plan §1.1).
    pub pk: ColId,
}

impl Schema {
    pub fn new(table: TableId, name: impl Into<String>, columns: Vec<Column>, pk: ColId) -> Self {
        Schema {
            table,
            name: name.into(),
            columns,
            pk,
        }
    }

    pub fn col(&self, name: &str) -> Option<ColId> {
        self.columns
            .iter()
            .position(|c| c.name == name)
            .map(|i| i as ColId)
    }

    /// Column id by name, panicking on typos.
    ///
    /// For test fixtures and the hardcoded M0 schema only — the query builder
    /// resolves names at codegen time and never calls this.
    pub fn col_or_panic(&self, name: &str) -> ColId {
        self.col(name)
            .unwrap_or_else(|| panic!("no column {name:?} in table {:?}", self.name))
    }

    pub fn arity(&self) -> usize {
        self.columns.len()
    }
}

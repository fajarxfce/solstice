//! Predicates and their evaluation.
//!
//! # Three-valued logic is not optional here
//!
//! SQL predicates evaluate to true, false, or *unknown*, and `NULL` produces
//! unknown. Collapsing unknown to false early gives the right answer for a bare
//! `WHERE`, and the wrong answer the moment a `NOT` is involved: `NOT (x > 1)`
//! is unknown when `x` is NULL, not true.
//!
//! Getting this wrong would not show up as a crash. It would show up as rows
//! quietly appearing in a client's view that the server's own evaluation
//! excludes — a divergence between client and server that the whole design goes
//! out of its way to make impossible (plan §3.2). So [`Tri`] is threaded all the
//! way through, and only the very last step — [`Predicate::matches`] — collapses
//! it.
//!
//! The predicate set is exactly what plan §1.1 admits into v1: everything here
//! can be answered from a row plus bound parameters, with no scan and no state.

use crate::value::{ColId, Row, Value};
use std::cmp::Ordering;
use std::sync::Arc;

/// Bound query parameters, positionally indexed.
///
/// Parameters exist so that `now()` and `random()` can be rejected at compile
/// time (plan §1.1): a query that needs the current time takes `$now` as a
/// parameter, which makes its invalidation cost explicit and schedulable rather
/// than continuous.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params(Vec<Value>);

impl Params {
    pub fn new(values: Vec<Value>) -> Self {
        Params(values)
    }

    pub fn empty() -> Self {
        Params(Vec::new())
    }

    /// Out-of-range parameters read as NULL, mirroring [`Row::get`]: query
    /// validation rules them out, and an operator is the wrong place to panic.
    pub fn get(&self, idx: u16) -> &Value {
        self.0.get(idx as usize).unwrap_or(&Value::Null)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A scalar expression. Deliberately not recursive: arbitrary expressions in
/// `orderBy` and predicates are excluded from v1 (plan §1.1), and computed
/// values belong in generated columns where the store can index them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Col(ColId),
    Lit(Value),
    Param(u16),
}

impl Expr {
    pub fn eval<'a>(&'a self, row: &'a Row, params: &'a Params) -> &'a Value {
        match self {
            Expr::Col(c) => row.get(*c),
            Expr::Lit(v) => v,
            Expr::Param(i) => params.get(*i),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn admits(self, ord: Ordering) -> bool {
        match self {
            CmpOp::Eq => ord == Ordering::Equal,
            CmpOp::Ne => ord != Ordering::Equal,
            CmpOp::Lt => ord == Ordering::Less,
            CmpOp::Le => ord != Ordering::Greater,
            CmpOp::Gt => ord == Ordering::Greater,
            CmpOp::Ge => ord != Ordering::Less,
        }
    }
}

/// SQL's three-valued logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    pub fn is_true(self) -> bool {
        self == Tri::True
    }

    fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }

    fn from_bool(b: bool) -> Tri {
        if b {
            Tri::True
        } else {
            Tri::False
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
    /// Matches everything. The identity of `And`, and what an absent `where`
    /// compiles to.
    True,
    Cmp {
        lhs: Expr,
        op: CmpOp,
        rhs: Expr,
    },
    In {
        lhs: Expr,
        list: Vec<Value>,
    },
    Between {
        lhs: Expr,
        low: Expr,
        high: Expr,
    },
    IsNull(Expr),
    IsNotNull(Expr),
    /// `LIKE 'prefix%'` only. Anchored prefixes are the one `LIKE` form an
    /// index can answer in O(log n), which is the rule that decides what DQL
    /// admits (plan §1.1). Infix and suffix matching belong to full-text
    /// search, which is excluded from the IVM graph.
    LikePrefix {
        lhs: Expr,
        prefix: Arc<str>,
    },
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Not(Box<Predicate>),
}

impl Predicate {
    pub fn and(preds: impl IntoIterator<Item = Predicate>) -> Predicate {
        Predicate::And(preds.into_iter().collect())
    }

    pub fn or(preds: impl IntoIterator<Item = Predicate>) -> Predicate {
        Predicate::Or(preds.into_iter().collect())
    }

    pub fn negate(pred: Predicate) -> Predicate {
        Predicate::Not(Box::new(pred))
    }

    pub fn eq(col: ColId, v: impl Into<Value>) -> Predicate {
        Predicate::Cmp {
            lhs: Expr::Col(col),
            op: CmpOp::Eq,
            rhs: Expr::Lit(v.into()),
        }
    }

    /// Evaluate under SQL's three-valued logic.
    pub fn eval(&self, row: &Row, params: &Params) -> Tri {
        match self {
            Predicate::True => Tri::True,

            Predicate::Cmp { lhs, op, rhs } => {
                match lhs.eval(row, params).sql_cmp(rhs.eval(row, params)) {
                    Some(ord) => Tri::from_bool(op.admits(ord)),
                    None => Tri::Unknown,
                }
            }

            Predicate::In { lhs, list } => {
                let v = lhs.eval(row, params);
                if v.is_null() {
                    return Tri::Unknown;
                }
                // `x IN (1, NULL)` is unknown rather than false when `x` is not
                // 1: the NULL might have been the match. A found match still
                // short-circuits to true, which is why the NULL check cannot
                // simply happen up front.
                let mut saw_null = false;
                for candidate in list {
                    if candidate.is_null() {
                        saw_null = true;
                    } else if v.sql_cmp(candidate) == Some(Ordering::Equal) {
                        return Tri::True;
                    }
                }
                if saw_null {
                    Tri::Unknown
                } else {
                    Tri::False
                }
            }

            Predicate::Between { lhs, low, high } => {
                let v = lhs.eval(row, params);
                let lo = match v.sql_cmp(low.eval(row, params)) {
                    Some(o) => Tri::from_bool(o != Ordering::Less),
                    None => Tri::Unknown,
                };
                let hi = match v.sql_cmp(high.eval(row, params)) {
                    Some(o) => Tri::from_bool(o != Ordering::Greater),
                    None => Tri::Unknown,
                };
                and2(lo, hi)
            }

            // IS NULL / IS NOT NULL are the two predicates that are *never*
            // unknown — that is the whole point of them.
            Predicate::IsNull(e) => Tri::from_bool(e.eval(row, params).is_null()),
            Predicate::IsNotNull(e) => Tri::from_bool(!e.eval(row, params).is_null()),

            Predicate::LikePrefix { lhs, prefix } => match lhs.eval(row, params) {
                Value::Null => Tri::Unknown,
                Value::Text(s) => Tri::from_bool(s.starts_with(&**prefix)),
                // SQLite would coerce a number to text here. DQL does not,
                // because the query builder only offers `like` on TEXT columns,
                // so a non-text operand is unreachable from a valid query.
                _ => Tri::False,
            },

            Predicate::And(preds) => {
                let mut acc = Tri::True;
                for p in preds {
                    acc = and2(acc, p.eval(row, params));
                    if acc == Tri::False {
                        return Tri::False;
                    }
                }
                acc
            }

            Predicate::Or(preds) => {
                let mut acc = Tri::False;
                for p in preds {
                    acc = or2(acc, p.eval(row, params));
                    if acc == Tri::True {
                        return Tri::True;
                    }
                }
                acc
            }

            Predicate::Not(p) => p.eval(row, params).not(),
        }
    }

    /// Does this row belong in the result? Unknown counts as no — this is the
    /// single point where three-valued logic collapses to two.
    pub fn matches(&self, row: &Row, params: &Params) -> bool {
        self.eval(row, params).is_true()
    }
}

/// `AND` is false-dominant: one false makes the whole thing false even if
/// another conjunct is unknown.
fn and2(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::False, _) | (_, Tri::False) => Tri::False,
        (Tri::Unknown, _) | (_, Tri::Unknown) => Tri::Unknown,
        _ => Tri::True,
    }
}

/// `OR` is true-dominant, the mirror image of [`and2`].
fn or2(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::True, _) | (_, Tri::True) => Tri::True,
        (Tri::Unknown, _) | (_, Tri::Unknown) => Tri::Unknown,
        _ => Tri::False,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(vals: Vec<Value>) -> Row {
        Row::new(vals)
    }

    #[test]
    fn empty_and_is_true_empty_or_is_false() {
        let r = row(vec![Value::Int(1)]);
        let p = Params::empty();
        assert_eq!(Predicate::And(vec![]).eval(&r, &p), Tri::True);
        assert_eq!(Predicate::Or(vec![]).eval(&r, &p), Tri::False);
    }

    #[test]
    fn not_of_unknown_stays_unknown() {
        // The reason `Tri` exists. A two-valued evaluator returns true here and
        // leaks a NULL row into the view.
        let r = row(vec![Value::Null]);
        let p = Params::empty();
        let gt = Predicate::Cmp {
            lhs: Expr::Col(0),
            op: CmpOp::Gt,
            rhs: Expr::Lit(Value::Int(1)),
        };
        assert_eq!(gt.eval(&r, &p), Tri::Unknown);
        assert_eq!(Predicate::negate(gt.clone()).eval(&r, &p), Tri::Unknown);
        assert!(!Predicate::negate(gt).matches(&r, &p));
    }

    #[test]
    fn and_is_false_dominant_over_unknown() {
        let r = row(vec![Value::Null, Value::Int(5)]);
        let p = Params::empty();
        let unknown = Predicate::Cmp {
            lhs: Expr::Col(0),
            op: CmpOp::Eq,
            rhs: Expr::Lit(Value::Int(1)),
        };
        let false_ = Predicate::eq(1, 99i64);
        assert_eq!(
            Predicate::and([unknown.clone(), false_]).eval(&r, &p),
            Tri::False
        );
        let true_ = Predicate::eq(1, 5i64);
        assert_eq!(Predicate::and([unknown, true_]).eval(&r, &p), Tri::Unknown);
    }

    #[test]
    fn or_is_true_dominant_over_unknown() {
        let r = row(vec![Value::Null, Value::Int(5)]);
        let p = Params::empty();
        let unknown = Predicate::eq(0, 1i64);
        assert_eq!(
            Predicate::or([unknown.clone(), Predicate::eq(1, 5i64)]).eval(&r, &p),
            Tri::True
        );
        assert_eq!(
            Predicate::or([unknown, Predicate::eq(1, 99i64)]).eval(&r, &p),
            Tri::Unknown
        );
    }

    #[test]
    fn in_with_a_null_in_the_list_is_unknown_when_no_match() {
        let r = row(vec![Value::Int(7)]);
        let p = Params::empty();
        let pred = |list: Vec<Value>| Predicate::In {
            lhs: Expr::Col(0),
            list,
        };
        assert_eq!(
            pred(vec![Value::Int(1), Value::Null]).eval(&r, &p),
            Tri::Unknown
        );
        // A match still wins outright.
        assert_eq!(
            pred(vec![Value::Int(7), Value::Null]).eval(&r, &p),
            Tri::True
        );
        assert_eq!(pred(vec![Value::Int(1)]).eval(&r, &p), Tri::False);
    }

    #[test]
    fn is_null_is_never_unknown() {
        let r = row(vec![Value::Null]);
        let p = Params::empty();
        assert_eq!(Predicate::IsNull(Expr::Col(0)).eval(&r, &p), Tri::True);
        assert_eq!(Predicate::IsNotNull(Expr::Col(0)).eval(&r, &p), Tri::False);
    }

    #[test]
    fn comparisons_coerce_numerics_like_sqlite() {
        let r = row(vec![Value::Int(1)]);
        let p = Params::empty();
        assert!(Predicate::Cmp {
            lhs: Expr::Col(0),
            op: CmpOp::Eq,
            rhs: Expr::Lit(Value::Real(1.0)),
        }
        .matches(&r, &p));
    }

    #[test]
    fn between_is_inclusive_and_null_aware() {
        let p = Params::empty();
        let pred = Predicate::Between {
            lhs: Expr::Col(0),
            low: Expr::Lit(Value::Int(1)),
            high: Expr::Lit(Value::Int(3)),
        };
        assert_eq!(pred.eval(&row(vec![Value::Int(1)]), &p), Tri::True);
        assert_eq!(pred.eval(&row(vec![Value::Int(3)]), &p), Tri::True);
        assert_eq!(pred.eval(&row(vec![Value::Int(4)]), &p), Tri::False);
        assert_eq!(pred.eval(&row(vec![Value::Null]), &p), Tri::Unknown);
    }

    #[test]
    fn like_prefix_matches_anchored_only() {
        let p = Params::empty();
        let pred = Predicate::LikePrefix {
            lhs: Expr::Col(0),
            prefix: Arc::from("ab"),
        };
        assert!(pred.matches(&row(vec![Value::text("abc")]), &p));
        assert!(!pred.matches(&row(vec![Value::text("zab")]), &p));
        assert_eq!(pred.eval(&row(vec![Value::Null]), &p), Tri::Unknown);
    }

    #[test]
    fn params_resolve_positionally() {
        let r = row(vec![Value::Int(42)]);
        let p = Params::new(vec![Value::Int(42)]);
        assert!(Predicate::Cmp {
            lhs: Expr::Col(0),
            op: CmpOp::Eq,
            rhs: Expr::Param(0),
        }
        .matches(&r, &p));
        // A missing parameter reads as NULL, so the predicate is unknown rather
        // than panicking inside an operator.
        assert_eq!(
            Predicate::Cmp {
                lhs: Expr::Col(0),
                op: CmpOp::Eq,
                rhs: Expr::Param(9),
            }
            .eval(&r, &p),
            Tri::Unknown
        );
    }
}

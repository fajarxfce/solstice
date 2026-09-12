//! Scalar values, rows, and row keys.
//!
//! # Two comparison orders, on purpose
//!
//! `Value` has *two* distinct notions of comparison, and conflating them is the
//! single easiest way to make this engine silently unsound:
//!
//! * [`Ord`] / [`Eq`] / [`Hash`] — a **structural** total order. `Int(1)` and
//!   `Real(1.0)` are different values. This is what indexes, hash maps, sort
//!   windows and `TopK` boundaries use. It is a true total order and `Eq` is
//!   consistent with both `Ord` and `Hash`, which the standard library requires
//!   of anything used as a `BTreeMap`/`HashMap` key.
//!
//! * [`Value::sql_cmp`] — SQLite's comparison semantics, where `Int(1)` and
//!   `Real(1.0)` *are* equal and `NULL` compares to nothing. This is used only
//!   when evaluating predicates, so that our results match the SQLite oracle in
//!   the property tests.
//!
//! If `Ord` used SQL semantics, `Int(1) == Real(1.0)` would hold while their
//! hashes differed, violating the `Eq`/`Hash` contract and corrupting every
//! index in the engine. So the numeric coercion lives in the predicate
//! evaluator instead, where it belongs.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// A scalar value. The first five variants mirror SQLite's storage classes —
/// deliberately, since SQLite is the durable store and we want a lossless round
/// trip. [`Value::Rows`] is the one that does not, and it says so.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(Arc<str>),
    Blob(Arc<[u8]>),
    /// An ordered collection of child rows — the result of traversing a
    /// declared relationship.
    ///
    /// # This one can never be stored
    ///
    /// It exists only *downstream of a `Join`*. Nothing writes it to SQLite,
    /// nothing reads it from SQLite, and no predicate can be written against it
    /// in DQL. Adding a variant to a type that otherwise mirrors storage classes
    /// is a real cost, paid for one reason: DQL results are **hierarchical, not
    /// flat** (plan §1.1), and the delta a join emits when a child changes is an
    /// *update to the parent row* (plan §1.3). For that to be expressible, the
    /// children have to live in a column of the parent.
    ///
    /// # What hierarchy buys, beyond the API
    ///
    /// A flat join multiplies rows: one parent with three comments becomes three
    /// result rows. A `TopK` above it would then be limiting *comment* rows, so
    /// it could not be pushed below the join — and a `TopK` above a join is
    /// exactly the configuration plan §7 names as the project's most likely
    /// technical failure, because refilling the window means asking the join for
    /// children of parents it is not holding.
    ///
    /// Hierarchical output leaves parent cardinality unchanged, so `TopK`
    /// commutes with the join and can run *below* it. The join then only ever
    /// holds children for the `k + slack` parents in the window. The memory
    /// bound is a consequence of this variant existing.
    Rows(Arc<[Row]>),
}

impl Value {
    pub fn text(s: impl AsRef<str>) -> Self {
        Value::Text(Arc::from(s.as_ref()))
    }

    pub fn blob(b: impl AsRef<[u8]>) -> Self {
        Value::Blob(Arc::from(b.as_ref()))
    }

    pub fn rows(rows: impl Into<Arc<[Row]>>) -> Self {
        Value::Rows(rows.into())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Bytes this value owns beyond its own discriminant.
    ///
    /// Approximate on purpose: it ignores allocator overhead and counts a shared
    /// `Arc` payload once per holder. Operator memory budgets are enforced at
    /// megabyte granularity (plan §7), so an estimate that is cheap enough to
    /// call on every state query beats an exact figure that nobody calls.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Value::Null | Value::Int(_) | Value::Real(_) => 0,
            Value::Text(s) => s.len(),
            Value::Blob(b) => b.len(),
            Value::Rows(rows) => {
                rows.len() * std::mem::size_of::<Row>()
                    + rows.iter().map(Row::heap_bytes).sum::<usize>()
            }
        }
    }

    /// SQLite storage-class rank, used for the structural order and for
    /// cross-class SQL comparison. NULL < numeric < text < blob.
    fn class_rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Int(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
            // Unreachable from `sql_cmp`, which rejects `Rows` before getting
            // here. Ranked anyway so the function stays total.
            Value::Rows(_) => 4,
        }
    }

    /// Discriminant for the *structural* order, which — unlike `class_rank` —
    /// keeps `Int` and `Real` apart so that `Eq` and `Hash` stay consistent.
    fn struct_rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Int(_) => 1,
            Value::Real(_) => 2,
            Value::Text(_) => 3,
            Value::Blob(_) => 4,
            Value::Rows(_) => 5,
        }
    }

    /// Compare using SQLite's rules: numerics coerce across `Int`/`Real`, and
    /// `NULL` is incomparable (hence the `Option`).
    ///
    /// Returns `None` if either side is `NULL`, which callers must propagate as
    /// SQL "unknown" rather than collapsing to false too early — `NOT (x > 1)`
    /// is unknown, not true, when `x` is NULL.
    ///
    /// It also returns `None` for [`Value::Rows`]: a child collection is not a
    /// SQL value, so comparing one is unknown rather than false. DQL rejects
    /// such a predicate at compile time; this is the runtime backstop, and
    /// unknown is the safe side to fail to — a row whose comparison is unknown
    /// drops out of the view instead of ordering by some invented rule.
    pub fn sql_cmp(&self, other: &Value) -> Option<Ordering> {
        if self.is_null() || other.is_null() {
            return None;
        }
        if matches!(self, Value::Rows(_)) || matches!(other, Value::Rows(_)) {
            return None;
        }
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Real(a), Value::Real(b)) => Some(total_cmp_f64(*a, *b)),
            (Value::Int(a), Value::Real(b)) => Some(cmp_i64_f64(*a, *b)),
            (Value::Real(a), Value::Int(b)) => Some(cmp_i64_f64(*b, *a).reverse()),
            (Value::Text(a), Value::Text(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
            (Value::Blob(a), Value::Blob(b)) => Some(a[..].cmp(&b[..])),
            // Different storage classes never compare equal; they order by rank.
            _ => Some(self.class_rank().cmp(&other.class_rank())),
        }
    }
}

/// Exact `i64` vs `f64` comparison.
///
/// The obvious `(a as f64).partial_cmp(&b)` is wrong for magnitudes above
/// 2^53, where the cast rounds and two distinct integers collapse onto the same
/// float. Row ids and timestamps live in exactly that range, so this is worth
/// getting right rather than discovering it as a mis-sorted list later.
fn cmp_i64_f64(a: i64, b: f64) -> Ordering {
    if b.is_nan() {
        // Mirror `total_cmp`: NaN sorts above every real number.
        return Ordering::Less;
    }
    if b == f64::INFINITY {
        return Ordering::Less;
    }
    if b == f64::NEG_INFINITY {
        return Ordering::Greater;
    }
    // `b.floor()` is exactly representable and within i64 range after clamping,
    // so comparing against it is exact.
    if b >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    if b < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    let bf = b.floor();
    let bi = bf as i64;
    match a.cmp(&bi) {
        Ordering::Equal => {
            // Same integer part: `b` is larger iff it has a fractional part.
            if b > bf {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }
        ord => ord,
    }
}

/// Total order over `f64` including NaN, matching `f64::total_cmp`: it is a
/// true total order, and it agrees with bitwise equality, which is what lets us
/// hash by bits below.
fn total_cmp_f64(a: f64, b: f64) -> Ordering {
    a.total_cmp(&b)
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    /// Structural total order. See the module docs for why this is not SQL
    /// comparison.
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Real(a), Value::Real(b)) => total_cmp_f64(*a, *b),
            (Value::Text(a), Value::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Value::Blob(a), Value::Blob(b)) => a[..].cmp(&b[..]),
            // Child collections compare element-wise, which is what makes a
            // parent row whose children changed compare unequal to its old
            // image — and therefore what makes the join emit an update.
            (Value::Rows(a), Value::Rows(b)) => a[..].cmp(&b[..]),
            _ => self.struct_rank().cmp(&other.struct_rank()),
        }
    }
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.struct_rank().hash(state);
        match self {
            Value::Null => {}
            Value::Int(v) => v.hash(state),
            // Hash by bits so that hashing agrees with `total_cmp` equality.
            Value::Real(v) => v.to_bits().hash(state),
            Value::Text(v) => v.as_bytes().hash(state),
            Value::Blob(v) => v[..].hash(state),
            Value::Rows(v) => v[..].hash(state),
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Real(v)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Int(v as i64)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::text(v)
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::text(v)
    }
}

/// Column index within a row, positional against the table's [`Schema`].
pub type ColId = u16;

/// A row: a positional tuple of values interpreted against a schema.
///
/// Cheap to clone (`Arc` over the value slice), which matters because deltas
/// carry both the before and after image of every changed row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Row {
    values: Arc<[Value]>,
}

impl Row {
    pub fn new(values: impl Into<Arc<[Value]>>) -> Self {
        Row {
            values: values.into(),
        }
    }

    pub fn get(&self, col: ColId) -> &Value {
        // Out-of-range column ids are a compile-time impossibility once queries
        // are validated against the schema, so treat it as NULL rather than
        // panicking deep inside an operator.
        self.values.get(col as usize).unwrap_or(&Value::Null)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Bytes this row owns: the value slice plus anything the values point at.
    pub fn heap_bytes(&self) -> usize {
        self.values.len() * std::mem::size_of::<Value>()
            + self.values.iter().map(Value::heap_bytes).sum::<usize>()
    }

    /// The same row with one more column on the end.
    ///
    /// How a `Join` attaches its children: the child collection becomes the
    /// parent's last column, at a position the planner knows statically. A join
    /// therefore only ever *widens* a row, never reorders it, so column ids
    /// assigned upstream stay valid downstream.
    pub fn with_appended(&self, value: Value) -> Row {
        let mut values = Vec::with_capacity(self.values.len() + 1);
        values.extend(self.values.iter().cloned());
        values.push(value);
        Row::new(values)
    }

    /// Project onto a subset of columns, preserving the given order.
    pub fn project(&self, cols: &[ColId]) -> Row {
        Row::new(
            cols.iter()
                .map(|c| self.get(*c).clone())
                .collect::<Vec<_>>(),
        )
    }
}

impl FromIterator<Value> for Row {
    fn from_iter<T: IntoIterator<Item = Value>>(iter: T) -> Self {
        Row::new(iter.into_iter().collect::<Vec<_>>())
    }
}

/// Primary key of a row.
///
/// A newtype rather than a bare `Value` so that composite keys can be added
/// without touching every call site. v1 is single-column (plan §1.1).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowKey(Value);

impl RowKey {
    pub fn new(v: Value) -> Self {
        RowKey(v)
    }

    pub fn value(&self) -> &Value {
        &self.0
    }

    pub fn heap_bytes(&self) -> usize {
        self.0.heap_bytes()
    }
}

impl From<i64> for RowKey {
    fn from(v: i64) -> Self {
        RowKey(Value::Int(v))
    }
}

impl From<&str> for RowKey {
    fn from(v: &str) -> Self {
        RowKey(Value::text(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_order_separates_int_and_real() {
        // The contract that keeps every index in the engine sound.
        assert_ne!(Value::Int(1), Value::Real(1.0));
        assert!(Value::Int(1) < Value::Real(1.0));
    }

    #[test]
    fn sql_cmp_coerces_numerics() {
        assert_eq!(
            Value::Int(1).sql_cmp(&Value::Real(1.0)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            Value::Int(2).sql_cmp(&Value::Real(1.5)),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn sql_cmp_is_unknown_for_null() {
        assert_eq!(Value::Null.sql_cmp(&Value::Int(1)), None);
        assert_eq!(Value::Int(1).sql_cmp(&Value::Null), None);
        assert_eq!(Value::Null.sql_cmp(&Value::Null), None);
    }

    #[test]
    fn large_integers_compare_exactly_against_floats() {
        // Both of these cast to the same f64; a naive implementation reports
        // them equal to it and mis-sorts rows keyed by snowflake-style ids.
        let a = 9_007_199_254_740_993i64; // 2^53 + 1
        let b = 9_007_199_254_740_992.0f64; // 2^53
        assert_eq!(
            Value::Int(a).sql_cmp(&Value::Real(b)),
            Some(Ordering::Greater)
        );
        assert_eq!(Value::Real(b).sql_cmp(&Value::Int(a)), Some(Ordering::Less));
    }

    #[test]
    fn fractional_floats_order_against_equal_integer_part() {
        assert_eq!(
            Value::Int(3).sql_cmp(&Value::Real(3.5)),
            Some(Ordering::Less)
        );
        assert_eq!(
            Value::Int(3).sql_cmp(&Value::Real(2.5)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Value::Int(-3).sql_cmp(&Value::Real(-2.5)),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn eq_hash_and_ord_agree() {
        use std::collections::hash_map::DefaultHasher;
        fn h(v: &Value) -> u64 {
            let mut s = DefaultHasher::new();
            v.hash(&mut s);
            s.finish()
        }
        let cases = [
            Value::Null,
            Value::Int(0),
            Value::Real(0.0),
            Value::Real(-0.0),
            Value::Real(f64::NAN),
            Value::text("a"),
            Value::blob([1u8, 2]),
        ];
        for a in &cases {
            for b in &cases {
                // `Eq` must imply equal hashes, and must agree with `Ord`.
                assert_eq!(a == b, a.cmp(b) == Ordering::Equal);
                if a == b {
                    assert_eq!(h(a), h(b));
                }
            }
        }
        // NaN is equal to itself under a total order, unlike `f64`'s own `PartialEq`.
        assert_eq!(Value::Real(f64::NAN), Value::Real(f64::NAN));
        // -0.0 and 0.0 are distinct under a total order, so their hashes may differ.
        assert_ne!(Value::Real(-0.0), Value::Real(0.0));
    }

    #[test]
    fn row_projection_reorders_columns() {
        let r = Row::new(vec![Value::Int(1), Value::text("b"), Value::Int(3)]);
        let p = r.project(&[2, 0]);
        assert_eq!(p.values(), &[Value::Int(3), Value::Int(1)]);
    }

    #[test]
    fn row_get_out_of_range_is_null() {
        let r = Row::new(vec![Value::Int(1)]);
        assert!(r.get(7).is_null());
    }

    #[test]
    fn a_child_collection_is_not_sql_comparable() {
        // Unknown, not false and not "ranks above blob". A DQL query can never
        // ask this, so the only job here is to fail to the safe side.
        let children = Value::rows(vec![Row::new(vec![Value::Int(1)])]);
        assert_eq!(children.sql_cmp(&Value::Int(1)), None);
        assert_eq!(Value::Int(1).sql_cmp(&children), None);
        assert_eq!(children.sql_cmp(&children), None);
        assert!(!children.is_null());
    }

    /// Structural equality still has to work, or a join could never tell that a
    /// parent's children changed.
    #[test]
    fn child_collections_compare_element_wise() {
        let one = Value::rows(vec![Row::new(vec![Value::Int(1)])]);
        let same = Value::rows(vec![Row::new(vec![Value::Int(1)])]);
        let other = Value::rows(vec![Row::new(vec![Value::Int(2)])]);
        let longer = Value::rows(vec![
            Row::new(vec![Value::Int(1)]),
            Row::new(vec![Value::Int(9)]),
        ]);

        assert_eq!(one, same);
        assert_ne!(one, other);
        assert_ne!(one, longer);
        assert!(one < longer, "a prefix orders before its extension");
        assert_eq!(Value::rows(Vec::new()), Value::rows(Vec::new()));
    }

    #[test]
    fn appending_widens_a_row_without_disturbing_it() {
        let r = Row::new(vec![Value::Int(1), Value::text("a")]);
        let wide = r.with_appended(Value::rows(Vec::new()));
        assert_eq!(wide.get(0), &Value::Int(1));
        assert_eq!(wide.get(1), &Value::text("a"));
        assert_eq!(wide.len(), 3);
    }
}

//! The query IR, encoded and decoded against
//! [`query.proto`](../../../proto/solstice/v1/query.proto).
//!
//! # Why these types are not `solstice_ivm`'s
//!
//! `solstice-ivm` has a [`Predicate`](solstice_ivm::Predicate) and an
//! [`Expr`](solstice_ivm::Expr) already, and they look almost like the ones
//! here. The difference is the one the `.proto` is built around: **these address
//! columns by name, the engine's address them by id.**
//!
//! Collapsing the two would mean either putting string compares on the filter's
//! hot path, or putting process-local ids on the wire. Keeping them apart makes
//! the resolution a single explicit step at subscribe — `solstice-core`'s
//! `compile` module — where an unknown column is an error at the call that
//! caused it rather than a row that quietly fails to match.
//!
//! # Which side is the oracle here
//!
//! [`view`](crate::view) is checked by `protoc`-generated *decoders*: Rust
//! writes, Dart and Kotlin read. This module is checked the other way round —
//! the host builds a query with generated code and this decodes it. Between
//! them, both halves of the hand-written wire implementation face an
//! independent implementation, which a round trip against itself could never
//! establish.

use solstice_ivm::Value;

use crate::view::{put_value, read_value};
use crate::wire::{wire_type, Decoder, Encoder, WireError};

/// A subscription request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub table: String,
    /// `Query.where` on the wire. `where` is a Rust keyword, and the same rule
    /// that renamed `int` to `integer` in `view.proto` applies in reverse here:
    /// the `.proto` is normative and the transcription dodges.
    pub filter: Option<Predicate>,
    pub order_by: Vec<Order>,
    /// Zero means unbounded, which is only legal with [`Query::allow_unbounded`].
    pub limit: u32,
    pub related: Vec<Related>,
    pub allow_unbounded: bool,
    pub params: Vec<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Related {
    pub rel_name: String,
    pub sub: Query,
    /// The parent column the children attach to. Empty means [`Related::rel_name`].
    pub as_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Order {
    pub col: String,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Col(String),
    Lit(Value),
    Param(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn tag(self) -> u64 {
        match self {
            CmpOp::Eq => 1,
            CmpOp::Ne => 2,
            CmpOp::Lt => 3,
            CmpOp::Le => 4,
            CmpOp::Gt => 5,
            CmpOp::Ge => 6,
        }
    }

    fn from_tag(v: u64) -> Option<CmpOp> {
        Some(match v {
            1 => CmpOp::Eq,
            2 => CmpOp::Ne,
            3 => CmpOp::Lt,
            4 => CmpOp::Le,
            5 => CmpOp::Gt,
            6 => CmpOp::Ge,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
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
    /// The prefix without its trailing `%`.
    LikePrefix {
        lhs: Expr,
        prefix: String,
    },
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Not(Box<Predicate>),
}

impl Query {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        put_query(&mut e, self);
        e.finish()
    }

    pub fn decode(buf: &[u8]) -> Result<Query, WireError> {
        read_query(&mut Decoder::new(buf))
    }
}

fn put_query(e: &mut Encoder, q: &Query) {
    if !q.table.is_empty() {
        e.bytes_field(1, q.table.as_bytes());
    }
    if let Some(p) = &q.filter {
        e.message(2, |e| put_pred(e, p));
    }
    for o in &q.order_by {
        e.message(3, |e| {
            if !o.col.is_empty() {
                e.bytes_field(1, o.col.as_bytes());
            }
            if o.desc {
                e.tag(2, wire_type::VARINT);
                e.varint(1);
            }
        });
    }
    e.uint32_field(4, q.limit);
    for r in &q.related {
        e.message(6, |e| {
            if !r.rel_name.is_empty() {
                e.bytes_field(1, r.rel_name.as_bytes());
            }
            e.message(2, |e| put_query(e, &r.sub));
            if !r.as_name.is_empty() {
                e.bytes_field(3, r.as_name.as_bytes());
            }
        });
    }
    if q.allow_unbounded {
        e.tag(9, wire_type::VARINT);
        e.varint(1);
    }
    for v in &q.params {
        put_value(e, 10, v);
    }
}

fn put_pred(e: &mut Encoder, p: &Predicate) {
    match p {
        // An empty message, exactly like an absent `where`: the two mean the
        // same thing and are worth nothing to distinguish.
        Predicate::True => e.message(1, |_| {}),
        Predicate::Cmp { lhs, op, rhs } => e.message(2, |e| {
            e.message(1, |e| put_expr(e, lhs));
            e.tag(2, wire_type::VARINT);
            e.varint(op.tag());
            e.message(3, |e| put_expr(e, rhs));
        }),
        Predicate::In { lhs, list } => e.message(3, |e| {
            e.message(1, |e| put_expr(e, lhs));
            for v in list {
                put_value(e, 2, v);
            }
        }),
        Predicate::Between { lhs, low, high } => e.message(4, |e| {
            e.message(1, |e| put_expr(e, lhs));
            e.message(2, |e| put_expr(e, low));
            e.message(3, |e| put_expr(e, high));
        }),
        Predicate::IsNull(x) => e.message(5, |e| put_expr(e, x)),
        Predicate::IsNotNull(x) => e.message(6, |e| put_expr(e, x)),
        Predicate::LikePrefix { lhs, prefix } => e.message(7, |e| {
            e.message(1, |e| put_expr(e, lhs));
            if !prefix.is_empty() {
                e.bytes_field(2, prefix.as_bytes());
            }
        }),
        Predicate::And(list) => e.message(8, |e| put_pred_list(e, list)),
        Predicate::Or(list) => e.message(9, |e| put_pred_list(e, list)),
        Predicate::Not(inner) => e.message(10, |e| put_pred(e, inner)),
    }
}

fn put_pred_list(e: &mut Encoder, list: &[Predicate]) {
    for p in list {
        e.message(1, |e| put_pred(e, p));
    }
}

fn put_expr(e: &mut Encoder, x: &Expr) {
    match x {
        Expr::Col(name) => e.bytes_field(1, name.as_bytes()),
        Expr::Lit(v) => put_value(e, 2, v),
        Expr::Param(i) => {
            // Written even when zero. `$0` is the commonest parameter there is,
            // and in a oneof the tag is the information: omitting it would
            // decode back as an unset expression.
            e.tag(3, wire_type::VARINT);
            e.varint(*i as u64);
        }
    }
}

fn read_query(d: &mut Decoder<'_>) -> Result<Query, WireError> {
    let mut q = Query::default();
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::LEN) => q.table = d.string()?.to_string(),
            (2, wire_type::LEN) => q.filter = Some(d.message(read_pred)?),
            (3, wire_type::LEN) => q.order_by.push(d.message(read_order)?),
            (4, wire_type::VARINT) => q.limit = d.varint()? as u32,
            (6, wire_type::LEN) => q.related.push(d.message(read_related)?),
            (9, wire_type::VARINT) => q.allow_unbounded = d.varint()? != 0,
            (10, wire_type::LEN) => q.params.push(d.message(read_value)?),
            _ => d.skip(t.wire)?,
        }
    }
    Ok(q)
}

fn read_order(d: &mut Decoder<'_>) -> Result<Order, WireError> {
    let mut o = Order::default();
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::LEN) => o.col = d.string()?.to_string(),
            (2, wire_type::VARINT) => o.desc = d.varint()? != 0,
            _ => d.skip(t.wire)?,
        }
    }
    Ok(o)
}

fn read_related(d: &mut Decoder<'_>) -> Result<Related, WireError> {
    let mut r = Related::default();
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::LEN) => r.rel_name = d.string()?.to_string(),
            (2, wire_type::LEN) => r.sub = d.message(read_query)?,
            (3, wire_type::LEN) => r.as_name = d.string()?.to_string(),
            _ => d.skip(t.wire)?,
        }
    }
    Ok(r)
}

fn read_pred(d: &mut Decoder<'_>) -> Result<Predicate, WireError> {
    // An unset oneof is `True`, for the same reason an absent `where` is: a
    // predicate that declines to say anything constrains nothing.
    let mut p = Predicate::True;
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::LEN) => {
                d.message(|_| Ok(()))?;
                p = Predicate::True;
            }
            (2, wire_type::LEN) => {
                p = d.message(|d| {
                    let mut lhs = Expr::Param(0);
                    let mut rhs = Expr::Param(0);
                    let mut op = None;
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::LEN) => lhs = d.message(read_expr)?,
                            (2, wire_type::VARINT) => {
                                let v = d.varint()?;
                                // Not skippable. A comparison this version does
                                // not know cannot be defaulted to `Eq` — that
                                // would return the wrong rows rather than fail,
                                // and a silently wrong query view is the worst
                                // outcome this codebase has.
                                op = Some(
                                    CmpOp::from_tag(v)
                                        .ok_or(WireError::UnknownEnum { field: 2, value: v })?,
                                );
                            }
                            (3, wire_type::LEN) => rhs = d.message(read_expr)?,
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(Predicate::Cmp {
                        lhs,
                        op: op.ok_or(WireError::UnknownEnum { field: 2, value: 0 })?,
                        rhs,
                    })
                })?
            }
            (3, wire_type::LEN) => {
                p = d.message(|d| {
                    let mut lhs = Expr::Param(0);
                    let mut list = Vec::new();
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::LEN) => lhs = d.message(read_expr)?,
                            (2, wire_type::LEN) => list.push(d.message(read_value)?),
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(Predicate::In { lhs, list })
                })?
            }
            (4, wire_type::LEN) => {
                p = d.message(|d| {
                    let mut lhs = Expr::Param(0);
                    let mut low = Expr::Param(0);
                    let mut high = Expr::Param(0);
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::LEN) => lhs = d.message(read_expr)?,
                            (2, wire_type::LEN) => low = d.message(read_expr)?,
                            (3, wire_type::LEN) => high = d.message(read_expr)?,
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(Predicate::Between { lhs, low, high })
                })?
            }
            (5, wire_type::LEN) => p = Predicate::IsNull(d.message(read_expr)?),
            (6, wire_type::LEN) => p = Predicate::IsNotNull(d.message(read_expr)?),
            (7, wire_type::LEN) => {
                p = d.message(|d| {
                    let mut lhs = Expr::Param(0);
                    let mut prefix = String::new();
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::LEN) => lhs = d.message(read_expr)?,
                            (2, wire_type::LEN) => prefix = d.string()?.to_string(),
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(Predicate::LikePrefix { lhs, prefix })
                })?
            }
            (8, wire_type::LEN) => p = Predicate::And(d.message(read_pred_list)?),
            (9, wire_type::LEN) => p = Predicate::Or(d.message(read_pred_list)?),
            (10, wire_type::LEN) => p = Predicate::Not(Box::new(d.message(read_pred)?)),
            _ => d.skip(t.wire)?,
        }
    }
    Ok(p)
}

fn read_pred_list(d: &mut Decoder<'_>) -> Result<Vec<Predicate>, WireError> {
    let mut out = Vec::new();
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::LEN) => out.push(d.message(read_pred)?),
            _ => d.skip(t.wire)?,
        }
    }
    Ok(out)
}

fn read_expr(d: &mut Decoder<'_>) -> Result<Expr, WireError> {
    let mut x = Expr::Param(0);
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::LEN) => x = Expr::Col(d.string()?.to_string()),
            (2, wire_type::LEN) => x = Expr::Lit(d.message(read_value)?),
            (3, wire_type::VARINT) => x = Expr::Param(d.varint()? as u32),
            _ => d.skip(t.wire)?,
        }
    }
    Ok(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plan §7's query, which is the one M0 exists to find out about.
    fn plan_7() -> Query {
        Query {
            table: "issues".into(),
            filter: Some(Predicate::And(vec![
                Predicate::Cmp {
                    lhs: Expr::Col("project_id".into()),
                    op: CmpOp::Eq,
                    rhs: Expr::Param(0),
                },
                Predicate::Cmp {
                    lhs: Expr::Col("closed".into()),
                    op: CmpOp::Eq,
                    rhs: Expr::Lit(Value::Int(0)),
                },
            ])),
            order_by: vec![Order {
                col: "priority".into(),
                desc: true,
            }],
            limit: 50,
            related: vec![Related {
                rel_name: "comments".into(),
                sub: Query {
                    table: "comments".into(),
                    order_by: vec![Order {
                        col: "created_at".into(),
                        desc: true,
                    }],
                    limit: 3,
                    ..Query::default()
                },
                as_name: String::new(),
            }],
            allow_unbounded: false,
            params: vec![Value::Int(7)],
        }
    }

    fn round_trip(q: &Query) -> Query {
        Query::decode(&q.encode()).expect("our own bytes")
    }

    #[test]
    fn the_query_m0_is_a_bet_on_survives_the_round_trip() {
        let q = plan_7();
        assert_eq!(round_trip(&q), q);
    }

    #[test]
    fn every_predicate_form_survives_the_round_trip() {
        let col = || Expr::Col("c".into());
        let forms = vec![
            Predicate::True,
            Predicate::Cmp {
                lhs: col(),
                op: CmpOp::Ge,
                rhs: Expr::Lit(Value::text("x")),
            },
            Predicate::In {
                lhs: col(),
                list: vec![Value::Int(1), Value::Null, Value::text("two")],
            },
            Predicate::Between {
                lhs: col(),
                low: Expr::Param(0),
                high: Expr::Param(1),
            },
            Predicate::IsNull(col()),
            Predicate::IsNotNull(col()),
            Predicate::LikePrefix {
                lhs: col(),
                prefix: "sol".into(),
            },
            Predicate::Not(Box::new(Predicate::Or(vec![
                Predicate::True,
                Predicate::IsNull(col()),
            ]))),
        ];
        for form in forms {
            let q = Query {
                table: "t".into(),
                filter: Some(form.clone()),
                ..Query::default()
            };
            assert_eq!(round_trip(&q).filter, Some(form));
        }
    }

    #[test]
    fn every_comparison_operator_round_trips_to_itself() {
        // A table of six one-line mappings is exactly the shape that gets
        // transposed by a copy-paste, and the symptom would be a view holding
        // the rows a `<` excludes rather than any kind of error.
        for op in [
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ] {
            let q = Query {
                filter: Some(Predicate::Cmp {
                    lhs: Expr::Col("c".into()),
                    op,
                    rhs: Expr::Param(0),
                }),
                ..Query::default()
            };
            let Some(Predicate::Cmp { op: back, .. }) = round_trip(&q).filter else {
                panic!("expected a comparison")
            };
            assert_eq!(back, op);
        }
    }

    #[test]
    fn an_unknown_comparison_is_refused_rather_than_guessed() {
        // The forward-compatibility promise in plan §3.5 is about *fields*. An
        // operator this version has never heard of cannot be skipped, because
        // there is no safe default: every choice returns some set of rows, and
        // the wrong set arrives as data rather than as an error.
        let mut e = Encoder::new();
        e.message(2, |e| {
            e.message(2, |e| {
                e.message(1, |e| e.bytes_field(1, b"c"));
                e.tag(2, wire_type::VARINT);
                e.varint(99);
            });
        });
        assert_eq!(
            Query::decode(&e.finish()),
            Err(WireError::UnknownEnum {
                field: 2,
                value: 99
            })
        );
    }

    #[test]
    fn an_absent_where_and_an_explicit_true_mean_the_same_thing() {
        let absent = Query {
            table: "t".into(),
            ..Query::default()
        };
        let explicit = Query {
            filter: Some(Predicate::True),
            ..absent.clone()
        };
        // They differ in the struct, because `Option` can say "not set" and the
        // engine may one day want to know. They must not differ in meaning, and
        // `True` must survive its own encoding — an empty message is the one
        // shape a decoder is most likely to lose.
        assert_eq!(round_trip(&explicit).filter, Some(Predicate::True));
        assert_eq!(round_trip(&absent).filter, None);
    }

    #[test]
    fn param_zero_is_not_mistaken_for_an_unset_expression() {
        // `$0` is the commonest parameter in any query, and proto3's rule of
        // omitting zero-valued scalars would erase it. In a oneof the tag is
        // the information.
        let q = Query {
            filter: Some(Predicate::IsNull(Expr::Param(0))),
            ..Query::default()
        };
        assert_eq!(
            round_trip(&q).filter,
            Some(Predicate::IsNull(Expr::Param(0)))
        );
    }

    #[test]
    fn a_nested_relation_carries_its_own_window() {
        // The property the whole 1:N traversal rests on: the child query has a
        // limit of its own, and losing it turns three comments per issue into
        // every comment ever written.
        let back = round_trip(&plan_7());
        assert_eq!(back.related.len(), 1);
        assert_eq!(back.related[0].sub.limit, 3);
        assert_eq!(back.related[0].sub.order_by[0].col, "created_at");
        assert!(back.related[0].sub.order_by[0].desc);
    }

    #[test]
    fn garbage_is_rejected_rather_than_believed() {
        for bad in [
            &b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff"[..],
            &b"\x12\xff\xff\xff\x7f"[..],
            &b"\x00"[..],
            &b"\x0c"[..],
        ] {
            assert!(Query::decode(bad).is_err(), "{bad:?} should not decode");
        }
    }
}

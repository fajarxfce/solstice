//! The view diff, encoded and decoded against
//! [`view.proto`](../../../proto/solstice/v1/view.proto).
//!
//! Field numbers appear as bare integers here rather than as constants. That is
//! on purpose: the `.proto` file is normative, this is its transcription, and a
//! reviewer should be able to read the two side by side without chasing a table
//! of names in between.

use solstice_ivm::{ColId, Row, Value};

use crate::wire::{wire_type, Decoder, Encoder, WireError};

/// One row-level change, with the position it happened at.
///
/// # Why this type lives in the encoding crate
///
/// It is really the product of the `Output` operator (plan §1.3), and when that
/// operator is built in M1 the type moves to `solstice-ivm` and this becomes
/// only its encoding. Defining it there *now* would put a public type in the
/// engine that the engine neither produces nor consumes — a shape asserted
/// ahead of the code that has to satisfy it. It lives here until something
/// emits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewChange {
    Added {
        index: u32,
        row: Row,
    },
    Removed {
        index: u32,
        key: Value,
    },
    Changed {
        index: u32,
        row: Row,
        cols: Vec<ColId>,
    },
    Moved {
        from: u32,
        to: u32,
    },
}

/// A coherent batch for one subscription at one version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewDelta {
    pub sub_id: u64,
    pub version: u64,
    pub changes: Vec<ViewChange>,
}

impl ViewDelta {
    pub fn encode(&self) -> Vec<u8> {
        // Guessing the payload size beats growing into it. A hydration of a
        // thousand joined rows reallocates eleven times from empty, copying the
        // frame each time; the guess does not have to be right to beat that.
        let mut e = Encoder::with_capacity(64 + self.changes.len() * 256);
        e.uint64_field(1, self.sub_id);
        e.uint64_field(2, self.version);
        for change in &self.changes {
            e.message(3, |e| put_change(e, change));
        }
        e.finish()
    }

    pub fn decode(buf: &[u8]) -> Result<ViewDelta, WireError> {
        let mut d = Decoder::new(buf);
        read_delta(&mut d)
    }
}

fn put_change(e: &mut Encoder, change: &ViewChange) {
    match change {
        ViewChange::Added { index, row } => e.message(1, |e| {
            e.uint32_field(1, *index);
            put_row(e, 2, row);
        }),
        ViewChange::Removed { index, key } => e.message(2, |e| {
            e.uint32_field(1, *index);
            put_value(e, 2, key);
        }),
        ViewChange::Changed { index, row, cols } => e.message(3, |e| {
            e.uint32_field(1, *index);
            put_row(e, 2, row);
            if !cols.is_empty() {
                // Packed, which is the proto3 default for repeated scalars: one
                // tag for the run instead of one per column.
                e.message(3, |e| {
                    for c in cols {
                        e.varint(*c as u64);
                    }
                });
            }
        }),
        ViewChange::Moved { from, to } => e.message(4, |e| {
            e.uint32_field(1, *from);
            e.uint32_field(2, *to);
        }),
    }
}

fn put_row(e: &mut Encoder, field: u32, row: &Row) {
    e.message(field, |e| {
        for v in row.values() {
            put_value(e, 1, v);
        }
    });
}

fn put_value(e: &mut Encoder, field: u32, v: &Value) {
    e.message(field, |e| match v {
        // NULL is the empty message: the oneof simply has no case set. Every
        // other arm writes its field even when the payload is zero, because in
        // a oneof the tag *is* the information — omitting `Int(0)` would decode
        // back as NULL.
        Value::Null => {}
        Value::Int(i) => e.sint64_field(1, *i),
        Value::Real(r) => e.double_field(2, *r),
        Value::Text(s) => e.bytes_field(3, s.as_bytes()),
        Value::Blob(b) => e.bytes_field(4, b),
        Value::Rows(rows) => e.message(5, |e| {
            for r in rows.iter() {
                put_row(e, 1, r);
            }
        }),
    });
}

fn read_delta(d: &mut Decoder<'_>) -> Result<ViewDelta, WireError> {
    let mut out = ViewDelta::default();
    while !d.is_done() {
        let tag = d.tag()?;
        match (tag.field, tag.wire) {
            (1, wire_type::VARINT) => out.sub_id = d.varint()?,
            (2, wire_type::VARINT) => out.version = d.varint()?,
            (3, wire_type::LEN) => out.changes.push(d.message(read_change)?),
            _ => d.skip(tag.wire)?,
        }
    }
    Ok(out)
}

fn read_change(d: &mut Decoder<'_>) -> Result<ViewChange, WireError> {
    // A oneof is a plain field on the wire, so "which case" is only known once
    // a tag is read — and a frame carrying none of them is legal protobuf.
    // `Moved { 0, 0 }` is the identity move, which is the right reading of a
    // change that declines to say what it changed.
    let mut change = ViewChange::Moved { from: 0, to: 0 };
    while !d.is_done() {
        let tag = d.tag()?;
        match (tag.field, tag.wire) {
            (1, wire_type::LEN) => {
                change = d.message(|d| {
                    let (index, row) = read_indexed_row(d)?;
                    Ok(ViewChange::Added { index, row })
                })?
            }
            (2, wire_type::LEN) => {
                change = d.message(|d| {
                    let mut index = 0;
                    let mut key = Value::Null;
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::VARINT) => index = d.varint()? as u32,
                            (2, wire_type::LEN) => key = d.message(read_value)?,
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(ViewChange::Removed { index, key })
                })?
            }
            (3, wire_type::LEN) => {
                change = d.message(|d| {
                    let mut index = 0;
                    let mut row = Row::new(Vec::new());
                    let mut cols = Vec::new();
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::VARINT) => index = d.varint()? as u32,
                            (2, wire_type::LEN) => row = d.message(read_row)?,
                            // Packed is what this encoder writes, but the spec
                            // lets any conformant peer send the unpacked form,
                            // so both are accepted. Encoders may choose;
                            // decoders may not.
                            (3, wire_type::LEN) => d.message(|d| {
                                while !d.is_done() {
                                    cols.push(d.varint()? as ColId);
                                }
                                Ok(())
                            })?,
                            (3, wire_type::VARINT) => cols.push(d.varint()? as ColId),
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(ViewChange::Changed { index, row, cols })
                })?
            }
            (4, wire_type::LEN) => {
                change = d.message(|d| {
                    let mut from = 0;
                    let mut to = 0;
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::VARINT) => from = d.varint()? as u32,
                            (2, wire_type::VARINT) => to = d.varint()? as u32,
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(ViewChange::Moved { from, to })
                })?
            }
            _ => d.skip(tag.wire)?,
        }
    }
    Ok(change)
}

fn read_indexed_row(d: &mut Decoder<'_>) -> Result<(u32, Row), WireError> {
    let mut index = 0;
    let mut row = Row::new(Vec::new());
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::VARINT) => index = d.varint()? as u32,
            (2, wire_type::LEN) => row = d.message(read_row)?,
            _ => d.skip(t.wire)?,
        }
    }
    Ok((index, row))
}

fn read_row(d: &mut Decoder<'_>) -> Result<Row, WireError> {
    let mut values = Vec::new();
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::LEN) => values.push(d.message(read_value)?),
            _ => d.skip(t.wire)?,
        }
    }
    Ok(Row::new(values))
}

fn read_value(d: &mut Decoder<'_>) -> Result<Value, WireError> {
    let mut v = Value::Null;
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::VARINT) => v = Value::Int(crate::wire::unzigzag(d.varint()?)),
            (2, wire_type::I64) => v = Value::Real(f64::from_bits(d.fixed64()?)),
            (3, wire_type::LEN) => v = Value::text(d.string()?),
            (4, wire_type::LEN) => v = Value::blob(d.bytes()?),
            (5, wire_type::LEN) => {
                let rows = d.message(|d| {
                    let mut rows = Vec::new();
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::LEN) => rows.push(d.message(read_row)?),
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(rows)
                })?;
                v = Value::rows(rows);
            }
            _ => d.skip(t.wire)?,
        }
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(changes: Vec<ViewChange>) -> ViewDelta {
        ViewDelta {
            sub_id: 7,
            version: 42,
            changes,
        }
    }

    fn round_trip(d: &ViewDelta) -> ViewDelta {
        ViewDelta::decode(&d.encode()).expect("decodes")
    }

    #[test]
    fn every_change_kind_survives_the_round_trip() {
        let d = delta(vec![
            ViewChange::Added {
                index: 0,
                row: Row::new(vec![Value::Int(1), Value::text("hi")]),
            },
            ViewChange::Removed {
                index: 3,
                key: Value::Int(99),
            },
            ViewChange::Changed {
                index: 2,
                row: Row::new(vec![Value::Real(1.5)]),
                cols: vec![0, 4, 9],
            },
            ViewChange::Moved { from: 5, to: 1 },
        ]);
        assert_eq!(round_trip(&d), d);
    }

    #[test]
    fn null_is_the_absence_of_a_case_and_not_a_zero() {
        // The distinction the whole `Value` encoding turns on. A NULL column
        // and an integer column holding 0 must not decode to the same thing —
        // if they did, `priority IS NULL` and `priority = 0` would select the
        // same rows after a round trip.
        let d = delta(vec![ViewChange::Added {
            index: 0,
            row: Row::new(vec![
                Value::Null,
                Value::Int(0),
                Value::Real(0.0),
                Value::text(""),
                Value::blob([0u8; 0]),
            ]),
        }]);
        let back = round_trip(&d);
        assert_eq!(back, d);

        let ViewChange::Added { row, .. } = &back.changes[0] else {
            panic!("expected Added")
        };
        assert!(row.get(0).is_null());
        assert!(!row.get(1).is_null());
        assert_ne!(row.get(0), row.get(1));
    }

    #[test]
    fn a_null_column_costs_two_bytes() {
        // Why the unset-oneof encoding is worth the subtlety: a nullable column
        // is exactly where nulls cluster, and this is a tag plus a zero length.
        let one = |v: Value| ViewDelta {
            sub_id: 0,
            version: 0,
            changes: vec![ViewChange::Added {
                index: 0,
                row: Row::new(vec![v]),
            }],
        };
        let null = one(Value::Null).encode().len();
        let zero = one(Value::Int(0)).encode().len();
        assert_eq!(zero - null, 2);
    }

    #[test]
    fn nested_children_ride_inside_the_parent_row() {
        // The hierarchical shape the join produces: three comments hanging off
        // one issue, in one row, in one change.
        let comments = (0..3)
            .map(|i| Row::new(vec![Value::Int(i), Value::text(format!("comment {i}"))]))
            .collect::<Vec<_>>();
        let d = delta(vec![ViewChange::Added {
            index: 0,
            row: Row::new(vec![
                Value::Int(1),
                Value::text("an issue"),
                Value::rows(comments.clone()),
            ]),
        }]);

        let back = round_trip(&d);
        assert_eq!(back, d);
        let ViewChange::Added { row, .. } = &back.changes[0] else {
            panic!("expected Added")
        };
        let Value::Rows(children) = row.get(2) else {
            panic!("expected children")
        };
        assert_eq!(children.len(), 3);
        assert_eq!(children[1].get(1), &Value::text("comment 1"));
    }

    #[test]
    fn an_empty_child_collection_is_not_null() {
        // An issue with no comments yet. The join emits an empty collection and
        // the host must render "no comments", not "unknown".
        let d = delta(vec![ViewChange::Added {
            index: 0,
            row: Row::new(vec![Value::rows(Vec::<Row>::new())]),
        }]);
        let back = round_trip(&d);
        assert_eq!(back, d);
        let ViewChange::Added { row, .. } = &back.changes[0] else {
            panic!("expected Added")
        };
        assert!(matches!(row.get(0), Value::Rows(r) if r.is_empty()));
        assert!(!row.get(0).is_null());
    }

    #[test]
    fn an_unpacked_cols_field_decodes_the_same_as_a_packed_one() {
        // A conformant peer may send either form; this encoder sends packed.
        let mut e = Encoder::new();
        e.message(3, |e| {
            e.message(3, |e| {
                e.uint32_field(1, 1);
                e.uint32_field(3, 7);
                e.uint32_field(3, 8);
            });
        });
        let buf = e.finish();
        let back = ViewDelta::decode(&buf).expect("decodes");
        assert_eq!(
            back.changes,
            vec![ViewChange::Changed {
                index: 1,
                row: Row::new(Vec::new()),
                cols: vec![7, 8],
            }]
        );
    }

    #[test]
    fn a_frame_from_a_newer_peer_still_decodes() {
        // Plan §3.5's promise, exercised: unknown fields at three levels of
        // nesting are skipped, and the known ones come through intact.
        let mut e = Encoder::new();
        e.uint64_field(1, 5);
        e.bytes_field(77, b"a field this version has never heard of");
        e.message(3, |e| {
            e.uint32_field(66, 1);
            e.message(1, |e| {
                e.uint32_field(1, 2);
                e.message(2, |e| {
                    e.message(1, |e| e.sint64_field(1, -3));
                    e.double_field(55, 1.0);
                });
            });
        });
        let back = ViewDelta::decode(&e.finish()).expect("decodes");
        assert_eq!(back.sub_id, 5);
        assert_eq!(
            back.changes,
            vec![ViewChange::Added {
                index: 2,
                row: Row::new(vec![Value::Int(-3)]),
            }]
        );
    }

    #[test]
    fn garbage_is_rejected_rather_than_believed() {
        // Not a fuzz test — that comes with `cargo-fuzz` (plan §6) — but the
        // property it protects is that a bad frame is an error value, never a
        // panic and never an allocation the sender chose the size of.
        for bad in [
            &b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff"[..],
            &b"\x1a\xff\xff\xff\x7f"[..],
            &b"\x00"[..],
            &b"\x1c"[..],
        ] {
            assert!(ViewDelta::decode(bad).is_err(), "{bad:?} should not decode");
        }
    }

    #[test]
    fn deep_nesting_is_refused_before_it_reaches_the_stack() {
        // `Value::Rows` is recursive, so a frame can nest arbitrarily and a
        // naive decoder would recurse until the stack ran out. DQL caps
        // traversal at depth 3; anything near the limit is already hostile.
        // The cycle in the schema is three messages long:
        // `Row.values` → `Value.rows` → `RowList.rows` → `Row`.
        let mut buf = Vec::new();
        for _ in 0..100 {
            let mut e = Encoder::new();
            e.message(1, |e| e.message(5, |e| e.message(1, |e| e.raw(&buf))));
            buf = e.finish();
        }
        let mut framed = Encoder::new();
        framed.message(3, |e| e.message(1, |e| e.message(2, |e| e.raw(&buf))));
        assert_eq!(ViewDelta::decode(&framed.finish()), Err(WireError::TooDeep));
    }
}

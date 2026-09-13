//! Mutation bodies, encoded and decoded against
//! [`mutation.proto`](../../../proto/solstice/v1/mutation.proto).
//!
//! Names again, not ids — for the reason in [`query`](crate::query), plus one
//! that is specific to this message: a mutation outlives the process that
//! created it. It sits in the local log across restarts and is replayed against
//! a base that has moved (plan §2.3), so anything in it that meant "the second
//! table this process happened to register" would be a corruption waiting for
//! the next schema change.

use solstice_ivm::{Row, Value};

use crate::view::{put_row, put_value, read_row, read_value};
use crate::wire::{wire_type, Decoder, Encoder, WireError};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mutation {
    pub client_id: Vec<u8>,
    pub mutation_id: u64,
    /// Advisory. Never used for ordering.
    pub timestamp_ms: i64,
    pub ops: Vec<Op>,
    /// Operations whose case this version does not recognise.
    ///
    /// Counted rather than dropped silently. Plan §3.5's forward-compatibility
    /// promise lets an old client keep *reading* a newer peer's frames, and for
    /// a view diff that is the right behaviour — a column it cannot show is a
    /// column it does not show. A mutation is the opposite: applying the half
    /// of a body that this version understands writes a state the sender never
    /// asked for, into a store whose whole soundness argument is that one code
    /// path owns every write (plan §1.4). So the count comes out of the decoder
    /// and the engine refuses the body.
    pub unknown_ops: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Insert {
        table: String,
        row: Row,
    },
    Update {
        table: String,
        key: Value,
        set: Vec<(String, Value)>,
    },
    Delete {
        table: String,
        key: Value,
    },
}

impl Mutation {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        if !self.client_id.is_empty() {
            e.bytes_field(2, &self.client_id);
        }
        e.uint64_field(3, self.mutation_id);
        if self.timestamp_ms != 0 {
            e.sint64_field(4, self.timestamp_ms);
        }
        // The body is a oneof, so `Patch` is written even when it holds no ops:
        // a mutation with an empty patch is a mutation that does nothing, which
        // is different from a mutation whose body this version cannot read.
        e.message(5, |e| {
            for op in &self.ops {
                e.message(1, |e| put_op(e, op));
            }
        });
        e.finish()
    }

    pub fn decode(buf: &[u8]) -> Result<Mutation, WireError> {
        let mut m = Mutation::default();
        let mut d = Decoder::new(buf);
        while !d.is_done() {
            let t = d.tag()?;
            match (t.field, t.wire) {
                (2, wire_type::LEN) => m.client_id = d.bytes()?.to_vec(),
                (3, wire_type::VARINT) => m.mutation_id = d.varint()?,
                (4, wire_type::VARINT) => {
                    m.timestamp_ms = crate::wire::unzigzag(d.varint()?);
                }
                (5, wire_type::LEN) => {
                    let (ops, unknown) = d.message(|d| {
                        let mut ops = Vec::new();
                        let mut unknown = 0;
                        while !d.is_done() {
                            let t = d.tag()?;
                            match (t.field, t.wire) {
                                (1, wire_type::LEN) => match d.message(read_op)? {
                                    Some(op) => ops.push(op),
                                    None => unknown += 1,
                                },
                                _ => d.skip(t.wire)?,
                            }
                        }
                        Ok((ops, unknown))
                    })?;
                    m.ops = ops;
                    m.unknown_ops = unknown;
                }
                _ => d.skip(t.wire)?,
            }
        }
        Ok(m)
    }
}

fn put_op(e: &mut Encoder, op: &Op) {
    match op {
        Op::Insert { table, row } => e.message(1, |e| {
            e.bytes_field(1, table.as_bytes());
            put_row(e, 2, row);
        }),
        Op::Update { table, key, set } => e.message(2, |e| {
            e.bytes_field(1, table.as_bytes());
            put_value(e, 2, key);
            for (col, value) in set {
                e.message(3, |e| {
                    e.bytes_field(1, col.as_bytes());
                    put_value(e, 2, value);
                });
            }
        }),
        Op::Delete { table, key } => e.message(3, |e| {
            e.bytes_field(1, table.as_bytes());
            put_value(e, 2, key);
        }),
    }
}

/// `None` for an operation whose case this version does not recognise, counted
/// into [`Mutation::unknown_ops`].
fn read_op(d: &mut Decoder<'_>) -> Result<Option<Op>, WireError> {
    let mut op = None;
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::LEN) => {
                op = Some(d.message(|d| {
                    let mut table = String::new();
                    let mut row = Row::new(Vec::new());
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::LEN) => table = d.string()?.to_string(),
                            (2, wire_type::LEN) => row = d.message(read_row)?,
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(Op::Insert { table, row })
                })?)
            }
            (2, wire_type::LEN) => {
                op = Some(d.message(|d| {
                    let mut table = String::new();
                    let mut key = Value::Null;
                    let mut set = Vec::new();
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::LEN) => table = d.string()?.to_string(),
                            (2, wire_type::LEN) => key = d.message(read_value)?,
                            (3, wire_type::LEN) => set.push(d.message(read_col_value)?),
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(Op::Update { table, key, set })
                })?)
            }
            (3, wire_type::LEN) => {
                op = Some(d.message(|d| {
                    let mut table = String::new();
                    let mut key = Value::Null;
                    while !d.is_done() {
                        let t = d.tag()?;
                        match (t.field, t.wire) {
                            (1, wire_type::LEN) => table = d.string()?.to_string(),
                            (2, wire_type::LEN) => key = d.message(read_value)?,
                            _ => d.skip(t.wire)?,
                        }
                    }
                    Ok(Op::Delete { table, key })
                })?)
            }
            _ => d.skip(t.wire)?,
        }
    }
    Ok(op)
}

fn read_col_value(d: &mut Decoder<'_>) -> Result<(String, Value), WireError> {
    let mut col = String::new();
    let mut value = Value::Null;
    while !d.is_done() {
        let t = d.tag()?;
        match (t.field, t.wire) {
            (1, wire_type::LEN) => col = d.string()?.to_string(),
            (2, wire_type::LEN) => value = d.message(read_value)?,
            _ => d.skip(t.wire)?,
        }
    }
    Ok((col, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(m: &Mutation) -> Mutation {
        Mutation::decode(&m.encode()).expect("our own bytes")
    }

    #[test]
    fn every_operation_survives_the_round_trip() {
        let m = Mutation {
            client_id: b"device-1".to_vec(),
            mutation_id: 42,
            timestamp_ms: -1,
            ops: vec![
                Op::Insert {
                    table: "issues".into(),
                    row: Row::new(vec![Value::Int(1), Value::text("new"), Value::Null]),
                },
                Op::Update {
                    table: "issues".into(),
                    key: Value::Int(1),
                    set: vec![
                        ("closed".into(), Value::Int(1)),
                        ("priority".into(), Value::Null),
                    ],
                },
                Op::Delete {
                    table: "comments".into(),
                    key: Value::Int(9),
                },
            ],
            unknown_ops: 0,
        };
        assert_eq!(round_trip(&m), m);
    }

    #[test]
    fn an_update_that_sets_a_column_to_null_is_not_an_update_that_skips_it() {
        // `set` carries the columns the mutation *names*, and NULL is a value
        // like any other. A decoder that dropped null-valued entries would turn
        // "clear the priority" into "leave the priority alone", and the client
        // would show a cleared field that the server never cleared.
        let m = Mutation {
            ops: vec![Op::Update {
                table: "issues".into(),
                key: Value::Int(1),
                set: vec![("priority".into(), Value::Null)],
            }],
            ..Mutation::default()
        };
        let Op::Update { set, .. } = &round_trip(&m).ops[0] else {
            panic!("expected an update")
        };
        assert_eq!(set.len(), 1);
        assert_eq!(set[0], ("priority".to_string(), Value::Null));
    }

    #[test]
    fn the_order_of_set_entries_is_the_order_it_was_written_in() {
        // Why this is a repeated pair and not a `map`: plan §1.2 hashes these
        // for identity, and a map has no defined wire order, so two encoders
        // could produce two hashes for one mutation.
        let cols = ["z", "a", "m", "b"];
        let m = Mutation {
            ops: vec![Op::Update {
                table: "t".into(),
                key: Value::Int(1),
                set: cols
                    .iter()
                    .map(|c| (c.to_string(), Value::Int(0)))
                    .collect(),
            }],
            ..Mutation::default()
        };
        let Op::Update { set, .. } = &round_trip(&m).ops[0] else {
            panic!("expected an update")
        };
        let got: Vec<&str> = set.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(got, cols);
    }

    #[test]
    fn an_operation_from_a_newer_peer_is_counted_and_not_swallowed() {
        // The frame is well formed and the known operation decodes. What must
        // not happen is that the unknown one disappears without trace, leaving
        // the engine to apply a body it only half understood.
        let mut e = Encoder::new();
        e.message(5, |e| {
            e.message(1, |e| {
                // `increment`, reserved and not implemented.
                e.message(5, |e| e.bytes_field(1, b"reactions"));
            });
            e.message(1, |e| {
                put_op(
                    e,
                    &Op::Delete {
                        table: "t".into(),
                        key: Value::Int(1),
                    },
                )
            });
        });
        let back = Mutation::decode(&e.finish()).expect("the frame is well formed");
        assert_eq!(back.ops.len(), 1);
        assert_eq!(back.unknown_ops, 1);
    }

    #[test]
    fn garbage_is_rejected_rather_than_believed() {
        for bad in [
            &b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff"[..],
            &b"\x2a\xff\xff\xff\x7f"[..],
            &b"\x00"[..],
        ] {
            assert!(Mutation::decode(bad).is_err(), "{bad:?} should not decode");
        }
    }
}

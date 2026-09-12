//! The encoding's law, and the decoder's one hard guarantee.
//!
//! `solstice-ivm` has the delta law; this crate has a smaller one:
//!
//! ```text
//! for all view deltas D:    decode(encode(D)) == D
//! ```
//!
//! Smaller, but not less load-bearing. This is the boundary where the engine
//! stops being able to check itself: an operator that drops a column is caught
//! by the triple oracle, while an *encoder* that drops a column produces a view
//! the engine still believes is correct and the user can see is not.
//!
//! The second property is the one that matters for a parser reading bytes off a
//! socket (plan §6 wants `cargo-fuzz` on exactly this): arbitrary input must
//! produce an error, never a panic and never an allocation sized by the sender.

use proptest::prelude::*;
use solstice_ivm::{Row, Value};
use solstice_proto::{ViewChange, ViewDelta};

/// Values, including the nested child collections a join produces.
///
/// Depth is capped at 3 because that is where DQL caps relation traversal (plan
/// §1.1); generating deeper would test a frame the query language cannot ask
/// for. Text and blobs stay short on purpose — payload *size* is the fixture
/// binary's job, and a proptest that spends its budget on long strings explores
/// fewer shapes.
fn any_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<i64>().prop_map(Value::Int),
        any::<f64>().prop_map(Value::Real),
        ".{0,12}".prop_map(Value::text),
        prop::collection::vec(any::<u8>(), 0..8).prop_map(Value::blob),
    ];
    leaf.prop_recursive(3, 32, 4, |inner| {
        prop::collection::vec(prop::collection::vec(inner, 0..4).prop_map(Row::new), 0..4)
            .prop_map(Value::rows)
    })
}

fn any_row() -> impl Strategy<Value = Row> {
    prop::collection::vec(any_value(), 0..6).prop_map(Row::new)
}

fn any_change() -> impl Strategy<Value = ViewChange> {
    prop_oneof![
        (any::<u32>(), any_row()).prop_map(|(index, row)| ViewChange::Added { index, row }),
        (any::<u32>(), any_value()).prop_map(|(index, key)| ViewChange::Removed { index, key }),
        (
            any::<u32>(),
            any_row(),
            prop::collection::vec(any::<u16>(), 0..5)
        )
            .prop_map(|(index, row, cols)| ViewChange::Changed { index, row, cols }),
        (any::<u32>(), any::<u32>()).prop_map(|(from, to)| ViewChange::Moved { from, to }),
    ]
}

fn any_delta() -> impl Strategy<Value = ViewDelta> {
    (
        any::<u64>(),
        any::<u64>(),
        prop::collection::vec(any_change(), 0..8),
    )
        .prop_map(|(sub_id, version, changes)| ViewDelta {
            sub_id,
            version,
            changes,
        })
}

proptest! {
    #[test]
    fn a_delta_survives_the_round_trip(delta in any_delta()) {
        let bytes = delta.encode();
        let back = ViewDelta::decode(&bytes).expect("our own bytes must decode");
        prop_assert_eq!(back, delta);
    }

    /// Encoding is a function of the value, not of the buffer it lands in.
    ///
    /// The length-prefix splice rewrites bytes that are already written, which
    /// is exactly the kind of trick that works until a payload crosses a varint
    /// boundary and starts depending on what preceded it. Encoding the same
    /// delta alone and as the tail of a longer run must give the same bytes.
    #[test]
    fn encoding_does_not_depend_on_what_came_before(
        head in prop::collection::vec(any_change(), 0..4),
        tail in any_change(),
    ) {
        let alone = ViewDelta { sub_id: 0, version: 0, changes: vec![tail.clone()] };
        let mut together = head;
        together.push(tail);
        let together = ViewDelta { sub_id: 0, version: 0, changes: together };

        let alone = alone.encode();
        let together = together.encode();
        prop_assert!(
            together.ends_with(&alone),
            "the last change encoded differently in company"
        );
    }

    /// Arbitrary bytes are rejected, not believed — and above all, not fatal.
    ///
    /// No assertion on the outcome: a random buffer is occasionally a valid
    /// frame, and demanding an error would make this test a statement about
    /// proptest's luck. The property is that `decode` *returns*.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = ViewDelta::decode(&bytes);
    }

    /// Truncation is the common real failure — a short read, a closed socket,
    /// a frame cut by a reconnect — and it must not be the one that panics.
    #[test]
    fn every_prefix_of_a_valid_frame_is_survivable(delta in any_delta()) {
        let bytes = delta.encode();
        for cut in 0..bytes.len() {
            let _ = ViewDelta::decode(&bytes[..cut]);
        }
    }

    /// Corrupting one byte must not crash the decoder either. A flipped bit in
    /// a length prefix is how a frame asks for a gigabyte.
    #[test]
    fn a_single_flipped_byte_never_panics(
        delta in any_delta(),
        at in any::<prop::sample::Index>(),
        to in any::<u8>(),
    ) {
        let mut bytes = delta.encode();
        prop_assume!(!bytes.is_empty());
        let i = at.index(bytes.len());
        bytes[i] = to;
        let _ = ViewDelta::decode(&bytes);
    }
}

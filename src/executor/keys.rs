use std::ops::Bound;

use crate::{
    planner::{
        physical::{KeyRange, ValueRange},
        types::DataType,
        value::Value,
    },
    storage::engine::bplus_tree::Key,
};

pub type TableId = u16;
pub type ObjectId = u16;

pub const CATALOG_TABLE: TableId = 0;
pub const FIRST_USER_TABLE: TableId = 1;

const ID_SHIFT: u32 = 112;
const PREFIX_SHIFT: u32 = 64;
const PREFIX_BITS: u32 = ID_SHIFT - PREFIX_SHIFT;
pub const PREFIX_MAX: u64 = (1 << PREFIX_BITS) - 1;
const SIGN: u64 = 1 << 63;

pub const MAX_HIDDEN_ROW_ID: u64 = u64::MAX - 1;

pub fn encode_row_key(key: i64) -> u64 {
    key as u64 ^ SIGN
}

pub fn decode_row_key(bits: u64) -> i64 {
    (bits ^ SIGN) as i64
}

pub fn tree_key(table: TableId, row_bits: u64) -> Key {
    ((table as Key) << ID_SHIFT) | row_bits as Key
}

pub fn index_key(index: ObjectId, prefix: u64, row_bits: u64) -> Key {
    ((index as Key) << ID_SHIFT)
        | (((prefix & PREFIX_MAX) as Key) << PREFIX_SHIFT)
        | row_bits as Key
}

pub fn row_bits(tree_key: Key) -> u64 {
    tree_key as u64
}

pub fn object_id(tree_key: Key) -> ObjectId {
    (tree_key >> ID_SHIFT) as ObjectId
}

pub fn prefix_of(tree_key: Key) -> u64 {
    (tree_key >> PREFIX_SHIFT) as u64 & PREFIX_MAX
}

pub fn object_bounds(id: ObjectId) -> (Bound<Key>, Bound<Key>) {
    let low = (id as Key) << ID_SHIFT;
    (
        Bound::Included(low),
        Bound::Included(low | ((1 << ID_SHIFT) - 1)),
    )
}

pub fn table_bounds(table: TableId) -> (Bound<Key>, Bound<Key>) {
    (
        Bound::Included(tree_key(table, 0)),
        Bound::Included(tree_key(table, u64::MAX)),
    )
}

pub fn prefix_bounds(index: ObjectId, low: u64, high: u64) -> (Bound<Key>, Bound<Key>) {
    (
        Bound::Included(index_key(index, low, 0)),
        Bound::Included(index_key(index, high, u64::MAX)),
    )
}

pub fn index_prefix(column_type: DataType, value: &Value) -> Option<u64> {
    const HALF: i64 = 1 << (PREFIX_BITS - 1);
    match value {
        Value::Null => None,
        Value::Boolean(b) => Some(*b as u64),
        Value::Integer(i) if column_type == DataType::Float => {
            index_prefix(column_type, &Value::Float(*i as f64))
        }
        Value::Integer(i) => Some((i.clamp(&-HALF, &(HALF - 1)) + HALF) as u64),
        Value::Float(x) => {
            let x = if *x == 0.0 {
                0.0
            } else if x.is_nan() {
                f64::NAN
            } else {
                *x
            };
            let bits = x.to_bits();
            let ordered = if bits & SIGN != 0 { !bits } else { bits | SIGN };
            Some(ordered >> (64 - PREFIX_BITS))
        }
        Value::Text(s) => {
            let mut bytes = [0u8; 8];
            let n = s.len().min(PREFIX_BITS as usize / 8);
            bytes[2..2 + n].copy_from_slice(&s.as_bytes()[..n]);
            Some(u64::from_be_bytes(bytes))
        }
    }
}

pub fn value_range_prefixes(column_type: DataType, range: &ValueRange) -> Option<(u64, u64)> {
    let low = match &range.lower {
        Bound::Included(v) | Bound::Excluded(v) => index_prefix(column_type, v)?,
        Bound::Unbounded => 0,
    };
    let high = match &range.upper {
        Bound::Included(v) | Bound::Excluded(v) => index_prefix(column_type, v)?,
        Bound::Unbounded => PREFIX_MAX,
    };
    (low <= high).then_some((low, high))
}

pub fn key_range_bounds(table: TableId, range: &KeyRange) -> Option<(Bound<Key>, Bound<Key>)> {
    let low = match range.lower {
        Bound::Included(v) => v as i128,
        Bound::Excluded(v) => v as i128 + 1,
        Bound::Unbounded => i64::MIN as i128,
    };
    let high = match range.upper {
        Bound::Included(v) => v as i128,
        Bound::Excluded(v) => v as i128 - 1,
        Bound::Unbounded => i64::MAX as i128,
    };
    if low > high || low > i64::MAX as i128 || high < i64::MIN as i128 {
        return None;
    }
    let encode = |v: i128| tree_key(table, encode_row_key(v as i64));
    Some((Bound::Included(encode(low)), Bound::Included(encode(high))))
}

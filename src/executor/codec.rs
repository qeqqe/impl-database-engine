use crate::{
    executor::error::{DbError, DbResult},
    planner::{
        types::DataType,
        value::{Row, Value},
    },
};

const ROW_FORMAT_V1: u8 = 1;

pub fn encode_row(types: &[DataType], row: &[Value]) -> DbResult<Vec<u8>> {
    if types.len() != row.len() {
        return Err(DbError::Internal(format!(
            "row has {} values for {} columns",
            row.len(),
            types.len()
        )));
    }
    let bitmap_len = types.len().div_ceil(8);
    let mut out = Vec::with_capacity(1 + bitmap_len + row.len() * 8);
    out.push(ROW_FORMAT_V1);
    out.resize(1 + bitmap_len, 0);

    for (i, (ty, value)) in types.iter().zip(row).enumerate() {
        match (ty, value) {
            (_, Value::Null) => out[1 + i / 8] |= 1 << (i % 8),
            (DataType::Integer, Value::Integer(v)) => out.extend_from_slice(&v.to_le_bytes()),
            (DataType::Float, Value::Float(v)) => out.extend_from_slice(&v.to_bits().to_le_bytes()),
            (DataType::Boolean, Value::Boolean(v)) => out.push(u8::from(*v)),
            (t, Value::Text(s)) if t.is_textual() => {
                let len = u32::try_from(s.len())
                    .map_err(|_| DbError::Internal("text value longer than 4 GiB".into()))?;
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            (t, v) => {
                return Err(DbError::Internal(format!(
                    "value {v} of type {} stored in column {i} of type {t}",
                    v.data_type()
                )));
            }
        }
    }
    Ok(out)
}

pub fn decode_row(types: &[DataType], bytes: &[u8]) -> DbResult<Row> {
    let corrupt = |msg: &str| DbError::Corrupt(msg.to_string());
    let (&version, rest) = bytes.split_first().ok_or_else(|| corrupt("empty row"))?;
    if version != ROW_FORMAT_V1 {
        return Err(corrupt(&format!("unknown row format {version}")));
    }
    let bitmap_len = types.len().div_ceil(8);
    if rest.len() < bitmap_len {
        return Err(corrupt("row shorter than its null bitmap"));
    }
    let (bitmap, mut data) = rest.split_at(bitmap_len);
    if !types.len().is_multiple_of(8) && bitmap[bitmap_len - 1] >> (types.len() % 8) != 0 {
        return Err(corrupt("null bitmap marks a column that does not exist"));
    }

    let mut take = |n: usize| -> DbResult<&[u8]> {
        if data.len() < n {
            return Err(corrupt("row ends in the middle of a value"));
        }
        let (head, tail) = data.split_at(n);
        data = tail;
        Ok(head)
    };

    let mut row = Vec::with_capacity(types.len());
    for (i, ty) in types.iter().enumerate() {
        if bitmap[i / 8] & (1 << (i % 8)) != 0 {
            row.push(Value::Null);
            continue;
        }
        let value = match ty {
            DataType::Integer => Value::Integer(i64::from_le_bytes(take(8)?.try_into().unwrap())),
            DataType::Float => Value::Float(f64::from_bits(u64::from_le_bytes(
                take(8)?.try_into().unwrap(),
            ))),
            DataType::Boolean => match take(1)?[0] {
                0 => Value::Boolean(false),
                1 => Value::Boolean(true),
                b => return Err(corrupt(&format!("boolean byte {b} is not 0 or 1"))),
            },
            t if t.is_textual() => {
                let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
                let text = std::str::from_utf8(take(len)?)
                    .map_err(|_| corrupt("text value is not UTF-8"))?;
                Value::Text(text.to_string())
            }
            other => return Err(corrupt(&format!("column {i} has unstorable type {other}"))),
        };
        row.push(value);
    }
    if !data.is_empty() {
        return Err(corrupt("trailing bytes after the last value"));
    }
    Ok(row)
}

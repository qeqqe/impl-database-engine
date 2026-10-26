use std::cmp::Ordering;

use crate::planner::physical::PhysSortKey;
use crate::planner::schema::Schema;
use crate::planner::value::{Row, Value};

use super::super::error::ExecResult;
use super::super::operator::{BoxedOperator, Operator, not_open};

type Keyed = (Vec<Value>, Row);

fn compare(a: &[Value], b: &[Value], keys: &[PhysSortKey]) -> Ordering {
    for ((x, y), key) in a.iter().zip(b).zip(keys) {
        let ordering = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) if key.nulls_first => Ordering::Less,
            (true, false) => Ordering::Greater,
            (false, true) if key.nulls_first => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) if key.ascending => x.cmp(y),
            (false, false) => y.cmp(x),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn keyed(keys: &[PhysSortKey], row: Row) -> ExecResult<Keyed> {
    let values = keys
        .iter()
        .map(|k| k.expr.eval(&row))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((values, row))
}

fn sort_keyed(rows: &mut [Keyed], keys: &[PhysSortKey]) {
    rows.sort_by(|a, b| compare(&a.0, &b.0, keys));
}

pub struct SortExec<'a> {
    input: BoxedOperator<'a>,
    keys: Vec<PhysSortKey>,
    output: Option<std::vec::IntoIter<Keyed>>,
}

impl<'a> SortExec<'a> {
    pub fn new(input: BoxedOperator<'a>, keys: Vec<PhysSortKey>) -> Self {
        Self {
            input,
            keys,
            output: None,
        }
    }
}

impl Operator for SortExec<'_> {
    fn schema(&self) -> &Schema {
        self.input.schema()
    }

    fn open(&mut self) -> ExecResult<()> {
        self.input.open()?;
        let mut rows = Vec::new();
        while let Some(row) = self.input.next()? {
            rows.push(keyed(&self.keys, row)?);
        }
        sort_keyed(&mut rows, &self.keys);
        self.output = Some(rows.into_iter());
        Ok(())
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        let output = self.output.as_mut().ok_or_else(|| not_open("Sort"))?;
        Ok(output.next().map(|(_, row)| row))
    }

    fn close(&mut self) -> ExecResult<()> {
        self.output = None;
        self.input.close()
    }
}

pub struct TopNExec<'a> {
    input: BoxedOperator<'a>,
    keys: Vec<PhysSortKey>,
    limit: usize,
    offset: usize,
    output: Option<std::vec::IntoIter<Keyed>>,
}

impl<'a> TopNExec<'a> {
    pub fn new(
        input: BoxedOperator<'a>,
        keys: Vec<PhysSortKey>,
        limit: usize,
        offset: usize,
    ) -> Self {
        Self {
            input,
            keys,
            limit,
            offset,
            output: None,
        }
    }
}

impl Operator for TopNExec<'_> {
    fn schema(&self) -> &Schema {
        self.input.schema()
    }

    fn open(&mut self) -> ExecResult<()> {
        self.input.open()?;
        let keep = self.limit.saturating_add(self.offset);
        let threshold = keep.saturating_mul(2).max(64);
        let mut buffer: Vec<Keyed> = Vec::new();
        while let Some(row) = self.input.next()? {
            buffer.push(keyed(&self.keys, row)?);
            if buffer.len() >= threshold {
                sort_keyed(&mut buffer, &self.keys);
                buffer.truncate(keep);
            }
        }
        sort_keyed(&mut buffer, &self.keys);
        buffer.truncate(keep);
        let skip = self.offset.min(buffer.len());
        buffer.drain(..skip);
        self.output = Some(buffer.into_iter());
        Ok(())
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        let output = self.output.as_mut().ok_or_else(|| not_open("TopN"))?;
        Ok(output.next().map(|(_, row)| row))
    }

    fn close(&mut self) -> ExecResult<()> {
        self.output = None;
        self.input.close()
    }
}

#[cfg(test)]
mod tests {
    use crate::planner::physical_expr::PhysExpr;

    use super::*;
    fn key(ascending: bool, nulls_first: bool) -> PhysSortKey {
        PhysSortKey {
            expr: PhysExpr::Column {
                index: 0,
                name: "a".into(),
            },
            ascending,
            nulls_first,
        }
    }

    #[test]
    fn nulls_placement_follows_key() {
        let null = [Value::Null];
        let one = [Value::Integer(1)];
        assert_eq!(compare(&null, &one, &[key(true, false)]), Ordering::Greater);
        assert_eq!(compare(&null, &one, &[key(true, true)]), Ordering::Less);
        assert_eq!(compare(&null, &one, &[key(false, true)]), Ordering::Less);
    }

    #[test]
    fn descending_reverses_values_only() {
        let a = [Value::Integer(1)];
        let b = [Value::Integer(2)];
        assert_eq!(compare(&a, &b, &[key(false, true)]), Ordering::Greater);
        assert_eq!(compare(&a, &b, &[key(true, false)]), Ordering::Less);
    }
}

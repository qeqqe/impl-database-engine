use std::{
    collections::{BTreeSet, HashMap},
    ops::Bound,
};

use crate::{
    execution::{
        error::{ExecError, ExecResult},
        source::{RowStream, TableSource},
    },
    executor::{
        codec::decode_row,
        error::{DbError, DbResult},
        keys::{
            ObjectId, TableId, key_range_bounds, prefix_bounds, row_bits, table_bounds, tree_key,
            value_range_prefixes,
        },
    },
    planner::{
        catalog::{Catalog, IndexDef, TableSchema},
        physical::{KeyRange, ValueRange},
        types::DataType,
        value::Row,
    },
    storage::engine::bplus_tree::{BplusTree, Key},
};

#[derive(Clone, Debug, Default)]
pub struct CatalogState {
    pub catalog: Catalog,
    pub table_ids: HashMap<String, TableId>,
    pub index_ids: HashMap<String, ObjectId>,
    pub next_table_id: TableId,
}

impl CatalogState {
    pub fn table(&self, name: &str) -> Option<(TableId, &TableSchema)> {
        let id = *self.table_ids.get(&name.to_lowercase())?;
        let schema = self.catalog.get_table(name).ok()?;
        Some((id, schema))
    }

    pub fn index_id(&self, name: &str) -> Option<ObjectId> {
        self.index_ids.get(&name.to_lowercase()).copied()
    }
}

pub fn column_types(schema: &TableSchema) -> Vec<DataType> {
    (0..schema.columns.len())
        .map(|i| schema.column_type(i))
        .collect()
}

pub struct StorageSource<'a> {
    pub tree: &'a BplusTree,
    pub state: &'a CatalogState,
    pub txn: u64,
}

impl<'a> StorageSource<'a> {
    pub fn keyed_rows(
        &self,
        types: &[DataType],
        bounds: (Bound<Key>, Bound<Key>),
    ) -> DbResult<Vec<(Key, Row)>> {
        self.tree
            .range(self.txn, bounds.0, bounds.1)
            .map(|entry| {
                let (key, records) = entry.map_err(DbError::storage)?;
                Ok((key, decode_records(types, &records)?))
            })
            .collect()
    }

    pub fn fetch_row(&self, types: &[DataType], key: Key) -> DbResult<Option<Row>> {
        Ok(self
            .keyed_rows(types, (Bound::Included(key), Bound::Included(key)))?
            .pop()
            .map(|(_, row)| row))
    }

    pub fn indexed_row_keys(
        &self,
        table: TableId,
        index: ObjectId,
        index_types: &[DataType],
        ranges: &[ValueRange],
    ) -> DbResult<Vec<Key>> {
        let mut rows = BTreeSet::new();
        for range in ranges {
            let Some((low, high)) = value_range_prefixes(index_types[0], range) else {
                continue;
            };
            let (from, to) = prefix_bounds(index, low, high);
            for entry in self.tree.range(self.txn, from, to) {
                let (key, records) = entry.map_err(DbError::storage)?;
                let values = decode_records(index_types, &records)?;
                if range.contains(&values[0]) {
                    rows.insert(row_bits(key));
                }
            }
        }
        Ok(rows.into_iter().map(|bits| tree_key(table, bits)).collect())
    }

    pub fn looked_up_rows(
        &self,
        table: TableId,
        types: &[DataType],
        index: &IndexDef,
        index_id: ObjectId,
        ranges: &[ValueRange],
    ) -> DbResult<Vec<(Key, Row)>> {
        let index_types: Vec<DataType> = index.columns.iter().map(|&c| types[c]).collect();
        let mut rows = Vec::new();
        for key in self.indexed_row_keys(table, index_id, &index_types, ranges)? {
            match self.fetch_row(types, key)? {
                Some(row) => rows.push((key, row)),
                None => {
                    return Err(DbError::Corrupt(format!(
                        "index {} points at row {key:#x}, which does not exist",
                        index.name
                    )));
                }
            }
        }
        Ok(rows)
    }

    fn stream(&self, table: &str, range: Option<&KeyRange>) -> ExecResult<RowStream<'a>> {
        let (id, schema) = self
            .state
            .table(table)
            .ok_or_else(|| ExecError::TableNotFound(table.to_string()))?;
        let bounds = match range {
            None => table_bounds(id),
            Some(range) => match key_range_bounds(id, range) {
                Some(bounds) => bounds,
                None => return Ok(Box::new(std::iter::empty())),
            },
        };
        let types = column_types(schema);
        let rows = self
            .tree
            .range(self.txn, bounds.0, bounds.1)
            .map(move |entry| {
                let (_, records) = entry.map_err(|e| ExecError::Storage(e.to_string()))?;
                decode_records(&types, &records).map_err(|e| ExecError::Storage(e.to_string()))
            });
        Ok(Box::new(rows))
    }
}

pub fn decode_records(types: &[DataType], records: &[Vec<u8>]) -> DbResult<Row> {
    match records {
        [single] => decode_row(types, single),
        other => Err(DbError::Corrupt(format!(
            "expected one record per row, found {}",
            other.len()
        ))),
    }
}

impl TableSource for StorageSource<'_> {
    fn scan(&self, table: &str) -> ExecResult<RowStream<'_>> {
        self.stream(table, None)
    }

    fn index_scan(
        &self,
        table: &str,
        _key_column: usize,
        range: &KeyRange,
    ) -> ExecResult<RowStream<'_>> {
        self.stream(table, Some(range))
    }

    fn index_lookup(
        &self,
        table: &str,
        index: &str,
        _column: usize,
        ranges: &[ValueRange],
    ) -> ExecResult<RowStream<'_>> {
        let (table_id, schema) = self
            .state
            .table(table)
            .ok_or_else(|| ExecError::TableNotFound(table.to_string()))?;
        let def = self
            .state
            .catalog
            .get_index(index)
            .map_err(|e| ExecError::Storage(e.to_string()))?;
        let index_id = self
            .state
            .index_id(index)
            .ok_or_else(|| ExecError::Storage(format!("index {index} has no storage id")))?;
        let types = column_types(schema);
        let rows = self
            .looked_up_rows(table_id, &types, def, index_id, ranges)
            .map_err(|e| ExecError::Storage(e.to_string()))?;
        Ok(Box::new(rows.into_iter().map(|(_, row)| Ok(row))))
    }
}

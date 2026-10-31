use std::collections::HashMap;

use crate::{
    executor::{
        codec::decode_row,
        error::{DbError, DbResult},
        keys::{ObjectId, TableId},
    },
    planner::{
        catalog::{Catalog, TableSchema},
        types::DataType,
        value::Row,
    },
    storage::engine::bplus_tree::BplusTree,
};

#[derive(Clone, Debug)]
pub struct CatalogState {
    pub catalog: Catalog,
    pub table_ids: HashMap<String, TableId>,
    pub index_ids: HashMap<String, ObjectId>,
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

pub fn decode_records(types: &[DataType], records: &[Vec<u8>]) -> DbResult<Row> {
    match records {
        [single] => decode_row(types, single),
        other => Err(DbError::Corrupt(format!(
            "expected one record per row, found {}",
            other.len()
        ))),
    }
}

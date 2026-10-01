use std::collections::BTreeMap;

use crate::{
    planner::{
        error::{PlanError, PlanResult},
        types::DataType,
    },
    sql::{ColumnConstraint, ColumnDef, CreateTable},
};

pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnDef>,

    // NOTE:  This is a `Vec<usize>` because all our consumer will want a position
    // the executor usually extracts the key from a row by index, and the
    // physical planner compares a key column with a SCAN's projection list of
    // indices, names would be looked up over and over, case insensitively, with
    // the chance of a typo that is only found at runtime, so we resolve once,
    // when the table is created, and fail early if a key names a missing column
    pub primary_key: Vec<usize>,
    pub unique_keys: Vec<Vec<usize>>,
}

impl TableSchema {
    pub fn new(name: impl Into<String>, columns: Vec<ColumnDef>) -> PlanResult<Self> {
        let name = name.into();
        for (i, column) in columns.iter().enumerate() {
            if columns[..i]
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(&column.name))
            {
                return Err(PlanError::DuplicateColumn(column.name.clone()));
            }
        }

        let primary_key: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.constraints.contains(&ColumnConstraint::PrimaryKey))
            .map(|(i, _)| i)
            .collect();
        let unique_keys = columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.constraints.contains(&ColumnConstraint::Unique))
            .map(|(i, _)| vec![i])
            .collect();

        Ok(Self {
            name,
            columns,
            primary_key,
            unique_keys,
        })
    }

    pub fn from_create(create: &CreateTable) -> PlanResult<Self> {
        Self::new(create.name.clone(), create.columns.clone())?
            .with_keys(&create.primary_key, &create.unique_keys)
    }

    pub fn with_keys(
        mut self,
        primary_key: &[String],
        unique_keys: &[Vec<String>],
    ) -> PlanResult<Self> {
        self.primary_key = self.resolve_key(primary_key)?;
        self.unique_keys = unique_keys
            .iter()
            .map(|k| self.resolve_key(k))
            .collect::<PlanResult<Vec<_>>>()?;
        Ok(self)
    }

    pub fn resolve_key(&self, names: &[String]) -> PlanResult<Vec<usize>> {
        names
            .iter()
            .map(|n| {
                self.column_index(n)
                    .ok_or_else(|| PlanError::ColumnNotFound {
                        table: Some(self.name.clone()),
                        column: n.clone(),
                    })
            })
            .collect()
    }

    pub fn find_column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| c.name.as_str()).collect()
    }

    pub fn column_type(&self, index: usize) -> DataType {
        DataType::from(&self.columns[index].data_type)
    }

    pub fn is_nullable(&self, index: usize) -> bool {
        !self.columns[index].is_not_null() && !self.primary_key.contains(&index)
    }

    pub fn integer_key_column(&self) -> Option<usize> {
        match self.primary_key.as_slice() {
            [k] if self.column_type(*k) == DataType::Integer => Some(*k),
            _ => None,
        }
    }
}

pub struct Catalog {
    tables: BTreeMap<String, TableSchema>,
}

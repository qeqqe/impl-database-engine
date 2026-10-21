use std::collections::BTreeMap;

use crate::{
    planner::{
        error::{PlanError, PlanResult},
        types::DataType,
    },
    sql::{CheckConstraint, ColumnConstraint, ColumnDef, CreateTable},
};

#[derive(Debug, PartialEq, Clone)]
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
    pub checks: Vec<CheckConstraint>,
}

impl TableSchema {
    pub fn new(name: impl Into<String>, mut columns: Vec<ColumnDef>) -> PlanResult<Self> {
        let name = name.into();
        let checks = take_column_checks(&mut columns);
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

        Self {
            name,
            columns,
            primary_key,
            unique_keys,
            checks: Vec::new(),
        }
        .with_checks(checks)
    }

    pub fn from_create(create: &CreateTable) -> PlanResult<Self> {
        Self::new(create.name.clone(), create.columns.clone())?
            .with_keys(&create.primary_key, &create.unique_keys)?
            .with_checks(create.checks.clone())
    }

    pub fn with_checks(mut self, checks: Vec<CheckConstraint>) -> PlanResult<Self> {
        for check in checks {
            if let Some(name) = &check.name
                && self.checks.iter().any(|c| {
                    c.name
                        .as_ref()
                        .is_some_and(|other| other.eq_ignore_ascii_case(name))
                })
            {
                return Err(PlanError::DuplicateConstraint(name.clone()));
            }
            self.checks.push(check);
        }
        Ok(self)
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

    fn resolve_key(&self, names: &[String]) -> PlanResult<Vec<usize>> {
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

    pub fn with_column_added(&self, mut column: ColumnDef) -> PlanResult<Self> {
        if self.column_index(&column.name).is_some() {
            return Err(PlanError::DuplicateColumn(column.name));
        }
        let checks = take_column_checks(std::slice::from_mut(&mut column));
        let mut next = self.clone();
        next.columns.push(column);
        next.with_checks(checks)
    }

    pub fn with_column_dropped(&self, column: usize) -> Self {
        let shift = |key: &Vec<usize>| key.iter().map(|&c| shift_past(c, column)).collect();
        let mut columns = self.columns.clone();
        columns.remove(column);
        Self {
            name: self.name.clone(),
            columns,
            primary_key: shift(&self.primary_key),
            unique_keys: self.unique_keys.iter().map(shift).collect(),
            checks: self.checks.clone(),
        }
    }

    pub fn with_column_renamed(&self, column: usize, name: &str) -> PlanResult<Self> {
        if self.column_index(name).is_some_and(|other| other != column) {
            return Err(PlanError::DuplicateColumn(name.to_string()));
        }
        let mut next = self.clone();
        let old = std::mem::replace(&mut next.columns[column].name, name.to_string());
        let own_table = self.name.clone();
        next.visit_check_columns(|table, column| {
            let ours = table
                .as_ref()
                .is_none_or(|t| t.eq_ignore_ascii_case(&own_table));
            if ours && column.eq_ignore_ascii_case(&old) {
                *column = name.to_string();
            }
        });
        Ok(next)
    }

    pub fn renamed(&self, name: &str) -> Self {
        let mut next = Self {
            name: name.to_string(),
            ..self.clone()
        };
        next.visit_check_columns(|table, _| {
            if let Some(t) = table
                && t.eq_ignore_ascii_case(&self.name)
            {
                *t = name.to_string();
            }
        });
        next
    }

    pub fn check_using(&self, column: usize) -> Option<&CheckConstraint> {
        let target = &self.columns[column].name;
        self.checks.iter().find(|check| {
            let mut uses = false;
            check.expr.clone().visit_columns_mut(&mut |table, name| {
                let ours = table
                    .as_ref()
                    .is_none_or(|t| t.eq_ignore_ascii_case(&self.name));
                uses |= ours && name.eq_ignore_ascii_case(target);
            });
            uses
        })
    }

    fn visit_check_columns<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Option<String>, &mut String),
    {
        for check in &mut self.checks {
            check.expr.visit_columns_mut(&mut f);
        }
    }

    pub fn automatic_indexes(&self) -> Vec<IndexDef> {
        let mut keys = Vec::new();
        if self.integer_key_column().is_none() && !self.primary_key.is_empty() {
            keys.push(self.primary_key.clone());
        }
        for key in &self.unique_keys {
            if !keys.contains(key) && self.integer_key_column().is_none_or(|pk| key != &[pk]) {
                keys.push(key.clone());
            }
        }
        keys.into_iter()
            .enumerate()
            .map(|(i, columns)| IndexDef {
                name: format!("{AUTO_INDEX_PREFIX}{}_{}", self.name, i + 1),
                table: self.name.clone(),
                columns,
                unique: true,
            })
            .collect()
    }
}

pub const AUTO_INDEX_PREFIX: &str = "autoindex_";

#[derive(Debug, Clone, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub table: String,
    pub columns: Vec<usize>,
    pub unique: bool,
}

impl IndexDef {
    pub fn is_automatic(&self) -> bool {
        is_automatic_index_name(&self.name)
    }

    pub fn with_column_dropped(&self, column: usize) -> Self {
        Self {
            columns: self
                .columns
                .iter()
                .map(|&c| shift_past(c, column))
                .collect(),
            ..self.clone()
        }
    }

    pub fn for_table_named(&self, table: &str) -> Self {
        let own_prefix = format!("{AUTO_INDEX_PREFIX}{}_", self.table);
        let name = match self.name.strip_prefix(&own_prefix) {
            Some(rest) => format!("{AUTO_INDEX_PREFIX}{table}_{rest}"),
            None => self.name.clone(),
        };
        Self {
            name,
            table: table.to_string(),
            ..self.clone()
        }
    }
}

fn take_column_checks(columns: &mut [ColumnDef]) -> Vec<CheckConstraint> {
    let mut checks = Vec::new();
    for column in columns {
        column.constraints.retain(|constraint| match constraint {
            ColumnConstraint::Check(check) => {
                checks.push(check.clone());
                false
            }
            _ => true,
        });
    }
    checks
}

fn shift_past(column: usize, dropped: usize) -> usize {
    if column > dropped { column - 1 } else { column }
}

pub fn is_automatic_index_name(name: &str) -> bool {
    name.get(..AUTO_INDEX_PREFIX.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(AUTO_INDEX_PREFIX))
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    tables: BTreeMap<String, TableSchema>,
    indexes: BTreeMap<String, IndexDef>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_table(&mut self, schema: TableSchema) -> PlanResult<()> {
        let key = schema.name.to_lowercase();
        if self.tables.contains_key(&key) {
            return Err(PlanError::TableAlreadyExists(schema.name));
        }
        self.tables.insert(key, schema);
        Ok(())
    }

    pub fn get_table(&self, name: &str) -> PlanResult<&TableSchema> {
        self.tables
            .get(&name.to_lowercase())
            .ok_or_else(|| PlanError::TableNotFound(name.to_string()))
    }

    pub fn has_table(&self, name: &str) -> bool {
        self.tables.contains_key(&name.to_lowercase())
    }

    pub fn drop_table(&mut self, name: &str) -> bool {
        self.indexes
            .retain(|_, index| !index.table.eq_ignore_ascii_case(name));
        self.tables.remove(&name.to_lowercase()).is_some()
    }

    pub fn create_index(&mut self, index: IndexDef) -> PlanResult<()> {
        let key = index.name.to_lowercase();
        if self.indexes.contains_key(&key) {
            return Err(PlanError::IndexAlreadyExists(index.name));
        }
        let table = self.get_table(&index.table)?;
        if index.columns.is_empty() || index.columns.iter().any(|&c| c >= table.columns.len()) {
            return Err(PlanError::Internal(format!(
                "index {} names a column outside table {}",
                index.name, index.table
            )));
        }
        self.indexes.insert(key, index);
        Ok(())
    }

    pub fn get_index(&self, name: &str) -> PlanResult<&IndexDef> {
        self.indexes
            .get(&name.to_lowercase())
            .ok_or_else(|| PlanError::IndexNotFound(name.to_string()))
    }

    pub fn has_index(&self, name: &str) -> bool {
        self.indexes.contains_key(&name.to_lowercase())
    }

    pub fn drop_index(&mut self, name: &str) -> Option<IndexDef> {
        self.indexes.remove(&name.to_lowercase())
    }

    pub fn indexes_on(&self, table: &str) -> Vec<&IndexDef> {
        self.indexes
            .values()
            .filter(|index| index.table.eq_ignore_ascii_case(table))
            .collect()
    }

    pub fn table_names(&self) -> Vec<&str> {
        self.tables.values().map(|t| t.name.as_str()).collect()
    }
}

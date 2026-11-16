use crate::{
    planner::catalog::{IndexDef, TableSchema},
    sql::{CheckConstraint, ColumnConstraint, CreateIndex, Parser, SqlDataType, Statement},
};

use super::error::{DbError, DbResult};

pub fn render_create_table(schema: &TableSchema) -> String {
    let mut parts: Vec<String> = schema
        .columns
        .iter()
        .map(|column| {
            let mut def = format!("{} {}", quote(&column.name), type_name(&column.data_type));
            for constraint in &column.constraints {
                match constraint {
                    ColumnConstraint::NotNull => def.push_str(" NOT NULL"),
                    ColumnConstraint::Default(expr) => def.push_str(&format!(" DEFAULT {expr}")),
                    ColumnConstraint::PrimaryKey
                    | ColumnConstraint::Unique
                    | ColumnConstraint::Check(_) => {}
                }
            }
            def
        })
        .collect();

    let names = |indices: &[usize]| {
        indices
            .iter()
            .map(|&i| quote(&schema.columns[i].name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !schema.primary_key.is_empty() {
        parts.push(format!("PRIMARY KEY ({})", names(&schema.primary_key)));
    }
    for key in &schema.unique_keys {
        parts.push(format!("UNIQUE ({})", names(key)));
    }
    for check in &schema.checks {
        parts.push(render_check(check));
    }

    format!(
        "CREATE TABLE {} ({})",
        quote(&schema.name),
        parts.join(", ")
    )
}

pub fn parse_create_table(sql: &str) -> DbResult<TableSchema> {
    match Parser::parse(sql) {
        Ok(Statement::CreateTable(create)) => Ok(TableSchema::from_create(&create)?),
        Ok(other) => Err(DbError::Corrupt(format!(
            "catalog entry is not CREATE TABLE: {other:?}"
        ))),
        Err(e) => Err(DbError::Corrupt(format!(
            "catalog entry does not parse ({e}): {sql}"
        ))),
    }
}

pub enum CatalogEntry {
    Table(TableSchema),
    Index(CreateIndex),
}

pub fn render_create_index(index: &IndexDef, schema: &TableSchema) -> String {
    let columns = index
        .columns
        .iter()
        .map(|&c| quote(&schema.columns[c].name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE {}INDEX {} ON {} ({columns})",
        if index.unique { "UNIQUE " } else { "" },
        quote(&index.name),
        quote(&schema.name)
    )
}

pub fn parse_catalog_entry(sql: &str) -> DbResult<CatalogEntry> {
    match Parser::parse(sql) {
        Ok(Statement::CreateTable(create)) => {
            Ok(CatalogEntry::Table(TableSchema::from_create(&create)?))
        }
        Ok(Statement::CreateIndex(create)) => Ok(CatalogEntry::Index(create)),
        Ok(other) => Err(DbError::Corrupt(format!(
            "catalog entry is not CREATE TABLE or CREATE INDEX: {other:?}"
        ))),
        Err(e) => Err(DbError::Corrupt(format!(
            "catalog entry does not parse ({e}): {sql}"
        ))),
    }
}

pub fn resolve_index(create: &CreateIndex, schema: &TableSchema) -> DbResult<IndexDef> {
    let columns = create
        .columns
        .iter()
        .map(|name| {
            schema.column_index(name).ok_or_else(|| {
                DbError::Corrupt(format!(
                    "index {} names column {name}, which table {} does not have",
                    create.name, schema.name
                ))
            })
        })
        .collect::<DbResult<Vec<_>>>()?;
    Ok(IndexDef {
        name: create.name.clone(),
        table: schema.name.clone(),
        columns,
        unique: create.unique,
    })
}

fn render_check(check: &CheckConstraint) -> String {
    let mut expr = check.expr.clone();
    expr.visit_columns_mut(&mut |table, name| {
        if let Some(table) = table {
            *table = quote(table);
        }
        *name = quote(name);
    });
    match &check.name {
        Some(name) => format!("CONSTRAINT {} CHECK ({expr})", quote(name)),
        None => format!("CHECK ({expr})"),
    }
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn type_name(data_type: &SqlDataType) -> &'static str {
    match data_type {
        SqlDataType::Text => "TEXT",
        SqlDataType::Integer => "INTEGER",
        SqlDataType::Float => "FLOAT",
        SqlDataType::Boolean => "BOOLEAN",
        SqlDataType::Json => "JSON",
        SqlDataType::Timestamp => "TIMESTAMP",
        SqlDataType::Uuid => "UUID",
    }
}

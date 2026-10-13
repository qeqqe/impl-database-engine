use crate::{
    planner::catalog::{Catalog, TableSchema},
    sql::{Parser, Statement},
};

pub fn catalog() -> Catalog {
    let mut catalog = Catalog::new();
    for ddl in [
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, city TEXT DEFAULT 'nowhere')",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, amount FLOAT)",
        "CREATE TABLE tags (name TEXT PRIMARY KEY, weight FLOAT)",
    ] {
        let Statement::CreateTable(create) = Parser::parse(ddl).unwrap() else {
            unreachable!()
        };
        let schema = TableSchema::from_create(&create).unwrap();
        catalog.create_table(schema).unwrap();
    }
    catalog
}

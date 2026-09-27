use sqlparser::ast as sp;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser as SqlParser;

use super::ast::*;
use super::error::{ParseError, ParseResult};

pub struct Parser;

impl Parser {
    pub fn parse(sql: &str) -> ParseResult<Statement> {
        let sql = sql.trim();
        if sql.is_empty() {
            return Err(ParseError::EmptyQuery);
        }

        // special commands not supported by sqlparser
        let trimmed_sql = sql.trim_end_matches(';').trim();
        let upper = trimmed_sql.to_uppercase();
        if upper == "BEGIN" || upper == "BEGIN TRANSACTION" || upper == "START TRANSACTION" {
            return Ok(Statement::Begin);
        }
        if upper == "COMMIT" {
            return Ok(Statement::Commit);
        }
        if upper == "ROLLBACK" {
            return Ok(Statement::Rollback);
        }
        if upper == "SHOW TABLES" || upper == "SHOW TABLE" {
            return Ok(Statement::ShowTables);
        }
        if upper.starts_with("DESCRIBE ") || upper.starts_with("DESC ") {
            let table = trimmed_sql
                .split_whitespace()
                .nth(1)
                .ok_or_else(|| ParseError::MissingClause("table name".into()))?;
            return Ok(Statement::Describe(table.to_string()));
        }

        let dialect = GenericDialect {};
        let statements = SqlParser::parse_sql(&dialect, sql)?;

        if statements.is_empty() {
            return Err(ParseError::EmptyQuery);
        }
        if statements.len() > 1 {
            return Err(ParseError::MultipleStatements);
        }

        Self::convert_statement(&statements[0])
    }

    /// parse multiple SQL statements
    pub fn parse_multi(sql: &str) -> ParseResult<Vec<Statement>> {
        let dialect = GenericDialect {};
        let statements = SqlParser::parse_sql(&dialect, sql)?;
        statements.iter().map(Self::convert_statement).collect()
    }

    fn convert_statement(stmt: &sp::Statement) -> ParseResult<Statement> {
        match stmt {
            sp::Statement::CreateTable(create) => Self::convert_create_table(create),
            sp::Statement::Drop {
                object_type,
                names,
                if_exists,
                ..
            } => Self::convert_drop(object_type, names, *if_exists),
            sp::Statement::Query(query) => Self::convert_query(query).map(Statement::Select),
            sp::Statement::Insert(insert) => Self::convert_insert(insert),
            sp::Statement::Update(update) => Self::convert_update(update),
            sp::Statement::Delete(delete) => Self::convert_delete(delete),
            sp::Statement::StartTransaction { .. } => Ok(Statement::Begin),
            sp::Statement::Commit { .. } => Ok(Statement::Commit),
            sp::Statement::Rollback { .. } => Ok(Statement::Rollback),
            sp::Statement::ShowTables { .. } => Ok(Statement::ShowTables),
            sp::Statement::ExplainTable { table_name, .. } => {
                let name = Self::extract_table_name(table_name)?;
                Ok(Statement::Describe(name))
            }
            other => Err(ParseError::UnsupportedStatement(format!("{:?}", other))),
        }
    }

    fn convert_create_table(create: &sp::CreateTable) -> ParseResult<Statement> {
        if create.query.is_some() {
            return Err(ParseError::UnsupportedStatement(
                "CREATE TABLE ... AS SELECT not supported".into(),
            ));
        }
        if create.like.is_some() || create.clone.is_some() {
            return Err(ParseError::UnsupportedStatement(
                "CREATE TABLE ... LIKE/CLONE not supported".into(),
            ));
        }
        if create.or_replace || create.temporary {
            return Err(ParseError::UnsupportedStatement(
                "CREATE OR REPLACE / TEMPORARY TABLE not supported".into(),
            ));
        }

        let name = Self::extract_table_name(&create.name)?;
        let mut columns = create
            .columns
            .iter()
            .map(Self::convert_column_def)
            .collect::<ParseResult<Vec<_>>>()?;

        let mut primary_key: Vec<String> = columns
            .iter()
            .filter(|c| c.constraints.contains(&ColumnConstraint::PrimaryKey))
            .map(|c| c.name.clone())
            .collect();
        if primary_key.len() > 1 {
            return Err(ParseError::UnsupportedStatement(
                "multiple column level PRIMARY KEY declarations, use PRIMARY KEY (a, b)".into(),
            ));
        }

        let mut unique_keys: Vec<Vec<String>> = columns
            .iter()
            .filter(|c| c.constraints.contains(&ColumnConstraint::Unique))
            .map(|c| vec![c.name.clone()])
            .collect();

        for constraint in &create.constraints {
            match constraint {
                sp::TableConstraint::PrimaryKey(pk) => {
                    if !primary_key.is_empty() {
                        return Err(ParseError::UnsupportedStatement(
                            "multiple PRIMARY KEY declarations".into(),
                        ));
                    }
                    primary_key = Self::constraint_columns(&pk.columns, &columns)?;
                }
                sp::TableConstraint::Unique(uq) => {
                    let key = Self::constraint_columns(&uq.columns, &columns)?;
                    if !unique_keys.contains(&key) {
                        unique_keys.push(key);
                    }
                }
                other => {
                    return Err(ParseError::UnsupportedStatement(format!(
                        "table constraint not supported: {}",
                        other
                    )));
                }
            }
        }

        for column in columns.iter_mut() {
            let in_pk = primary_key.iter().any(|k| k == &column.name);
            if in_pk && !column.constraints.contains(&ColumnConstraint::PrimaryKey) {
                column.constraints.push(ColumnConstraint::PrimaryKey);
            }
            let single_unique = unique_keys
                .iter()
                .any(|k| k.len() == 1 && k[0] == column.name);
            if single_unique && !column.constraints.contains(&ColumnConstraint::Unique) {
                column.constraints.push(ColumnConstraint::Unique);
            }
        }

        Ok(Statement::CreateTable(CreateTable {
            name,
            columns,
            primary_key,
            unique_keys,
            if_not_exists: create.if_not_exists,
        }))
    }

    fn constraint_columns(
        index_columns: &[sp::IndexColumn],
        columns: &[ColumnDef],
    ) -> ParseResult<Vec<String>> {
        let mut out: Vec<String> = Vec::with_capacity(index_columns.len());
        for col in index_columns {
            let sp::Expr::Identifier(ident) = &col.column.expr else {
                return Err(ParseError::UnsupportedExpression(format!(
                    "constraint column must be a plain column name: {}",
                    col.column.expr
                )));
            };
            let target = columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&ident.value))
                .ok_or_else(|| {
                    ParseError::InvalidIdentifier(format!(
                        "constraint references unknown column {}",
                        ident.value
                    ))
                })?;
            if out.contains(&target.name) {
                return Err(ParseError::InvalidIdentifier(format!(
                    "column {} listed twice in constraint",
                    target.name
                )));
            }
            out.push(target.name.clone());
        }
        Ok(out)
    }

    fn convert_column_def(col: &sp::ColumnDef) -> ParseResult<ColumnDef> {
        let data_type = Self::convert_data_type(&col.data_type)?;
        let mut constraints: Vec<ColumnConstraint> = Vec::new();
        for opt in &col.options {
            if let Some(constraint) = Self::convert_column_option(&opt.option)?
                && !constraints.contains(&constraint)
            {
                constraints.push(constraint);
            }
        }

        Ok(ColumnDef {
            name: col.name.value.clone(),
            data_type,
            constraints,
        })
    }

    fn convert_data_type(dt: &sp::DataType) -> ParseResult<SqlDataType> {
        match dt {
            sp::DataType::Text
            | sp::DataType::Varchar(_)
            | sp::DataType::CharVarying(_)
            | sp::DataType::Character(_)
            | sp::DataType::Char(_)
            | sp::DataType::String(_) => Ok(SqlDataType::Text),

            sp::DataType::Int(_)
            | sp::DataType::Integer(_)
            | sp::DataType::BigInt(_)
            | sp::DataType::SmallInt(_)
            | sp::DataType::TinyInt(_) => Ok(SqlDataType::Integer),

            sp::DataType::Float(_)
            | sp::DataType::Real
            | sp::DataType::Double(_)
            | sp::DataType::DoublePrecision
            | sp::DataType::Decimal(_)
            | sp::DataType::Numeric(_) => Ok(SqlDataType::Float),

            sp::DataType::Boolean | sp::DataType::Bool => Ok(SqlDataType::Boolean),

            sp::DataType::JSON | sp::DataType::JSONB => Ok(SqlDataType::Json),

            sp::DataType::Timestamp(_, _) | sp::DataType::Datetime(_) | sp::DataType::Date => {
                Ok(SqlDataType::Timestamp)
            }

            sp::DataType::Uuid => Ok(SqlDataType::Uuid),

            other => Err(ParseError::UnsupportedDataType(format!("{:?}", other))),
        }
    }

    fn convert_column_option(opt: &sp::ColumnOption) -> ParseResult<Option<ColumnConstraint>> {
        match opt {
            sp::ColumnOption::Null => Ok(None), // Nullable by default
            sp::ColumnOption::NotNull => Ok(Some(ColumnConstraint::NotNull)),
            sp::ColumnOption::Unique(..) => Ok(Some(ColumnConstraint::Unique)),
            sp::ColumnOption::PrimaryKey(..) => Ok(Some(ColumnConstraint::PrimaryKey)),
            sp::ColumnOption::Default(expr) => {
                let e = Self::convert_expr(expr)?;
                Ok(Some(ColumnConstraint::Default(e)))
            }
            sp::ColumnOption::Comment(_) => Ok(None),
            other => Err(ParseError::UnsupportedStatement(format!(
                "column option not supported: {}",
                other
            ))),
        }
    }

    fn convert_drop(
        object_type: &sp::ObjectType,
        names: &[sp::ObjectName],
        if_exists: bool,
    ) -> ParseResult<Statement> {
        match object_type {
            sp::ObjectType::Table => {
                if names.len() != 1 {
                    return Err(ParseError::UnsupportedStatement(
                        "DROP multiple tables not supported".into(),
                    ));
                }
                let name = Self::extract_table_name(&names[0])?;
                Ok(Statement::DropTable(DropTable { name, if_exists }))
            }
            other => Err(ParseError::UnsupportedStatement(format!(
                "DROP {:?} not supported",
                other
            ))),
        }
    }

    fn convert_query(query: &sp::Query) -> ParseResult<Select> {
        if query.with.is_some() {
            return Err(ParseError::UnsupportedStatement(
                "WITH (common table expressions) not supported".into(),
            ));
        }
        if query.fetch.is_some() || !query.locks.is_empty() || query.for_clause.is_some() {
            return Err(ParseError::UnsupportedStatement(
                "FETCH / FOR UPDATE / FOR clauses not supported".into(),
            ));
        }
        if !query.pipe_operators.is_empty() {
            return Err(ParseError::UnsupportedStatement(
                "pipe operators not supported".into(),
            ));
        }

        let select = match query.body.as_ref() {
            sp::SetExpr::Select(s) => s,
            other => {
                return Err(ParseError::UnsupportedStatement(format!(
                    "Unsupported query type: {}",
                    other
                )));
            }
        };

        Self::reject_unsupported_select_clauses(select)?;

        let distinct = match &select.distinct {
            None | Some(sp::Distinct::All) => false,
            Some(sp::Distinct::Distinct) => true,
            Some(other) => {
                return Err(ParseError::UnsupportedStatement(format!(
                    "{} not supported",
                    other
                )));
            }
        };

        let from = Self::convert_from(&select.from)?;

        // SELECT columns
        let columns = Self::convert_projection(&select.projection)?;

        let where_clause = select
            .selection
            .as_ref()
            .map(Self::convert_expr)
            .transpose()?;

        // GROUP BY
        let group_by = match &select.group_by {
            sp::GroupByExpr::Expressions(exprs, modifiers) => {
                if !modifiers.is_empty() {
                    return Err(ParseError::UnsupportedStatement(
                        "GROUP BY ROLLUP/CUBE/GROUPING SETS not supported".into(),
                    ));
                }
                exprs
                    .iter()
                    .map(Self::convert_expr)
                    .collect::<ParseResult<Vec<_>>>()?
            }
            sp::GroupByExpr::All(_) => {
                return Err(ParseError::UnsupportedStatement(
                    "GROUP BY ALL not supported".into(),
                ));
            }
        };

        // HAVING
        let having = select.having.as_ref().map(Self::convert_expr).transpose()?;

        let order_by = query
            .order_by
            .as_ref()
            .map(Self::extract_order_by_exprs)
            .transpose()?
            .unwrap_or_default();

        let (limit, offset) = match &query.limit_clause {
            Some(sp::LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            }) => {
                if !limit_by.is_empty() {
                    return Err(ParseError::UnsupportedStatement(
                        "LIMIT BY not supported".into(),
                    ));
                }
                (
                    limit
                        .as_ref()
                        .map(|l| Self::expr_to_usize(l, "LIMIT"))
                        .transpose()?,
                    offset
                        .as_ref()
                        .map(|o| Self::expr_to_usize(&o.value, "OFFSET"))
                        .transpose()?,
                )
            }
            Some(sp::LimitClause::OffsetCommaLimit { offset, limit }) => (
                Some(Self::expr_to_usize(limit, "LIMIT")?),
                Some(Self::expr_to_usize(offset, "OFFSET")?),
            ),
            None => (None, None),
        };

        Ok(Select {
            distinct,
            columns,
            from,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    fn reject_unsupported_select_clauses(select: &sp::Select) -> ParseResult<()> {
        let unsupported = [
            (select.top.is_some(), "TOP"),
            (select.into.is_some(), "SELECT INTO"),
            (!select.lateral_views.is_empty(), "LATERAL VIEW"),
            (select.prewhere.is_some(), "PREWHERE"),
            (!select.connect_by.is_empty(), "CONNECT BY"),
            (!select.cluster_by.is_empty(), "CLUSTER BY"),
            (!select.distribute_by.is_empty(), "DISTRIBUTE BY"),
            (!select.sort_by.is_empty(), "SORT BY"),
            (!select.named_window.is_empty(), "WINDOW"),
            (select.qualify.is_some(), "QUALIFY"),
            (select.exclude.is_some(), "EXCLUDE"),
        ];
        match unsupported.iter().find(|(present, _)| *present) {
            Some((_, clause)) => Err(ParseError::UnsupportedStatement(format!(
                "{} not supported",
                clause
            ))),
            None => Ok(()),
        }
    }

    fn convert_from(from: &[sp::TableWithJoins]) -> ParseResult<TableRef> {
        let Some((first, rest)) = from.split_first() else {
            return Err(ParseError::MissingClause("FROM".into()));
        };

        let mut table_ref = Self::extract_table_ref(first)?;
        for item in rest {
            let (table, alias) = Self::extract_table_factor(&item.relation)?;
            table_ref.joins.push(Join {
                table,
                alias,
                join_type: JoinType::Cross,
                on: None,
            });
            for join in &item.joins {
                table_ref.joins.push(Self::convert_join(join)?);
            }
        }
        Ok(table_ref)
    }

    fn convert_projection(items: &[sp::SelectItem]) -> ParseResult<Vec<SelectColumn>> {
        items
            .iter()
            .map(|item| match item {
                sp::SelectItem::Wildcard(_) => Ok(SelectColumn::Wildcard),
                sp::SelectItem::UnnamedExpr(expr) => match expr {
                    sp::Expr::Identifier(ident) => Ok(SelectColumn::Column {
                        table: None,
                        name: ident.value.clone(),
                    }),
                    sp::Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                        [table, col] => Ok(SelectColumn::Column {
                            table: Some(table.value.clone()),
                            name: col.value.clone(),
                        }),
                        _ => Err(ParseError::UnsupportedExpression(format!(
                            "Unsupported identifier depth: {:?}",
                            parts
                        ))),
                    },
                    _ => {
                        let e = Self::convert_expr(expr)?;
                        Ok(SelectColumn::Expr {
                            expr: e,
                            alias: None,
                        })
                    }
                },
                sp::SelectItem::ExprWithAlias { expr, alias } => {
                    let e = Self::convert_expr(expr)?;
                    Ok(SelectColumn::Expr {
                        expr: e,
                        alias: Some(alias.value.clone()),
                    })
                }
                sp::SelectItem::ExprWithAliases { expr, aliases } => {
                    if aliases.len() > 1 {
                        return Err(ParseError::UnsupportedExpression(
                            "multiple aliases for a single select item".into(),
                        ));
                    }
                    let e = Self::convert_expr(expr)?;
                    Ok(SelectColumn::Expr {
                        expr: e,
                        alias: aliases.first().map(|a| a.value.clone()),
                    })
                }
                sp::SelectItem::QualifiedWildcard(kind, _) => match kind {
                    sp::SelectItemQualifiedWildcardKind::ObjectName(name) => Ok(
                        SelectColumn::QualifiedWildcard(Self::extract_table_name(name)?),
                    ),
                    other => Err(ParseError::UnsupportedExpression(format!(
                        "Unsupported qualified wildcard: {:?}",
                        other
                    ))),
                },
            })
            .collect()
    }

    fn extract_order_by_exprs(ob: &sp::OrderBy) -> ParseResult<Vec<OrderBy>> {
        if ob.interpolate.is_some() {
            return Err(ParseError::UnsupportedStatement(
                "ORDER BY ... INTERPOLATE not supported".into(),
            ));
        }
        match &ob.kind {
            sp::OrderByKind::All(_) => Err(ParseError::UnsupportedStatement(
                "ORDER BY ALL not supported".into(),
            )),
            sp::OrderByKind::Expressions(exprs) => {
                exprs.iter().map(Self::convert_order_by_expr).collect()
            }
        }
    }

    fn convert_order_by_expr(expr: &sp::OrderByExpr) -> ParseResult<OrderBy> {
        if expr.with_fill.is_some() {
            return Err(ParseError::UnsupportedStatement(
                "ORDER BY ... WITH FILL not supported".into(),
            ));
        }
        let ascending = match &expr.options.sort {
            None | Some(sp::OrderBySort::Asc) => true,
            Some(sp::OrderBySort::Desc) => false,
            Some(other) => {
                return Err(ParseError::UnsupportedStatement(format!(
                    "ORDER BY {:?} not supported",
                    other
                )));
            }
        };
        Ok(OrderBy {
            expr: Self::convert_expr(&expr.expr)?,
            ascending,
            nulls_first: expr.options.nulls_first,
        })
    }

    fn convert_insert(insert: &sp::Insert) -> ParseResult<Statement> {
        let unsupported = [
            (insert.or.is_some(), "INSERT OR ..."),
            (insert.ignore, "INSERT IGNORE"),
            (insert.overwrite, "INSERT OVERWRITE"),
            (insert.replace_into, "REPLACE INTO"),
            (insert.on.is_some(), "ON CONFLICT / ON DUPLICATE KEY"),
            (insert.returning.is_some(), "RETURNING"),
            (insert.output.is_some(), "OUTPUT"),
            (insert.partitioned.is_some(), "PARTITION"),
            (!insert.assignments.is_empty(), "INSERT ... SET"),
            (insert.table_alias.is_some(), "INSERT table alias"),
        ];
        if let Some((_, clause)) = unsupported.iter().find(|(present, _)| *present) {
            return Err(ParseError::UnsupportedStatement(format!(
                "{} not supported",
                clause
            )));
        }

        let table = Self::extract_table_from_object(&insert.table)?;

        let columns = if insert.columns.is_empty() {
            None
        } else {
            Some(
                insert
                    .columns
                    .iter()
                    .map(Self::extract_table_name)
                    .collect::<ParseResult<Vec<_>>>()?,
            )
        };

        let Some(query) = insert.source.as_ref() else {
            return Err(ParseError::MissingClause(
                "INSERT requires VALUES or SELECT".into(),
            ));
        };

        let source = match query.body.as_ref() {
            sp::SetExpr::Values(sp::Values { rows, .. }) => {
                if query.order_by.is_some() || query.limit_clause.is_some() || query.with.is_some()
                {
                    return Err(ParseError::UnsupportedStatement(
                        "ORDER BY / LIMIT / WITH on INSERT ... VALUES not supported".into(),
                    ));
                }
                let rows = rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(Self::convert_expr)
                            .collect::<ParseResult<Vec<_>>>()
                    })
                    .collect::<ParseResult<Vec<_>>>()?;
                InsertSource::Values(rows)
            }
            _ => InsertSource::Select(Box::new(Self::convert_query(query)?)),
        };

        Ok(Statement::Insert(Insert {
            table,
            columns,
            source,
        }))
    }

    fn convert_update(update: &sp::Update) -> ParseResult<Statement> {
        let unsupported = [
            (update.from.is_some(), "UPDATE ... FROM"),
            (update.returning.is_some(), "RETURNING"),
            (update.output.is_some(), "OUTPUT"),
            (update.or.is_some(), "UPDATE OR ..."),
            (!update.order_by.is_empty(), "UPDATE ... ORDER BY"),
            (update.limit.is_some(), "UPDATE ... LIMIT"),
        ];
        if let Some((_, clause)) = unsupported.iter().find(|(present, _)| *present) {
            return Err(ParseError::UnsupportedStatement(format!(
                "{} not supported",
                clause
            )));
        }

        let (table_name, alias) = Self::extract_single_table(&update.table)?;

        let assigns = update
            .assignments
            .iter()
            .map(|a| {
                let column =
                    Self::extract_assignment_target(&a.target, &table_name, alias.as_deref())?;
                let value = Self::convert_expr(&a.value)?;
                Ok(Assignment { column, value })
            })
            .collect::<ParseResult<Vec<_>>>()?;

        let where_clause = update
            .selection
            .as_ref()
            .map(Self::convert_expr)
            .transpose()?;

        Ok(Statement::Update(Update {
            table: table_name,
            alias,
            assignments: assigns,
            where_clause,
        }))
    }

    fn extract_assignment_target(
        target: &sp::AssignmentTarget,
        table: &str,
        alias: Option<&str>,
    ) -> ParseResult<String> {
        let sp::AssignmentTarget::ColumnName(name) = target else {
            return Err(ParseError::UnsupportedExpression(
                "tuple assignment targets not supported".into(),
            ));
        };
        let parts = name
            .0
            .iter()
            .map(|p| {
                p.as_ident()
                    .map(|id| id.value.clone())
                    .ok_or_else(|| ParseError::InvalidIdentifier(p.to_string()))
            })
            .collect::<ParseResult<Vec<_>>>()?;

        match parts.as_slice() {
            [column] => Ok(column.clone()),
            [qualifier, column] => {
                let visible = alias.unwrap_or(table);
                if qualifier.eq_ignore_ascii_case(visible) {
                    Ok(column.clone())
                } else {
                    Err(ParseError::InvalidIdentifier(format!(
                        "assignment target {}.{} does not refer to table {}",
                        qualifier, column, visible
                    )))
                }
            }
            _ => Err(ParseError::InvalidIdentifier(name.to_string())),
        }
    }

    fn convert_delete(delete: &sp::Delete) -> ParseResult<Statement> {
        let unsupported = [
            (!delete.tables.is_empty(), "multi table DELETE"),
            (delete.using.is_some(), "DELETE ... USING"),
            (delete.returning.is_some(), "RETURNING"),
            (delete.output.is_some(), "OUTPUT"),
            (!delete.order_by.is_empty(), "DELETE ... ORDER BY"),
            (delete.limit.is_some(), "DELETE ... LIMIT"),
        ];
        if let Some((_, clause)) = unsupported.iter().find(|(present, _)| *present) {
            return Err(ParseError::UnsupportedStatement(format!(
                "{} not supported",
                clause
            )));
        }

        let tables = match &delete.from {
            sp::FromTable::WithFromKeyword(tables) => tables,
            sp::FromTable::WithoutKeyword(tables) => tables,
        };

        if tables.len() != 1 {
            return Err(ParseError::UnsupportedStatement(
                "DELETE from multiple tables not supported".into(),
            ));
        }

        let (table, alias) = Self::extract_single_table(&tables[0])?;
        let where_clause = delete
            .selection
            .as_ref()
            .map(Self::convert_expr)
            .transpose()?;

        Ok(Statement::Delete(Delete {
            table,
            alias,
            where_clause,
        }))
    }

    fn convert_expr(expr: &sp::Expr) -> ParseResult<Expr> {
        match expr {
            sp::Expr::Identifier(id) => Ok(Expr::Column {
                table: None,
                name: id.value.clone(),
            }),

            sp::Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                [table, col] => Ok(Expr::Column {
                    table: Some(table.value.clone()),
                    name: col.value.clone(),
                }),
                _ => Err(ParseError::UnsupportedExpression(format!(
                    "Unsupported identifier depth: {:?}",
                    parts
                ))),
            },

            sp::Expr::Value(v) => Ok(Expr::Literal(Self::convert_value(v)?)),

            sp::Expr::BinaryOp { left, op, right } => {
                let l = Self::convert_expr(left)?;
                let r = Self::convert_expr(right)?;
                let o = Self::convert_binary_op(op)?;
                Ok(Expr::BinaryOp {
                    left: Box::new(l),
                    op: o,
                    right: Box::new(r),
                })
            }

            sp::Expr::UnaryOp { op, expr } => {
                let e = Self::convert_expr(expr)?;
                let o = Self::convert_unary_op(op)?;
                Ok(Expr::UnaryOp {
                    op: o,
                    expr: Box::new(e),
                })
            }

            sp::Expr::IsNull(e) => {
                let inner = Self::convert_expr(e)?;
                Ok(Expr::IsNull {
                    expr: Box::new(inner),
                    negated: false,
                })
            }

            sp::Expr::IsNotNull(e) => {
                let inner = Self::convert_expr(e)?;
                Ok(Expr::IsNull {
                    expr: Box::new(inner),
                    negated: true,
                })
            }

            sp::Expr::InList {
                expr,
                list,
                negated,
            } => {
                let e = Self::convert_expr(expr)?;
                let items = list
                    .iter()
                    .map(Self::convert_expr)
                    .collect::<ParseResult<Vec<_>>>()?;
                Ok(Expr::InList {
                    expr: Box::new(e),
                    list: items,
                    negated: *negated,
                })
            }

            sp::Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let e = Self::convert_expr(expr)?;
                let l = Self::convert_expr(low)?;
                let h = Self::convert_expr(high)?;
                Ok(Expr::Between {
                    expr: Box::new(e),
                    low: Box::new(l),
                    high: Box::new(h),
                    negated: *negated,
                })
            }

            sp::Expr::Like {
                expr,
                pattern,
                negated,
                any,
                escape_char,
            } => Self::convert_like(expr, pattern, *negated, *any, escape_char.is_some(), false),

            sp::Expr::ILike {
                expr,
                pattern,
                negated,
                any,
                escape_char,
            } => Self::convert_like(expr, pattern, *negated, *any, escape_char.is_some(), true),

            sp::Expr::Function(f) => Self::convert_function(f),

            sp::Expr::Nested(inner) => {
                let e = Self::convert_expr(inner)?;
                Ok(Expr::Nested(Box::new(e)))
            }

            other => Err(ParseError::UnsupportedExpression(format!("{:?}", other))),
        }
    }

    fn convert_like(
        expr: &sp::Expr,
        pattern: &sp::Expr,
        negated: bool,
        any: bool,
        has_escape: bool,
        case_insensitive: bool,
    ) -> ParseResult<Expr> {
        if any {
            return Err(ParseError::UnsupportedExpression(
                "LIKE ANY not supported".into(),
            ));
        }
        if has_escape {
            return Err(ParseError::UnsupportedExpression(
                "LIKE ... ESCAPE not supported".into(),
            ));
        }
        Ok(Expr::Like {
            expr: Box::new(Self::convert_expr(expr)?),
            pattern: Self::extract_string_from_expr(pattern)?,
            negated,
            case_insensitive,
        })
    }

    fn convert_function(f: &sp::Function) -> ParseResult<Expr> {
        let name = Self::extract_table_name(&f.name)?;
        if f.over.is_some() {
            return Err(ParseError::UnsupportedExpression(format!(
                "window function {} not supported",
                name
            )));
        }
        if f.filter.is_some() || !f.within_group.is_empty() || f.null_treatment.is_some() {
            return Err(ParseError::UnsupportedExpression(format!(
                "FILTER / WITHIN GROUP / null treatment on {} not supported",
                name
            )));
        }
        if !matches!(f.parameters, sp::FunctionArguments::None) {
            return Err(ParseError::UnsupportedExpression(format!(
                "parametric function {} not supported",
                name
            )));
        }

        let args = match &f.args {
            sp::FunctionArguments::None => FunctionArgs::List {
                args: vec![],
                distinct: false,
            },
            sp::FunctionArguments::Subquery(_) => {
                return Err(ParseError::UnsupportedExpression(
                    "subquery as function argument not supported".into(),
                ));
            }
            sp::FunctionArguments::List(list) => {
                if !list.clauses.is_empty() {
                    return Err(ParseError::UnsupportedExpression(format!(
                        "function argument clauses on {} not supported",
                        name
                    )));
                }
                let distinct = matches!(
                    list.duplicate_treatment,
                    Some(sp::DuplicateTreatment::Distinct)
                );
                match list.args.as_slice() {
                    [sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Wildcard)] if !distinct => {
                        FunctionArgs::Star
                    }
                    args => FunctionArgs::List {
                        args: args
                            .iter()
                            .map(|arg| match arg {
                                sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Expr(e)) => {
                                    Self::convert_expr(e)
                                }
                                other => Err(ParseError::UnsupportedExpression(format!(
                                    "function argument not supported: {}",
                                    other
                                ))),
                            })
                            .collect::<ParseResult<Vec<_>>>()?,
                        distinct,
                    },
                }
            }
        };

        Ok(Expr::Function { name, args })
    }

    fn convert_value(v: &sp::ValueWithSpan) -> ParseResult<LiteralValue> {
        match &v.value {
            sp::Value::Null => Ok(LiteralValue::Null),
            sp::Value::Boolean(b) => Ok(LiteralValue::Boolean(*b)),
            sp::Value::Number(s, _) => {
                if let Ok(i) = s.parse::<i64>() {
                    Ok(LiteralValue::Integer(i))
                } else if let Ok(f) = s.parse::<f64>() {
                    Ok(LiteralValue::Float(f))
                } else {
                    Err(ParseError::UnsupportedExpression(format!(
                        "Invalid number: {}",
                        s
                    )))
                }
            }
            sp::Value::SingleQuotedString(s) => Ok(LiteralValue::String(s.clone())),
            sp::Value::DoubleQuotedString(s) => Ok(LiteralValue::String(s.clone())),
            other => Err(ParseError::UnsupportedExpression(format!(
                "Unsupported value: {:?}",
                other
            ))),
        }
    }

    fn extract_string_from_expr(expr: &sp::Expr) -> ParseResult<String> {
        match expr {
            sp::Expr::Value(v) => match &v.value {
                sp::Value::SingleQuotedString(s) => Ok(s.clone()),
                sp::Value::DoubleQuotedString(s) => Ok(s.clone()),
                _ => Err(ParseError::UnsupportedExpression("expected string".into())),
            },
            _ => Err(ParseError::UnsupportedExpression(
                "expected string literal".into(),
            )),
        }
    }

    fn convert_binary_op(op: &sp::BinaryOperator) -> ParseResult<BinaryOperator> {
        match op {
            sp::BinaryOperator::Eq => Ok(BinaryOperator::Eq),
            sp::BinaryOperator::NotEq => Ok(BinaryOperator::NotEq),
            sp::BinaryOperator::Lt => Ok(BinaryOperator::Lt),
            sp::BinaryOperator::LtEq => Ok(BinaryOperator::LtEq),
            sp::BinaryOperator::Gt => Ok(BinaryOperator::Gt),
            sp::BinaryOperator::GtEq => Ok(BinaryOperator::GtEq),
            sp::BinaryOperator::And => Ok(BinaryOperator::And),
            sp::BinaryOperator::Or => Ok(BinaryOperator::Or),
            sp::BinaryOperator::Plus => Ok(BinaryOperator::Plus),
            sp::BinaryOperator::Minus => Ok(BinaryOperator::Minus),
            sp::BinaryOperator::Multiply => Ok(BinaryOperator::Multiply),
            sp::BinaryOperator::Divide => Ok(BinaryOperator::Divide),
            sp::BinaryOperator::Modulo => Ok(BinaryOperator::Modulo),
            sp::BinaryOperator::StringConcat => Ok(BinaryOperator::Concat),
            other => Err(ParseError::UnsupportedExpression(format!(
                "Unsupported operator: {:?}",
                other
            ))),
        }
    }

    fn convert_unary_op(op: &sp::UnaryOperator) -> ParseResult<UnaryOperator> {
        match op {
            sp::UnaryOperator::Not => Ok(UnaryOperator::Not),
            sp::UnaryOperator::Minus => Ok(UnaryOperator::Minus),
            sp::UnaryOperator::Plus => Ok(UnaryOperator::Plus),
            other => Err(ParseError::UnsupportedExpression(format!(
                "Unsupported unary operator: {:?}",
                other
            ))),
        }
    }

    fn extract_table_name(name: &sp::ObjectName) -> ParseResult<String> {
        // just use the table name, ignore schema
        name.0
            .last()
            .map(|i| {
                i.as_ident()
                    .map(|id| id.value.clone())
                    .unwrap_or_else(|| i.to_string())
            })
            .ok_or_else(|| ParseError::InvalidIdentifier("empty table name".into()))
    }

    fn extract_table_from_object(table: &sp::TableObject) -> ParseResult<String> {
        match table {
            sp::TableObject::TableName(name) => Self::extract_table_name(name),
            sp::TableObject::TableFunction(_) => Err(ParseError::UnsupportedStatement(
                "table function not supported".into(),
            )),
            sp::TableObject::TableQuery(_) => Err(ParseError::UnsupportedStatement(
                "table query not supported".into(),
            )),
        }
    }

    fn extract_table_factor(factor: &sp::TableFactor) -> ParseResult<(String, Option<String>)> {
        match factor {
            sp::TableFactor::Table {
                name, alias, args, ..
            } => {
                if args.is_some() {
                    return Err(ParseError::UnsupportedStatement(
                        "table valued functions not supported".into(),
                    ));
                }
                if let Some(a) = alias
                    && !a.columns.is_empty()
                {
                    return Err(ParseError::UnsupportedStatement(
                        "column aliases in table alias not supported".into(),
                    ));
                }
                let table = Self::extract_table_name(name)?;
                Ok((table, alias.as_ref().map(|a| a.name.value.clone())))
            }
            other => Err(ParseError::UnsupportedStatement(format!(
                "Unsupported table factor: {}",
                other
            ))),
        }
    }

    fn extract_table_ref(from: &sp::TableWithJoins) -> ParseResult<TableRef> {
        let (base, base_alias) = Self::extract_table_factor(&from.relation)?;
        let joins = from
            .joins
            .iter()
            .map(Self::convert_join)
            .collect::<ParseResult<Vec<_>>>()?;
        Ok(TableRef {
            base,
            base_alias,
            joins,
        })
    }

    fn convert_join(join: &sp::Join) -> ParseResult<Join> {
        let (table, alias) = Self::extract_table_factor(&join.relation)?;

        let (join_type, constraint) = match &join.join_operator {
            sp::JoinOperator::Join(c) | sp::JoinOperator::Inner(c) => (JoinType::Inner, c),
            sp::JoinOperator::Left(c) | sp::JoinOperator::LeftOuter(c) => (JoinType::Left, c),
            sp::JoinOperator::Right(c) | sp::JoinOperator::RightOuter(c) => (JoinType::Right, c),
            sp::JoinOperator::FullOuter(c) => (JoinType::Full, c),
            sp::JoinOperator::CrossJoin(c) => (JoinType::Cross, c),
            other => {
                return Err(ParseError::UnsupportedStatement(format!(
                    "Unsupported join type: {:?}",
                    other
                )));
            }
        };

        let on = match constraint {
            sp::JoinConstraint::On(expr) if join_type != JoinType::Cross => {
                Some(Self::convert_expr(expr)?)
            }
            sp::JoinConstraint::On(_) => {
                return Err(ParseError::UnsupportedStatement(
                    "CROSS JOIN does not take an ON condition".into(),
                ));
            }
            sp::JoinConstraint::Using(_) => {
                return Err(ParseError::UnsupportedStatement(
                    "JOIN ... USING not supported, use ON".into(),
                ));
            }
            sp::JoinConstraint::Natural => {
                return Err(ParseError::UnsupportedStatement(
                    "NATURAL JOIN not supported".into(),
                ));
            }
            sp::JoinConstraint::None if join_type == JoinType::Cross => None,
            sp::JoinConstraint::None => {
                return Err(ParseError::MissingClause("JOIN ... ON condition".into()));
            }
        };

        Ok(Join {
            table,
            alias,
            join_type,
            on,
        })
    }

    fn extract_single_table(from: &sp::TableWithJoins) -> ParseResult<(String, Option<String>)> {
        if !from.joins.is_empty() {
            return Err(ParseError::UnsupportedStatement(
                "JOIN not supported in UPDATE/DELETE".into(),
            ));
        }
        Self::extract_table_factor(&from.relation)
    }

    fn expr_to_usize(expr: &sp::Expr, clause: &str) -> ParseResult<usize> {
        let invalid = || {
            ParseError::UnsupportedExpression(format!(
                "{} must be a non negative integer literal, got {}",
                clause, expr
            ))
        };
        match expr {
            sp::Expr::Value(v) => match &v.value {
                sp::Value::Number(s, _) => s.parse().map_err(|_| invalid()),
                _ => Err(invalid()),
            },
            _ => Err(invalid()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_create_table() {
        let sql = "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT NOT NULL, age INTEGER)";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::CreateTable(ct) => {
                assert_eq!(ct.name, "users");
                assert_eq!(ct.columns.len(), 3);
                assert!(ct.if_not_exists);

                assert_eq!(ct.columns[0].name, "id");
                assert!(
                    ct.columns[0]
                        .constraints
                        .contains(&ColumnConstraint::PrimaryKey)
                );

                assert_eq!(ct.columns[1].name, "name");
                assert!(
                    ct.columns[1]
                        .constraints
                        .contains(&ColumnConstraint::NotNull)
                );
            }
            _ => panic!("Expected CreateTable"),
        }
    }

    #[test]
    fn test_parse_drop_table() {
        let sql = "DROP TABLE users";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::DropTable(dt) => {
                assert_eq!(dt.name, "users");
                assert!(!dt.if_exists);
            }
            _ => panic!("Expected DropTable"),
        }
    }

    #[test]
    fn test_parse_select_order_limit() {
        let sql = "SELECT * FROM users ORDER BY name DESC LIMIT 10 OFFSET 5";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.order_by.len(), 1);
                assert_eq!(
                    s.order_by[0].expr,
                    Expr::Column {
                        table: None,
                        name: "name".into(),
                    }
                );
                assert!(!s.order_by[0].ascending);
                assert_eq!(s.limit, Some(10));
                assert_eq!(s.offset, Some(5));
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_parse_complex_where() {
        let sql = "SELECT * FROM users WHERE age >= 18 AND (status = 'active' OR role = 'admin')";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::Select(s) => {
                assert!(s.where_clause.is_some());
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_parse_insert() {
        let sql = "INSERT INTO users (id, name) VALUES ('1', 'Alice')";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::Insert(i) => {
                assert_eq!(i.table, "users");
                assert_eq!(i.columns, Some(vec!["id".into(), "name".into()]));
                match i.source {
                    InsertSource::Values(rows) => {
                        assert_eq!(rows.len(), 1);
                        assert_eq!(rows[0].len(), 2);
                    }
                    other => panic!("Expected VALUES source, got {:?}", other),
                }
            }
            _ => panic!("Expected Insert"),
        }
    }

    #[test]
    fn test_parse_update() {
        let sql = "UPDATE users SET name = 'Bob' WHERE id = '1'";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::Update(u) => {
                assert_eq!(u.table, "users");
                assert_eq!(u.assignments.len(), 1);
                assert_eq!(u.assignments[0].column, "name");
                assert!(u.where_clause.is_some());
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_delete() {
        let sql = "DELETE FROM users WHERE id = '1'";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::Delete(d) => {
                assert_eq!(d.table, "users");
                assert!(d.where_clause.is_some());
            }
            _ => panic!("Expected Delete"),
        }
    }

    #[test]
    fn test_parse_describe() {
        match Parser::parse("DESCRIBE users").unwrap() {
            Statement::Describe(table) => assert_eq!(table, "users"),
            _ => panic!("Expected Describe"),
        }
    }

    #[test]
    fn test_parse_in_list() {
        let sql = "SELECT * FROM users WHERE status IN ('active', 'pending')";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::Select(s) => match s.where_clause {
                Some(Expr::InList { list, negated, .. }) => {
                    assert_eq!(list.len(), 2);
                    assert!(!negated);
                }
                _ => panic!("Expected InList"),
            },
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_parse_like() {
        let sql = "SELECT * FROM users WHERE name LIKE 'A%'";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::Select(s) => match s.where_clause {
                Some(Expr::Like {
                    pattern, negated, ..
                }) => {
                    assert_eq!(pattern, "A%");
                    assert!(!negated);
                }
                _ => panic!("Expected Like"),
            },
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_parse_between() {
        let sql = "SELECT * FROM users WHERE age BETWEEN 18 AND 65";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::Select(s) => {
                assert!(matches!(
                    s.where_clause,
                    Some(Expr::Between { negated: false, .. })
                ));
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_empty_query() {
        assert!(matches!(
            Parser::parse("").unwrap_err(),
            ParseError::EmptyQuery
        ));
        assert!(matches!(
            Parser::parse("   ").unwrap_err(),
            ParseError::EmptyQuery
        ));
    }

    #[test]
    fn test_parse_multi() {
        let sql = "CREATE TABLE t1 (id INT); SELECT * FROM t1";
        let stmts = Parser::parse_multi(sql).unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(matches!(stmts[0], Statement::CreateTable(_)));
        assert!(matches!(stmts[1], Statement::Select(_)));
    }

    #[test]
    fn test_table_level_constraints() {
        let sql = "CREATE TABLE users (id INT, email TEXT, PRIMARY KEY (id), UNIQUE (email))";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::CreateTable(ct) => {
                assert_eq!(ct.name, "users");
                let id_col = ct.columns.iter().find(|c| c.name == "id").unwrap();
                assert!(id_col.constraints.contains(&ColumnConstraint::PrimaryKey));
                let email_col = ct.columns.iter().find(|c| c.name == "email").unwrap();
                assert!(email_col.constraints.contains(&ColumnConstraint::Unique));
            }
            _ => panic!("Expected CreateTable"),
        }
    }

    #[test]
    fn test_qualified_column_in_expr() {
        let sql = "SELECT * FROM users WHERE users.age >= 18";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => match s.where_clause.unwrap() {
                Expr::BinaryOp { left, .. } => {
                    assert_eq!(
                        *left,
                        Expr::Column {
                            table: Some("users".into()),
                            name: "age".into(),
                        }
                    );
                }
                other => panic!("Expected BinaryOp, got {:?}", other),
            },
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_unqualified_column_in_expr() {
        let sql = "SELECT * FROM users WHERE age >= 18";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => match s.where_clause.unwrap() {
                Expr::BinaryOp { left, .. } => {
                    assert_eq!(
                        *left,
                        Expr::Column {
                            table: None,
                            name: "age".into(),
                        }
                    );
                }
                other => panic!("Expected BinaryOp, got {:?}", other),
            },
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_qualified_column_in_projection() {
        let sql = "SELECT a.id, b.name FROM a JOIN b ON a.id = b.a_id";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(
                    s.columns[0],
                    SelectColumn::Column {
                        table: Some("a".into()),
                        name: "id".into(),
                    }
                );
                assert_eq!(
                    s.columns[1],
                    SelectColumn::Column {
                        table: Some("b".into()),
                        name: "name".into(),
                    }
                );
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_unqualified_column_in_projection() {
        let sql = "SELECT id, name FROM users";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(
                    s.columns[0],
                    SelectColumn::Column {
                        table: None,
                        name: "id".into(),
                    }
                );
                assert_eq!(
                    s.columns[1],
                    SelectColumn::Column {
                        table: None,
                        name: "name".into(),
                    }
                );
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_inner_join() {
        let sql = "SELECT * FROM a JOIN b ON a.id = b.a_id";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.from.base, "a");
                assert!(s.from.base_alias.is_none());
                assert_eq!(s.from.joins.len(), 1);
                let j = &s.from.joins[0];
                assert_eq!(j.table, "b");
                assert_eq!(j.join_type, JoinType::Inner);
                match j.on.as_ref().unwrap() {
                    Expr::BinaryOp { left, op, right } => {
                        assert_eq!(
                            **left,
                            Expr::Column {
                                table: Some("a".into()),
                                name: "id".into(),
                            }
                        );
                        assert_eq!(*op, BinaryOperator::Eq);
                        assert_eq!(
                            **right,
                            Expr::Column {
                                table: Some("b".into()),
                                name: "a_id".into(),
                            }
                        );
                    }
                    other => panic!("Expected BinaryOp ON condition, got {:?}", other),
                }
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_left_join() {
        let sql = "SELECT * FROM a LEFT JOIN b ON a.id = b.a_id";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.from.joins.len(), 1);
                assert_eq!(s.from.joins[0].join_type, JoinType::Left);
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_multiple_joins() {
        let sql = "SELECT * FROM a JOIN b ON a.id = b.a_id LEFT JOIN c ON b.id = c.b_id";
        let stmt = Parser::parse(sql).unwrap();

        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.from.base, "a");
                assert_eq!(s.from.joins.len(), 2);
                assert_eq!(s.from.joins[0].table, "b");
                assert_eq!(s.from.joins[0].join_type, JoinType::Inner);
                assert_eq!(s.from.joins[1].table, "c");
                assert_eq!(s.from.joins[1].join_type, JoinType::Left);
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_table_alias() {
        let sql = "SELECT u.name FROM users AS u";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.from.base, "users");
                assert_eq!(s.from.base_alias.as_deref(), Some("u"));
                assert_eq!(
                    s.columns[0],
                    SelectColumn::Column {
                        table: Some("u".into()),
                        name: "name".into(),
                    }
                );
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_join_alias() {
        let sql = "SELECT * FROM a JOIN b AS bb ON a.id = bb.a_id";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.from.joins[0].table, "b");
                assert_eq!(s.from.joins[0].alias.as_deref(), Some("bb"));
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_cross_join() {
        let sql = "SELECT * FROM a CROSS JOIN b";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.from.joins.len(), 1);
                assert_eq!(s.from.joins[0].join_type, JoinType::Cross);
                assert!(s.from.joins[0].on.is_none());
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_group_by() {
        let sql = "SELECT name, COUNT(*) FROM users GROUP BY name";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.group_by.len(), 1);
                assert_eq!(
                    s.group_by[0],
                    Expr::Column {
                        table: None,
                        name: "name".into(),
                    }
                );
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_having() {
        let sql = "SELECT name, COUNT(*) FROM users GROUP BY name HAVING COUNT(*) > 1";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert!(s.having.is_some());
                assert_eq!(s.group_by.len(), 1);
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_no_group_by_produces_empty_vec() {
        let sql = "SELECT * FROM users";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert!(s.group_by.is_empty());
                assert!(s.having.is_none());
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_group_by_all_errors() {
        let sql = "SELECT name FROM users GROUP BY ALL";
        let result = Parser::parse(sql);
        assert!(result.is_err());
        match result.unwrap_err() {
            ParseError::UnsupportedStatement(msg) => {
                assert!(msg.contains("GROUP BY ALL"), "got: {}", msg);
            }
            other => panic!("Expected UnsupportedStatement, got {:?}", other),
        }
    }

    #[test]
    fn test_order_by_all_parsed_as_column() {
        let sql = "SELECT * FROM users ORDER BY ALL";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.order_by.len(), 1);
                // NOTE:`sqlparser::GenericDialect` treats ALL as a column name,
                // not OrderByKind::All The error path in extract_order_by_exprs
                // still guards against OrderByKind::All if a dialect that
                // supports it is ever used "ALL" parsed as a regular column name
                assert_eq!(
                    s.order_by[0].expr,
                    Expr::Column {
                        table: None,
                        name: "ALL".into(),
                    }
                );
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_qualified_wildcard() {
        let sql = "SELECT a.* FROM a JOIN b ON a.id = b.a_id";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.columns[0], SelectColumn::QualifiedWildcard("a".into()));
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_update_rejects_join() {
        let sql = "UPDATE a JOIN b ON a.id = b.a_id SET a.name = 'x'";
        let result = Parser::parse(sql);
        assert!(result.is_err());
        match result.unwrap_err() {
            ParseError::UnsupportedStatement(msg) => {
                assert!(msg.contains("JOIN"), "got: {}", msg);
            }
            other => panic!("Expected UnsupportedStatement, got {:?}", other),
        }
    }

    #[test]
    fn test_join_using_errors() {
        let sql = "SELECT * FROM a JOIN b USING (id)";
        let result = Parser::parse(sql);
        assert!(result.is_err());
        match result.unwrap_err() {
            ParseError::UnsupportedStatement(msg) => {
                assert!(msg.contains("USING"), "got: {}", msg);
            }
            other => panic!("Expected UnsupportedStatement, got {:?}", other),
        }
    }

    #[test]
    fn test_natural_join_errors() {
        let sql = "SELECT * FROM a NATURAL JOIN b";
        let result = Parser::parse(sql);
        assert!(result.is_err());
        match result.unwrap_err() {
            ParseError::UnsupportedStatement(msg) => {
                assert!(msg.contains("NATURAL"), "got: {}", msg);
            }
            other => panic!("Expected UnsupportedStatement, got {:?}", other),
        }
    }

    #[test]
    fn test_full_integration_query() {
        let sql = "SELECT a.id, b.name FROM a JOIN b ON a.id = b.a_id WHERE a.age >= 18 GROUP BY b.name HAVING COUNT(*) > 1";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(
                    s.columns[0],
                    SelectColumn::Column {
                        table: Some("a".into()),
                        name: "id".into(),
                    }
                );
                assert_eq!(
                    s.columns[1],
                    SelectColumn::Column {
                        table: Some("b".into()),
                        name: "name".into(),
                    }
                );

                assert_eq!(s.from.base, "a");
                assert_eq!(s.from.joins.len(), 1);
                assert_eq!(s.from.joins[0].table, "b");
                assert_eq!(s.from.joins[0].join_type, JoinType::Inner);

                match s.from.joins[0].on.as_ref().unwrap() {
                    Expr::BinaryOp { left, op, right } => {
                        assert_eq!(
                            **left,
                            Expr::Column {
                                table: Some("a".into()),
                                name: "id".into(),
                            }
                        );
                        assert_eq!(*op, BinaryOperator::Eq);
                        assert_eq!(
                            **right,
                            Expr::Column {
                                table: Some("b".into()),
                                name: "a_id".into(),
                            }
                        );
                    }
                    other => panic!("Expected BinaryOp, got {:?}", other),
                }

                match s.where_clause.as_ref().unwrap() {
                    Expr::BinaryOp { left, op, right } => {
                        assert_eq!(
                            **left,
                            Expr::Column {
                                table: Some("a".into()),
                                name: "age".into(),
                            }
                        );
                        assert_eq!(*op, BinaryOperator::GtEq);
                        assert_eq!(**right, Expr::Literal(LiteralValue::Integer(18)));
                    }
                    other => panic!("Expected BinaryOp, got {:?}", other),
                }

                assert_eq!(s.group_by.len(), 1);
                assert_eq!(
                    s.group_by[0],
                    Expr::Column {
                        table: Some("b".into()),
                        name: "name".into(),
                    }
                );

                assert!(s.having.is_some());
                match s.having.as_ref().unwrap() {
                    Expr::BinaryOp { left, op, right } => {
                        assert_eq!(*op, BinaryOperator::Gt);
                        match left.as_ref() {
                            Expr::Function { name, .. } => {
                                assert_eq!(name.to_uppercase(), "COUNT");
                            }
                            other => panic!("Expected Function, got {:?}", other),
                        }
                        assert_eq!(**right, Expr::Literal(LiteralValue::Integer(1)));
                    }
                    other => panic!("Expected BinaryOp, got {:?}", other),
                }
            }
            _ => panic!("Expected Select"),
        }
    }

    fn select(sql: &str) -> Select {
        match Parser::parse(sql).unwrap() {
            Statement::Select(s) => s,
            other => panic!("Expected Select, got {:?}", other),
        }
    }

    fn unsupported(sql: &str) -> String {
        match Parser::parse(sql) {
            Err(ParseError::UnsupportedStatement(msg))
            | Err(ParseError::UnsupportedExpression(msg)) => msg,
            other => panic!("Expected unsupported error for {sql}, got {:?}", other),
        }
    }

    #[test]
    fn test_select_distinct() {
        assert!(select("SELECT DISTINCT name FROM users").distinct);
        assert!(!select("SELECT name FROM users").distinct);
        assert!(unsupported("SELECT DISTINCT ON (a) a FROM t").contains("DISTINCT ON"));
    }

    #[test]
    fn test_order_by_keeps_qualifier_and_expressions() {
        let s =
            select("SELECT * FROM a JOIN b ON a.id = b.id ORDER BY b.id DESC NULLS LAST, a.x + 1");
        assert_eq!(
            s.order_by[0],
            OrderBy {
                expr: Expr::Column {
                    table: Some("b".into()),
                    name: "id".into(),
                },
                ascending: false,
                nulls_first: Some(false),
            }
        );
        assert!(matches!(s.order_by[1].expr, Expr::BinaryOp { .. }));
    }

    #[test]
    fn test_limit_must_be_literal() {
        assert!(unsupported("SELECT * FROM t LIMIT x").contains("LIMIT"));
        assert!(Parser::parse("SELECT * FROM t LIMIT -1").is_err());
        let s = select("SELECT * FROM t LIMIT 3, 7");
        assert_eq!((s.limit, s.offset), (Some(7), Some(3)));
    }

    #[test]
    fn test_count_star_and_distinct_arguments() {
        let s = select("SELECT COUNT(*), COUNT(DISTINCT a) FROM t");
        match &s.columns[0] {
            SelectColumn::Expr {
                expr: Expr::Function { args, .. },
                ..
            } => assert_eq!(*args, FunctionArgs::Star),
            other => panic!("Expected COUNT(*), got {:?}", other),
        }
        match &s.columns[1] {
            SelectColumn::Expr {
                expr:
                    Expr::Function {
                        args: FunctionArgs::List { args, distinct },
                        ..
                    },
                ..
            } => {
                assert!(*distinct);
                assert_eq!(args.len(), 1);
            }
            other => panic!("Expected COUNT(DISTINCT a), got {:?}", other),
        }
    }

    #[test]
    fn test_window_and_filter_clauses_rejected() {
        assert!(unsupported("SELECT COUNT(*) OVER () FROM t").contains("window"));
        assert!(unsupported("SELECT COUNT(*) FILTER (WHERE a > 1) FROM t").contains("FILTER"));
        assert!(unsupported("SELECT a FROM t WINDOW w AS (PARTITION BY a)").contains("WINDOW"));
    }

    #[test]
    fn test_ilike_and_like_escape() {
        let s = select("SELECT * FROM t WHERE a ILIKE 'x%'");
        assert!(matches!(
            s.where_clause,
            Some(Expr::Like {
                case_insensitive: true,
                ..
            })
        ));
        assert!(unsupported("SELECT * FROM t WHERE a LIKE 'x!%' ESCAPE '!'").contains("ESCAPE"));
    }

    #[test]
    fn test_comma_join_becomes_cross_join() {
        let s = select("SELECT * FROM a, b AS bb JOIN c ON bb.id = c.id");
        assert_eq!(s.from.base, "a");
        assert_eq!(s.from.joins.len(), 2);
        assert_eq!(s.from.joins[0].join_type, JoinType::Cross);
        assert_eq!(s.from.joins[0].alias.as_deref(), Some("bb"));
        assert_eq!(s.from.joins[1].join_type, JoinType::Inner);
    }

    #[test]
    fn test_cte_and_set_operations_rejected() {
        assert!(unsupported("WITH x AS (SELECT 1) SELECT * FROM x").contains("WITH"));
        assert!(Parser::parse("SELECT a FROM t UNION SELECT a FROM u").is_err());
    }

    #[test]
    fn test_insert_select_source() {
        match Parser::parse("INSERT INTO t (a) SELECT b FROM u WHERE b > 1").unwrap() {
            Statement::Insert(i) => match i.source {
                InsertSource::Select(s) => assert_eq!(s.from.base, "u"),
                other => panic!("Expected SELECT source, got {:?}", other),
            },
            other => panic!("Expected Insert, got {:?}", other),
        }
        assert!(
            unsupported("INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING").contains("ON CONFLICT")
        );
        assert!(unsupported("INSERT INTO t VALUES (1) RETURNING a").contains("RETURNING"));
    }

    #[test]
    fn test_update_alias_and_qualified_target() {
        match Parser::parse("UPDATE users u SET u.name = 'x' WHERE u.id = 1").unwrap() {
            Statement::Update(u) => {
                assert_eq!(u.alias.as_deref(), Some("u"));
                assert_eq!(u.assignments[0].column, "name");
            }
            other => panic!("Expected Update, got {:?}", other),
        }
        assert!(matches!(
            Parser::parse("UPDATE users SET other.name = 'x'"),
            Err(ParseError::InvalidIdentifier(_))
        ));
        assert!(Parser::parse("UPDATE users SET (a, b) = (1, 2)").is_err());
    }

    #[test]
    fn test_delete_alias_and_unsupported_clauses() {
        match Parser::parse("DELETE FROM users AS u WHERE u.id = 1").unwrap() {
            Statement::Delete(d) => assert_eq!(d.alias.as_deref(), Some("u")),
            other => panic!("Expected Delete, got {:?}", other),
        }
        assert!(unsupported("DELETE FROM users RETURNING id").contains("RETURNING"));
    }

    #[test]
    fn test_create_table_keys() {
        match Parser::parse(
            "CREATE TABLE t (a INT, b TEXT, c INT UNIQUE, PRIMARY KEY (b, a), UNIQUE (a, c))",
        )
        .unwrap()
        {
            Statement::CreateTable(ct) => {
                assert_eq!(ct.primary_key, vec!["b".to_string(), "a".to_string()]);
                assert_eq!(
                    ct.unique_keys,
                    vec![
                        vec!["c".to_string()],
                        vec!["a".to_string(), "c".to_string()]
                    ]
                );
                assert!(
                    ct.columns[0]
                        .constraints
                        .contains(&ColumnConstraint::PrimaryKey)
                );
                assert!(
                    !ct.columns[0]
                        .constraints
                        .contains(&ColumnConstraint::Unique)
                );
                assert!(
                    ct.columns[2]
                        .constraints
                        .contains(&ColumnConstraint::Unique)
                );
            }
            other => panic!("Expected CreateTable, got {:?}", other),
        }
    }

    #[test]
    fn test_create_table_rejections() {
        assert!(Parser::parse("CREATE TABLE t (a INT PRIMARY KEY, b INT PRIMARY KEY)").is_err());
        assert!(Parser::parse("CREATE TABLE t (a INT PRIMARY KEY, PRIMARY KEY (a))").is_err());
        assert!(matches!(
            Parser::parse("CREATE TABLE t (a INT, PRIMARY KEY (missing))"),
            Err(ParseError::InvalidIdentifier(_))
        ));
        assert!(unsupported("CREATE TABLE t (a INT CHECK (a > 0))").contains("CHECK"));
        assert!(unsupported("CREATE TABLE t (a INT REFERENCES u(id))").contains("REFERENCES"));
        assert!(unsupported("CREATE TABLE t AS SELECT 1").contains("AS SELECT"));
    }

    #[test]
    fn test_expr_display_round_trips_readably() {
        let s = select("SELECT COUNT(DISTINCT a), b || 'x', c NOT BETWEEN 1 AND 2 FROM t");
        let rendered: Vec<String> = s
            .columns
            .iter()
            .map(|c| match c {
                SelectColumn::Expr { expr, .. } => expr.to_string(),
                other => format!("{:?}", other),
            })
            .collect();
        assert_eq!(
            rendered,
            vec!["COUNT(DISTINCT a)", "b || 'x'", "c NOT BETWEEN 1 AND 2"]
        );
    }
}

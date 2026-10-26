use sqlparser::ast as sp;
use sqlparser::dialect::GenericDialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::Parser as SqlParser;
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};

use super::ast::*;
use super::error::{ParseError, ParseResult};

pub struct Parser;

pub const MAX_EXPR_DEPTH: usize = 1000;
pub const MAX_SEGMENT_TOKENS: usize = 10 * MAX_EXPR_DEPTH;
pub const MAX_COMPOUND_SELECT: usize = 500;
pub const MAX_FROM_TABLES: usize = 64;

fn parse_sql(sql: &str) -> ParseResult<Vec<sp::Statement>> {
    let dialect = GenericDialect {};
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize_with_location()
        .map_err(sqlparser::parser::ParserError::from)?;
    check_segment_lengths(&tokens)?;
    Ok(SqlParser::new(&dialect)
        .with_tokens_with_locations(tokens)
        .parse_statements()?)
}

fn check_segment_lengths(tokens: &[TokenWithSpan]) -> ParseResult<()> {
    let mut levels = vec![0usize];
    let mut set_operators = 0;
    for token in tokens {
        match &token.token {
            Token::Whitespace(_) => continue,
            Token::LParen => levels.push(0),
            Token::Comma => *levels.last_mut().unwrap() = 0,
            Token::SemiColon => {
                levels = vec![0];
                set_operators = 0;
            }
            Token::Word(word)
                if matches!(
                    word.keyword,
                    Keyword::UNION | Keyword::INTERSECT | Keyword::EXCEPT | Keyword::MINUS
                ) =>
            {
                set_operators += 1;
                if set_operators >= MAX_COMPOUND_SELECT {
                    return Err(ParseError::UnsupportedStatement(format!(
                        "more than {MAX_COMPOUND_SELECT} terms in a compound SELECT"
                    )));
                }
                *levels.last_mut().unwrap() += 1;
            }
            Token::RParen => {
                if levels.len() > 1 {
                    levels.pop();
                }
                *levels.last_mut().unwrap() += 1;
            }
            _ => *levels.last_mut().unwrap() += 1,
        }
        if *levels.last().unwrap() > MAX_SEGMENT_TOKENS {
            return Err(ParseError::UnsupportedExpression(format!(
                "more than {MAX_SEGMENT_TOKENS} tokens in one expression without a comma"
            )));
        }
    }
    Ok(())
}

thread_local! {
    static EXPR_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct ExprDepth;

impl ExprDepth {
    fn enter() -> ParseResult<ExprDepth> {
        let depth = EXPR_DEPTH.get() + 1;
        if depth > MAX_EXPR_DEPTH {
            return Err(ParseError::UnsupportedExpression(format!(
                "expression is nested more than {MAX_EXPR_DEPTH} levels deep"
            )));
        }
        EXPR_DEPTH.set(depth);
        Ok(ExprDepth)
    }
}

impl Drop for ExprDepth {
    fn drop(&mut self) {
        EXPR_DEPTH.set(EXPR_DEPTH.get() - 1);
    }
}

impl Parser {
    pub fn parse(sql: &str) -> ParseResult<Statement> {
        let sql = sql.trim();
        if sql.is_empty() {
            return Err(ParseError::EmptyQuery);
        }

        // special commands not supported by sqlparser
        let trimmed_sql = sql.trim_end_matches(';').trim();
        let upper = trimmed_sql.to_uppercase();
        if let Some(mode) = transaction_mode(&upper) {
            return Ok(Statement::Begin(mode));
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

        let statements = parse_sql(sql)?;

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
        let statements = parse_sql(sql)?;
        statements.iter().map(Self::convert_statement).collect()
    }

    fn convert_statement(stmt: &sp::Statement) -> ParseResult<Statement> {
        match stmt {
            sp::Statement::CreateTable(create) => Self::convert_create_table(create),
            sp::Statement::CreateIndex(create) => Self::convert_create_index(create),
            sp::Statement::AlterTable(alter) => Self::convert_alter_table(alter),
            sp::Statement::Drop {
                object_type,
                names,
                if_exists,
                ..
            } => Self::convert_drop(object_type, names, *if_exists),
            sp::Statement::Query(query) => match Self::convert_query(query)? {
                Query::Select(select) => Ok(Statement::Select(*select)),
                Query::SetOperation(operation) => Ok(Statement::SetOperation(operation)),
            },
            sp::Statement::Insert(insert) => Self::convert_insert(insert),
            sp::Statement::Update(update) => Self::convert_update(update),
            sp::Statement::Delete(delete) => Self::convert_delete(delete),
            sp::Statement::StartTransaction {
                modes,
                modifier,
                statements,
                exception,
                ..
            } => {
                if !modes.is_empty() || !statements.is_empty() || exception.is_some() {
                    return Err(ParseError::UnsupportedStatement(stmt.to_string()));
                }
                match modifier {
                    None | Some(sp::TransactionModifier::Deferred) => {
                        Ok(Statement::Begin(TransactionMode::Deferred))
                    }
                    Some(
                        sp::TransactionModifier::Immediate | sp::TransactionModifier::Exclusive,
                    ) => Ok(Statement::Begin(TransactionMode::Immediate)),
                    Some(_) => Err(ParseError::UnsupportedStatement(stmt.to_string())),
                }
            }
            sp::Statement::Commit {
                chain: false,
                modifier: None,
                ..
            } => Ok(Statement::Commit),
            sp::Statement::Rollback {
                chain: false,
                savepoint: None,
            } => Ok(Statement::Rollback),
            sp::Statement::Commit { .. } | sp::Statement::Rollback { .. } => {
                Err(ParseError::UnsupportedStatement(format!(
                    "{stmt} (savepoints and AND CHAIN are not supported)"
                )))
            }
            sp::Statement::ShowTables { .. } => Ok(Statement::ShowTables),
            sp::Statement::ExplainTable { table_name, .. } => {
                let name = Self::extract_table_name(table_name)?;
                Ok(Statement::Describe(name))
            }
            sp::Statement::Explain {
                describe_alias,
                analyze,
                verbose,
                query_plan,
                estimate,
                statement,
                format,
                options,
            } => {
                if !matches!(describe_alias, sp::DescribeAlias::Explain)
                    || *verbose
                    || *query_plan
                    || *estimate
                    || format.is_some()
                    || options.is_some()
                {
                    return Err(ParseError::UnsupportedStatement(
                        "only EXPLAIN and EXPLAIN ANALYZE are supported".into(),
                    ));
                }
                Ok(Statement::Explain {
                    analyze: *analyze,
                    statement: Box::new(Self::convert_statement(statement)?),
                })
            }
            other => Err(ParseError::UnsupportedStatement(format!("{}", other))),
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

        let mut checks = Vec::new();
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
                sp::TableConstraint::Check(check) => {
                    checks.push(Self::convert_check(check, check.name.as_ref())?);
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
            checks,
            if_not_exists: create.if_not_exists,
        }))
    }

    fn convert_check(
        check: &sp::CheckConstraint,
        name: Option<&sp::Ident>,
    ) -> ParseResult<CheckConstraint> {
        if check.no_inherit || check.enforced == Some(false) {
            return Err(ParseError::UnsupportedStatement(format!(
                "CHECK constraint form not supported: {check}"
            )));
        }
        Ok(CheckConstraint {
            name: name.map(|n| n.value.clone()),
            expr: Self::convert_expr(&check.expr)?,
        })
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
        let count =
            |f: fn(&sp::ColumnOption) -> bool| col.options.iter().filter(|o| f(&o.option)).count();
        let conflict = if count(|o| matches!(o, sp::ColumnOption::PrimaryKey(..))) > 1 {
            Some("multiple PRIMARY KEY declarations")
        } else if count(|o| matches!(o, sp::ColumnOption::Default(..))) > 1 {
            Some("multiple DEFAULT values")
        } else if count(|o| matches!(o, sp::ColumnOption::Null)) > 0
            && count(|o| matches!(o, sp::ColumnOption::NotNull)) > 0
        {
            Some("conflicting NULL and NOT NULL declarations")
        } else {
            None
        };
        if let Some(conflict) = conflict {
            return Err(ParseError::UnsupportedStatement(format!(
                "{conflict} for column {}",
                col.name.value
            )));
        }
        for opt in &col.options {
            if let sp::ColumnOption::Check(check) = &opt.option {
                let name = opt.name.as_ref().or(check.name.as_ref());
                constraints.push(ColumnConstraint::Check(Self::convert_check(check, name)?));
                continue;
            }
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

            other => Err(ParseError::UnsupportedDataType(format!("{}", other))),
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

    fn convert_create_index(create: &sp::CreateIndex) -> ParseResult<Statement> {
        let unsupported = |what: &str| {
            Err(ParseError::UnsupportedStatement(format!(
                "CREATE INDEX ... {what} not supported"
            )))
        };
        if create.using.is_some() || !create.index_options.is_empty() {
            return unsupported("USING");
        }
        if create.concurrently || create.r#async {
            return unsupported("CONCURRENTLY");
        }
        if !create.include.is_empty() {
            return unsupported("INCLUDE");
        }
        if create.nulls_distinct.is_some() {
            return unsupported("NULLS DISTINCT");
        }
        if !create.with.is_empty() || !create.alter_options.is_empty() {
            return unsupported("WITH");
        }
        if create.predicate.is_some() {
            return unsupported("WHERE (partial indexes)");
        }
        let Some(name) = &create.name else {
            return Err(ParseError::MissingClause("index name".into()));
        };
        let name = Self::extract_table_name(name)?;
        let table = Self::extract_table_name(&create.table_name)?;
        let columns = create
            .columns
            .iter()
            .map(|c| {
                let order = &c.column;
                if c.operator_class.is_some()
                    || order.with_fill.is_some()
                    || order.options.nulls_first.is_some()
                    || !matches!(order.options.sort, None | Some(sp::OrderBySort::Asc))
                {
                    return Err(ParseError::UnsupportedStatement(format!(
                        "index column options not supported: {}",
                        c.column
                    )));
                }
                match &order.expr {
                    sp::Expr::Identifier(ident) => Ok(ident.value.clone()),
                    other => Err(ParseError::UnsupportedStatement(format!(
                        "index on an expression not supported: {other}"
                    ))),
                }
            })
            .collect::<ParseResult<Vec<_>>>()?;
        if columns.is_empty() {
            return Err(ParseError::MissingClause("index columns".into()));
        }
        for (i, column) in columns.iter().enumerate() {
            if columns[..i].iter().any(|c| c.eq_ignore_ascii_case(column)) {
                return Err(ParseError::InvalidIdentifier(format!(
                    "column {column} appears twice in index {name}"
                )));
            }
        }
        Ok(Statement::CreateIndex(CreateIndex {
            name,
            table,
            columns,
            unique: create.unique,
            if_not_exists: create.if_not_exists,
        }))
    }

    fn convert_alter_table(alter: &sp::AlterTable) -> ParseResult<Statement> {
        if alter.only
            || alter.location.is_some()
            || alter.on_cluster.is_some()
            || alter.table_type.is_some()
        {
            return Err(ParseError::UnsupportedStatement(format!(
                "ALTER TABLE form not supported: {alter}"
            )));
        }
        let [operation] = alter.operations.as_slice() else {
            return Err(ParseError::UnsupportedStatement(
                "ALTER TABLE takes exactly one operation, use one statement per change".into(),
            ));
        };
        let operation = match operation {
            sp::AlterTableOperation::AddColumn {
                if_not_exists,
                column_def,
                column_position: None,
                ..
            } => AlterOperation::AddColumn {
                column: Self::convert_column_def(column_def)?,
                if_not_exists: *if_not_exists,
            },
            sp::AlterTableOperation::DropColumn {
                column_names,
                if_exists,
                drop_behavior: None,
                ..
            } => {
                let [name] = column_names.as_slice() else {
                    return Err(ParseError::UnsupportedStatement(
                        "ALTER TABLE DROP COLUMN takes one column".into(),
                    ));
                };
                AlterOperation::DropColumn {
                    name: name.value.clone(),
                    if_exists: *if_exists,
                }
            }
            sp::AlterTableOperation::RenameTable { table_name } => {
                let (sp::RenameTableNameKind::To(name) | sp::RenameTableNameKind::As(name)) =
                    table_name;
                AlterOperation::RenameTable {
                    name: Self::extract_table_name(name)?,
                }
            }
            sp::AlterTableOperation::RenameColumn {
                old_column_name,
                new_column_name,
            } => AlterOperation::RenameColumn {
                old: old_column_name.value.clone(),
                new: new_column_name.value.clone(),
            },
            other => {
                return Err(ParseError::UnsupportedStatement(format!(
                    "ALTER TABLE operation not supported: {other}"
                )));
            }
        };
        Ok(Statement::AlterTable(AlterTable {
            name: Self::extract_table_name(&alter.name)?,
            if_exists: alter.if_exists,
            operation,
        }))
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
            sp::ObjectType::Index => {
                if names.len() != 1 {
                    return Err(ParseError::UnsupportedStatement(
                        "DROP multiple indexes not supported".into(),
                    ));
                }
                let name = Self::extract_table_name(&names[0])?;
                Ok(Statement::DropIndex(DropIndex { name, if_exists }))
            }
            other => Err(ParseError::UnsupportedStatement(format!(
                "DROP {} not supported",
                other
            ))),
        }
    }

    fn convert_query(query: &sp::Query) -> ParseResult<Query> {
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

        let body = Self::convert_set_expr(query.body.as_ref())?;
        let (order_by, limit, offset) = Self::convert_order_limit(query)?;
        if order_by.is_empty() && limit.is_none() && offset.is_none() {
            return Ok(body);
        }
        match body {
            Query::Select(mut select)
                if select.order_by.is_empty()
                    && select.limit.is_none()
                    && select.offset.is_none() =>
            {
                select.order_by = order_by;
                select.limit = limit;
                select.offset = offset;
                Ok(Query::Select(select))
            }
            Query::SetOperation(mut operation)
                if operation.order_by.is_empty()
                    && operation.limit.is_none()
                    && operation.offset.is_none() =>
            {
                operation.order_by = order_by;
                operation.limit = limit;
                operation.offset = offset;
                Ok(Query::SetOperation(operation))
            }
            _ => Err(ParseError::UnsupportedStatement(
                "ORDER BY / LIMIT around a parenthesized query that has its own is not supported"
                    .into(),
            )),
        }
    }

    fn convert_set_expr(body: &sp::SetExpr) -> ParseResult<Query> {
        match body {
            sp::SetExpr::Select(select) => {
                Ok(Query::Select(Box::new(Self::convert_select(select)?)))
            }
            sp::SetExpr::Query(query) => Self::convert_query(query),
            sp::SetExpr::SetOperation {
                left,
                op,
                set_quantifier,
                right,
            } => {
                let op = match op {
                    sp::SetOperator::Union => SetOperator::Union,
                    sp::SetOperator::Intersect => SetOperator::Intersect,
                    sp::SetOperator::Except => SetOperator::Except,
                    sp::SetOperator::Minus => {
                        return Err(ParseError::UnsupportedStatement(
                            "MINUS not supported; use EXCEPT".into(),
                        ));
                    }
                };
                let all = match set_quantifier {
                    sp::SetQuantifier::All => true,
                    sp::SetQuantifier::Distinct | sp::SetQuantifier::None => false,
                    other => {
                        return Err(ParseError::UnsupportedStatement(format!(
                            "{op} {other} not supported"
                        )));
                    }
                };
                Ok(Query::SetOperation(Box::new(SetOperation {
                    op,
                    all,
                    left: Self::convert_set_expr(left)?,
                    right: Self::convert_set_expr(right)?,
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                })))
            }
            other => Err(ParseError::UnsupportedStatement(format!(
                "Unsupported query type: {}",
                other
            ))),
        }
    }

    fn convert_select(select: &sp::Select) -> ParseResult<Select> {
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

        Ok(Select {
            distinct,
            columns,
            from,
            where_clause,
            group_by,
            having,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })
    }

    fn convert_order_limit(
        query: &sp::Query,
    ) -> ParseResult<(Vec<OrderBy>, Option<usize>, Option<usize>)> {
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

        Ok((order_by, limit, offset))
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

    fn convert_from(from: &[sp::TableWithJoins]) -> ParseResult<Option<TableRef>> {
        let Some((first, rest)) = from.split_first() else {
            return Ok(None);
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
        if table_ref.joins.len() >= MAX_FROM_TABLES {
            return Err(ParseError::UnsupportedStatement(format!(
                "more than {MAX_FROM_TABLES} tables in one FROM clause"
            )));
        }
        Ok(Some(table_ref))
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
                            "Unsupported identifier depth: {}",
                            sp::ObjectName::from(parts.clone())
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
            _ => match Self::convert_query(query)? {
                Query::Select(select) => InsertSource::Select(select),
                Query::SetOperation(operation) => InsertSource::SetOperation(operation),
            },
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
        let _depth = ExprDepth::enter()?;
        Self::convert_expr_at_depth(expr)
    }

    fn convert_expr_at_depth(expr: &sp::Expr) -> ParseResult<Expr> {
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
                    "Unsupported identifier depth: {}",
                    sp::ObjectName::from(parts.clone())
                ))),
            },

            sp::Expr::Value(v) => Ok(Expr::Literal(Self::convert_value(v)?)),

            sp::Expr::BinaryOp { left, op, right } => {
                let mut ops = vec![(op, right.as_ref())];
                let mut current = left.as_ref();
                let mut spine_guards = Vec::new();
                while let sp::Expr::BinaryOp {
                    left: next_left,
                    op: next_op,
                    right: next_right,
                } = current
                {
                    spine_guards.push(ExprDepth::enter()?);
                    ops.push((next_op, next_right.as_ref()));
                    current = next_left.as_ref();
                }
                let mut acc = Self::convert_expr(current)?;
                while let Some((next_op, next_right)) = ops.pop() {
                    let _guard = spine_guards.pop();
                    let r = Self::convert_expr(next_right)?;
                    let o = Self::convert_binary_op(next_op)?;
                    acc = Expr::BinaryOp {
                        left: Box::new(acc),
                        op: o,
                        right: Box::new(r),
                    };
                }
                Ok(acc)
            }

            sp::Expr::UnaryOp { op, expr } => {
                if let (sp::UnaryOperator::Minus, sp::Expr::Value(v)) = (op, expr.as_ref())
                    && let sp::Value::Number(s, _) = &v.value
                    && s.parse::<i64>().is_err()
                    && let Ok(i) = format!("-{s}").parse::<i64>()
                {
                    return Ok(Expr::Literal(LiteralValue::Integer(i)));
                }
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

            sp::Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => Ok(Expr::Case {
                operand: operand
                    .as_ref()
                    .map(|e| Self::convert_expr(e).map(Box::new))
                    .transpose()?,
                branches: conditions
                    .iter()
                    .map(|w| {
                        Ok((
                            Self::convert_expr(&w.condition)?,
                            Self::convert_expr(&w.result)?,
                        ))
                    })
                    .collect::<ParseResult<Vec<_>>>()?,
                else_result: else_result
                    .as_ref()
                    .map(|e| Self::convert_expr(e).map(Box::new))
                    .transpose()?,
            }),

            sp::Expr::Cast {
                kind,
                expr,
                data_type,
                format,
            } => {
                if !matches!(kind, sp::CastKind::Cast | sp::CastKind::DoubleColon) {
                    return Err(ParseError::UnsupportedExpression(
                        "TRY_CAST / SAFE_CAST not supported".into(),
                    ));
                }
                if format.is_some() {
                    return Err(ParseError::UnsupportedExpression(
                        "CAST ... FORMAT not supported".into(),
                    ));
                }
                Ok(Expr::Cast {
                    expr: Box::new(Self::convert_expr(expr)?),
                    data_type: Self::convert_data_type(data_type)?,
                })
            }

            sp::Expr::Subquery(query) => Ok(Expr::Subquery(Box::new(Self::convert_query(query)?))),

            sp::Expr::Exists { subquery, negated } => Ok(Expr::Exists {
                query: Box::new(Self::convert_query(subquery)?),
                negated: *negated,
            }),

            sp::Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => Ok(Expr::InSubquery {
                expr: Box::new(Self::convert_expr(expr)?),
                query: Box::new(Self::convert_query(subquery)?),
                negated: *negated,
            }),

            sp::Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                let mut args = vec![Self::convert_expr(expr)?];
                match (substring_from, substring_for) {
                    (Some(from), _) => args.push(Self::convert_expr(from)?),
                    (None, Some(_)) => args.push(Expr::Literal(LiteralValue::Integer(1))),
                    (None, None) => {}
                }
                if let Some(length) = substring_for {
                    args.push(Self::convert_expr(length)?);
                }
                Ok(Expr::Function {
                    name: "SUBSTR".into(),
                    args: FunctionArgs::List {
                        args,
                        distinct: false,
                    },
                })
            }

            other => Err(ParseError::UnsupportedExpression(format!("{}", other))),
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
                "Unsupported value: {}",
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
                "Unsupported operator: {}",
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
                "Unsupported unary operator: {}",
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

fn transaction_mode(upper: &str) -> Option<TransactionMode> {
    if upper == "START TRANSACTION" {
        return Some(TransactionMode::Deferred);
    }
    let mut words = upper.split_whitespace();
    if words.next() != Some("BEGIN") {
        return None;
    }
    let mut rest: Vec<&str> = words.collect();
    if rest.last() == Some(&"TRANSACTION") {
        rest.pop();
    }
    match rest.as_slice() {
        [] | ["DEFERRED"] => Some(TransactionMode::Deferred),
        ["IMMEDIATE"] | ["EXCLUSIVE"] => Some(TransactionMode::Immediate),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_begin_modes() {
        let cases = [
            ("BEGIN", TransactionMode::Deferred),
            ("begin transaction;", TransactionMode::Deferred),
            ("START TRANSACTION", TransactionMode::Deferred),
            ("BEGIN DEFERRED", TransactionMode::Deferred),
            ("BEGIN IMMEDIATE", TransactionMode::Immediate),
            ("begin immediate transaction", TransactionMode::Immediate),
            ("BEGIN EXCLUSIVE;", TransactionMode::Immediate),
        ];
        for (sql, mode) in cases {
            assert_eq!(Parser::parse(sql).unwrap(), Statement::Begin(mode), "{sql}");
        }
        assert!(Parser::parse("BEGIN SOMETIMES").is_err());
        assert!(Parser::parse("BEGIN IMMEDIATE DEFERRED").is_err());
        let script = Parser::parse_multi(
            "BEGIN IMMEDIATE; COMMIT; BEGIN EXCLUSIVE TRANSACTION; COMMIT; BEGIN; COMMIT; START TRANSACTION; COMMIT",
        )
        .unwrap();
        let begins: Vec<_> = script
            .iter()
            .filter_map(|s| match s {
                Statement::Begin(mode) => Some(*mode),
                _ => None,
            })
            .collect();
        assert_eq!(
            begins,
            [
                TransactionMode::Immediate,
                TransactionMode::Immediate,
                TransactionMode::Deferred,
                TransactionMode::Deferred
            ]
        );
        assert!(Parser::parse_multi("START TRANSACTION ISOLATION LEVEL READ COMMITTED").is_err());
    }

    #[test]
    fn test_commit_and_rollback_variants() {
        for sql in [
            "COMMIT",
            "commit;",
            "COMMIT TRANSACTION",
            "COMMIT WORK",
            "END",
        ] {
            assert_eq!(Parser::parse(sql).unwrap(), Statement::Commit, "{sql}");
        }
        for sql in [
            "ROLLBACK",
            "rollback;",
            "ROLLBACK TRANSACTION",
            "ROLLBACK WORK",
        ] {
            assert_eq!(Parser::parse(sql).unwrap(), Statement::Rollback, "{sql}");
        }
        for sql in [
            "ROLLBACK TO s",
            "ROLLBACK TO SAVEPOINT s",
            "ROLLBACK TRANSACTION TO SAVEPOINT s",
            "ROLLBACK AND CHAIN",
            "COMMIT AND CHAIN",
            "SAVEPOINT s",
            "RELEASE SAVEPOINT s",
        ] {
            let err = Parser::parse(sql).unwrap_err().to_string();
            assert!(
                err.contains("not supported") || err.contains("Unsupported"),
                "{sql}: {err}"
            );
        }
    }

    #[test]
    fn test_conflicting_column_options_are_rejected() {
        for (sql, message) in [
            (
                "CREATE TABLE t (a INT PRIMARY KEY PRIMARY KEY)",
                "multiple PRIMARY KEY",
            ),
            (
                "CREATE TABLE t (a INT DEFAULT 1 DEFAULT 2)",
                "multiple DEFAULT",
            ),
            (
                "CREATE TABLE t (a INT NOT NULL NULL)",
                "conflicting NULL and NOT NULL",
            ),
            (
                "CREATE TABLE t (a INT NULL NOT NULL)",
                "conflicting NULL and NOT NULL",
            ),
        ] {
            let err = Parser::parse(sql).unwrap_err().to_string();
            assert!(err.contains(message), "{sql}: {err}");
        }
        for sql in [
            "CREATE TABLE t (a INT NULL NULL)",
            "CREATE TABLE t (a INT NOT NULL NOT NULL UNIQUE UNIQUE)",
        ] {
            assert!(Parser::parse(sql).is_ok(), "{sql}");
        }
    }

    #[test]
    fn test_unsupported_input_is_reported_as_sql() {
        for (sql, message) in [
            (
                "SELECT TRIM('  a ')",
                "Unsupported expression: TRIM('  a ')",
            ),
            (
                "SELECT 1 IS DISTINCT FROM 2",
                "Unsupported expression: 1 IS DISTINCT FROM 2",
            ),
            ("SELECT a.b.c FROM t", "a.b.c"),
        ] {
            let err = Parser::parse(sql).unwrap_err().to_string();
            assert!(err.contains(message), "{sql}: {err}");
            assert!(!err.contains("Span"), "{sql}: {err}");
        }
    }

    #[test]
    fn test_size_limits_are_reported_as_errors() {
        let terms = |n: usize| vec!["SELECT 1"; n].join(" UNION ALL ");
        assert!(Parser::parse(&terms(MAX_COMPOUND_SELECT)).is_ok());
        let err = Parser::parse(&terms(MAX_COMPOUND_SELECT + 1)).unwrap_err();
        assert!(
            err.to_string().contains("terms in a compound SELECT"),
            "{err}"
        );

        let tables = |n: usize| {
            let list: Vec<String> = (0..n).map(|i| format!("t AS t{i}")).collect();
            format!("SELECT 1 FROM {}", list.join(", "))
        };
        assert!(Parser::parse(&tables(MAX_FROM_TABLES)).is_ok());
        let err = Parser::parse(&tables(MAX_FROM_TABLES + 1)).unwrap_err();
        assert!(
            err.to_string().contains("tables in one FROM clause"),
            "{err}"
        );
        let joins: String = (1..=MAX_FROM_TABLES)
            .map(|i| format!(" JOIN t AS t{i} ON TRUE"))
            .collect();
        let err = Parser::parse(&format!("SELECT 1 FROM t AS t0{joins}")).unwrap_err();
        assert!(
            err.to_string().contains("tables in one FROM clause"),
            "{err}"
        );

        let chain = |n: usize| format!("SELECT {}", vec!["1"; n].join("+"));
        assert!(Parser::parse(&chain(MAX_EXPR_DEPTH)).is_ok());
        let err = Parser::parse(&chain(MAX_EXPR_DEPTH + 1)).unwrap_err();
        assert!(err.to_string().contains("nested more than"), "{err}");

        let parens = |n: usize| format!("SELECT {}1{}", "(".repeat(n), ")".repeat(n));
        assert!(Parser::parse(&parens(40)).is_ok());
        let err = Parser::parse(&parens(100)).unwrap_err();
        assert!(err.to_string().contains("nested too deeply"), "{err}");

        let long = format!("SELECT {}", vec!["1"; MAX_SEGMENT_TOKENS].join("+"));
        let err = Parser::parse(&long).unwrap_err();
        assert!(err.to_string().contains("without a comma"), "{err}");
    }

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
                assert_eq!(s.from.as_ref().unwrap().base, "a");
                assert!(s.from.as_ref().unwrap().base_alias.is_none());
                assert_eq!(s.from.as_ref().unwrap().joins.len(), 1);
                let j = &s.from.as_ref().unwrap().joins[0];
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
                assert_eq!(s.from.as_ref().unwrap().joins.len(), 1);
                assert_eq!(s.from.as_ref().unwrap().joins[0].join_type, JoinType::Left);
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_multiple_joins() {
        let sql = "SELECT * FROM a JOIN b ON a.id = b.a_id LEFT JOIN c ON b.id = c.b_id";
        let stmt = Parser::parse(sql).unwrap();
        println!("{:#?}", stmt);
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.from.as_ref().unwrap().base, "a");
                assert_eq!(s.from.as_ref().unwrap().joins.len(), 2);
                assert_eq!(s.from.as_ref().unwrap().joins[0].table, "b");
                assert_eq!(s.from.as_ref().unwrap().joins[0].join_type, JoinType::Inner);
                assert_eq!(s.from.as_ref().unwrap().joins[1].table, "c");
                assert_eq!(s.from.as_ref().unwrap().joins[1].join_type, JoinType::Left);
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
                assert_eq!(s.from.as_ref().unwrap().base, "users");
                assert_eq!(s.from.as_ref().unwrap().base_alias.as_deref(), Some("u"));
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
                assert_eq!(s.from.as_ref().unwrap().joins[0].table, "b");
                assert_eq!(
                    s.from.as_ref().unwrap().joins[0].alias.as_deref(),
                    Some("bb")
                );
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
                assert_eq!(s.from.as_ref().unwrap().joins.len(), 1);
                assert_eq!(s.from.as_ref().unwrap().joins[0].join_type, JoinType::Cross);
                assert!(s.from.as_ref().unwrap().joins[0].on.is_none());
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
        println!("{:#?}", stmt);
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

                assert_eq!(s.from.as_ref().unwrap().base, "a");
                assert_eq!(s.from.as_ref().unwrap().joins.len(), 1);
                assert_eq!(s.from.as_ref().unwrap().joins[0].table, "b");
                assert_eq!(s.from.as_ref().unwrap().joins[0].join_type, JoinType::Inner);

                match s.from.as_ref().unwrap().joins[0].on.as_ref().unwrap() {
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
        assert_eq!(s.from.as_ref().unwrap().base, "a");
        assert_eq!(s.from.as_ref().unwrap().joins.len(), 2);
        assert_eq!(s.from.as_ref().unwrap().joins[0].join_type, JoinType::Cross);
        assert_eq!(
            s.from.as_ref().unwrap().joins[0].alias.as_deref(),
            Some("bb")
        );
        assert_eq!(s.from.as_ref().unwrap().joins[1].join_type, JoinType::Inner);
    }

    #[test]
    fn test_cte_rejected() {
        assert!(unsupported("WITH x AS (SELECT 1) SELECT * FROM x").contains("WITH"));
    }

    fn set_operation(sql: &str) -> SetOperation {
        match Parser::parse(sql).unwrap() {
            Statement::SetOperation(s) => *s,
            other => panic!("Expected SetOperation, got {:?}", other),
        }
    }

    fn leaf_table(query: &Query) -> &str {
        match query {
            Query::Select(s) => &s.from.as_ref().unwrap().base,
            Query::SetOperation(s) => panic!("Expected a SELECT, got {s:?}"),
        }
    }

    #[test]
    fn test_set_operations_intersect_binds_tighter() {
        let top = set_operation(
            "SELECT a FROM t UNION ALL SELECT a FROM u INTERSECT SELECT a FROM v ORDER BY a DESC LIMIT 3 OFFSET 1",
        );
        assert_eq!(top.op, SetOperator::Union);
        assert!(top.all);
        assert_eq!(leaf_table(&top.left), "t");
        assert_eq!(top.order_by.len(), 1);
        assert!(!top.order_by[0].ascending);
        assert_eq!((top.limit, top.offset), (Some(3), Some(1)));
        let Query::SetOperation(right) = &top.right else {
            panic!("{:?}", top.right)
        };
        assert_eq!(right.op, SetOperator::Intersect);
        assert!(!right.all);
        assert_eq!(leaf_table(&right.left), "u");
        assert_eq!(leaf_table(&right.right), "v");

        let chain = set_operation("SELECT a FROM t EXCEPT SELECT a FROM u EXCEPT SELECT a FROM v");
        let Query::SetOperation(left) = &chain.left else {
            panic!("{:?}", chain.left)
        };
        assert_eq!(leaf_table(&left.left), "t");
        assert_eq!(leaf_table(&chain.right), "v");
        assert!(
            set_operation("SELECT a FROM t UNION DISTINCT SELECT a FROM u")
                .order_by
                .is_empty()
        );
    }

    #[test]
    fn test_subquery_expressions() {
        let s = select(
            "SELECT (SELECT MAX(a) FROM t) FROM u WHERE b NOT IN (SELECT a FROM t UNION SELECT 1) AND NOT EXISTS (SELECT * FROM v)",
        );
        let SelectColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!("{:?}", s.columns[0])
        };
        let Expr::Subquery(scalar) = expr else {
            panic!("{expr:?}")
        };
        assert_eq!(leaf_table(scalar), "t");
        let Some(Expr::BinaryOp { left, right, .. }) = &s.where_clause else {
            panic!("{:?}", s.where_clause)
        };
        assert!(matches!(
            left.as_ref(),
            Expr::InSubquery { negated: true, query, .. } if matches!(query.as_ref(), Query::SetOperation(_))
        ));
        assert!(matches!(right.as_ref(), Expr::Exists { negated: true, .. }));
    }

    #[test]
    fn test_queries_display_as_sql() {
        for sql in [
            "SELECT DISTINCT t.a, b AS c, * FROM t AS x LEFT JOIN u ON x.a = u.a WHERE a > 1 GROUP BY a HAVING COUNT(*) > 1 ORDER BY a DESC NULLS LAST LIMIT 3 OFFSET 2",
            "SELECT a FROM t UNION ALL (SELECT a FROM u INTERSECT SELECT a FROM v) ORDER BY 1",
            "(SELECT a FROM t LIMIT 1) EXCEPT SELECT a FROM u",
            "SELECT x.* FROM t AS x CROSS JOIN u",
        ] {
            let query = match Parser::parse(sql).unwrap() {
                Statement::Select(s) => Query::Select(Box::new(s)),
                Statement::SetOperation(s) => Query::SetOperation(s),
                other => panic!("{other:?}"),
            };
            let text = query.to_string();
            let again = match Parser::parse(&text).unwrap() {
                Statement::Select(s) => Query::Select(Box::new(s)),
                Statement::SetOperation(s) => Query::SetOperation(s),
                other => panic!("{other:?}"),
            };
            assert_eq!(query, again, "{sql} printed as {text}");
        }
    }

    #[test]
    fn test_select_without_from() {
        let s = select("SELECT 1 + 1");
        assert!(s.from.is_none());
        assert!(s.where_clause.is_none());
        assert!(select("SELECT 1 WHERE TRUE").where_clause.is_some());
    }

    #[test]
    fn test_parenthesized_queries() {
        let top = set_operation("(SELECT a FROM t ORDER BY a LIMIT 2) UNION SELECT a FROM u");
        let Query::Select(left) = &top.left else {
            panic!("{:?}", top.left)
        };
        assert_eq!(left.limit, Some(2));
        assert_eq!(left.order_by.len(), 1);
        assert_eq!(top.limit, None);

        let grouped =
            set_operation("SELECT a FROM t UNION (SELECT a FROM u UNION SELECT a FROM v)");
        assert!(matches!(grouped.right, Query::SetOperation(_)));

        let plain = select("(SELECT a FROM t) ORDER BY a LIMIT 1");
        assert_eq!(plain.limit, Some(1));
        assert_eq!(plain.order_by.len(), 1);

        assert!(unsupported("(SELECT a FROM t LIMIT 1) LIMIT 2").contains("parenthesized"));

        match Parser::parse("INSERT INTO t SELECT a FROM u UNION SELECT a FROM v").unwrap() {
            Statement::Insert(insert) => {
                assert!(matches!(insert.source, InsertSource::SetOperation(_)))
            }
            other => panic!("{other:?}"),
        }
    }

    fn where_of(sql: &str) -> Expr {
        select(sql).where_clause.unwrap()
    }

    #[test]
    fn test_case_simple_and_searched() {
        let searched = where_of("SELECT a FROM t WHERE CASE WHEN a > 1 THEN TRUE ELSE FALSE END");
        let Expr::Case {
            operand: None,
            branches,
            else_result: Some(_),
        } = &searched
        else {
            panic!("{searched:?}")
        };
        assert_eq!(branches.len(), 1);

        let simple =
            where_of("SELECT a FROM t WHERE CASE a WHEN 1 THEN TRUE WHEN 2 THEN FALSE END");
        let Expr::Case {
            operand: Some(operand),
            branches,
            else_result: None,
        } = &simple
        else {
            panic!("{simple:?}")
        };
        assert_eq!(operand.to_string(), "a");
        assert_eq!(branches.len(), 2);
        assert_eq!(
            simple.to_string(),
            "CASE a WHEN 1 THEN TRUE WHEN 2 THEN FALSE END"
        );
    }

    #[test]
    fn test_cast_forms() {
        for sql in [
            "SELECT a FROM t WHERE CAST(a AS BIGINT) = 1",
            "SELECT a FROM t WHERE a::INT = 1",
        ] {
            let Expr::BinaryOp { left, .. } = where_of(sql) else {
                panic!("{sql}")
            };
            assert_eq!(
                *left,
                Expr::Cast {
                    expr: Box::new(Expr::Column {
                        table: None,
                        name: "a".into()
                    }),
                    data_type: SqlDataType::Integer,
                }
            );
            assert_eq!(left.to_string(), "CAST(a AS INTEGER)");
        }
        assert!(unsupported("SELECT TRY_CAST(a AS INT) FROM t").contains("TRY_CAST"));
        assert!(Parser::parse("SELECT CAST(a AS BLOB) FROM t").is_err());
    }

    #[test]
    fn test_substring_forms_become_substr() {
        let args = |sql: &str| match where_of(sql) {
            Expr::BinaryOp { left, .. } => match *left {
                Expr::Function {
                    name,
                    args: FunctionArgs::List { args, .. },
                } => {
                    assert_eq!(name, "SUBSTR");
                    args.iter().map(|a| a.to_string()).collect::<Vec<_>>()
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        assert_eq!(
            args("SELECT a FROM t WHERE SUBSTR(a, 2, 3) = 'x'"),
            ["a", "2", "3"]
        );
        assert_eq!(
            args("SELECT a FROM t WHERE SUBSTRING(a FROM 2) = 'x'"),
            ["a", "2"]
        );
        assert_eq!(
            args("SELECT a FROM t WHERE SUBSTRING(a FOR 3) = 'x'"),
            ["a", "1", "3"]
        );
        assert_eq!(
            args("SELECT a FROM t WHERE SUBSTRING(a, 2) = 'x'"),
            ["a", "2"]
        );
    }

    #[test]
    fn test_insert_select_source() {
        match Parser::parse("INSERT INTO t (a) SELECT b FROM u WHERE b > 1").unwrap() {
            Statement::Insert(i) => match i.source {
                InsertSource::Select(s) => assert_eq!(s.from.as_ref().unwrap().base, "u"),
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
        assert!(
            unsupported("CREATE TABLE t (a INT, CHECK (a > 0) NOT ENFORCED)").contains("CHECK")
        );
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

    #[test]
    fn test_most_negative_integer_literal_stays_an_integer() {
        let s = select("SELECT -9223372036854775808, -5, 9223372036854775808 FROM t");
        let exprs: Vec<&Expr> = s
            .columns
            .iter()
            .map(|c| match c {
                SelectColumn::Expr { expr, .. } => expr,
                other => panic!("{:?}", other),
            })
            .collect();
        assert_eq!(exprs[0], &Expr::Literal(LiteralValue::Integer(i64::MIN)));
        assert!(matches!(
            exprs[1],
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                ..
            }
        ));
        assert!(matches!(exprs[2], Expr::Literal(LiteralValue::Float(_))));
    }

    #[test]
    fn test_check_constraints() {
        let Statement::CreateTable(create) = Parser::parse(
            "CREATE TABLE t (a INT CHECK (a > 0), b INT CONSTRAINT b_pos CHECK (b >= 0), \
             CHECK (a < b), CONSTRAINT pair CHECK (a + b < 100))",
        )
        .unwrap() else {
            unreachable!()
        };
        let column_check = |i: usize| match &create.columns[i].constraints[..] {
            [ColumnConstraint::Check(check)] => check.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(column_check(0).name, None);
        assert_eq!(column_check(0).expr.to_string(), "a > 0");
        assert_eq!(column_check(1).name.as_deref(), Some("b_pos"));
        let names: Vec<_> = create.checks.iter().map(|c| c.name.as_deref()).collect();
        assert_eq!(names, vec![None, Some("pair")]);
        assert_eq!(create.checks[0].to_string(), "CHECK (a < b)");
        assert_eq!(create.checks[1].to_string(), "CHECK constraint pair");
    }

    fn alter(sql: &str) -> AlterTable {
        match Parser::parse(sql) {
            Ok(Statement::AlterTable(alter)) => alter,
            other => panic!("{sql}: {other:?}"),
        }
    }

    #[test]
    fn test_alter_table_operations() {
        let add =
            alter("ALTER TABLE IF EXISTS t ADD COLUMN IF NOT EXISTS c INT NOT NULL DEFAULT 3");
        assert_eq!(add.name, "t");
        assert!(add.if_exists);
        let AlterOperation::AddColumn {
            column,
            if_not_exists: true,
        } = &add.operation
        else {
            panic!("{add:?}")
        };
        assert_eq!(column.name, "c");
        assert!(column.is_not_null());
        assert_eq!(
            column.default_value(),
            Some(&Expr::Literal(LiteralValue::Integer(3)))
        );

        assert_eq!(
            alter("ALTER TABLE t DROP c").operation,
            AlterOperation::DropColumn {
                name: "c".into(),
                if_exists: false
            }
        );
        assert_eq!(
            alter("ALTER TABLE t DROP COLUMN IF EXISTS c").operation,
            AlterOperation::DropColumn {
                name: "c".into(),
                if_exists: true
            }
        );
        assert_eq!(
            alter("ALTER TABLE t RENAME TO u").operation,
            AlterOperation::RenameTable { name: "u".into() }
        );
        assert_eq!(
            alter("ALTER TABLE t RENAME COLUMN a TO b").operation,
            AlterOperation::RenameColumn {
                old: "a".into(),
                new: "b".into()
            }
        );

        assert!(
            unsupported("ALTER TABLE t ADD a INT, ADD b INT").contains("exactly one operation")
        );
        assert!(unsupported("ALTER TABLE t DROP COLUMN a CASCADE").contains("not supported"));
        assert!(
            unsupported("ALTER TABLE t ALTER COLUMN a SET DEFAULT 1").contains("not supported")
        );
    }
}

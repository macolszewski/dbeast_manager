//! Fail-closed policy for read SQL in the configured database dialect.
//!
//! Parsing alone is not a safety boundary. Drivers also verify every physical
//! relation under locks, use read-only transactions, and pin identities. This
//! module only accepts syntax whose execution can be justified by that proof.

use super::{Result, dialect_identifier, error};
use crate::mcp::{MAX_SQL_BYTES, TableRef};
use crate::model::{DataSource, DriverKind};
use sqlparser::{
    ast::*,
    dialect::{Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect},
    parser::Parser,
};
use std::collections::HashSet;

pub(crate) struct ReadPlan {
    pub sql: String,
    pub tables: Vec<TableRef>,
    /// Actual MySQL relation aliases need separate LOCK TABLES entries.
    pub mysql_locks: Vec<(TableRef, Option<String>)>,
    /// PostgreSQL reads `rel.name` as `name(rel)` when `rel` has no such column.
    /// Each name qualified by a physical relation must be one of its columns.
    pub pg_columns: Vec<(TableRef, String)>,
    /// Names qualified by a CTE, subquery or unresolved qualifier. None may be
    /// a pg_catalog routine callable with a whole row.
    pub pg_row_attributes: Vec<String>,
}

pub(crate) const SAFE_FUNCTIONS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "abs",
    "lower",
    "upper",
    "length",
    "char_length",
    "octet_length",
    "round",
    "coalesce",
    "nullif",
];

/// pg_catalog routines (PostgreSQL 17) that accept a whole row as their only
/// required argument, so `rel.name` would call them. The driver repeats this
/// check against the server's own catalog for routines added later.
pub(crate) const PG_ROW_ROUTINES: &[&str] = &[
    "any_out",
    "any_value",
    "anycompatible_out",
    "anycompatiblenonarray_out",
    "anyelement_out",
    "anynonarray_out",
    "array_agg",
    "concat",
    "count",
    "cume_dist",
    "dense_rank",
    "first_value",
    "hash_record",
    "json_agg",
    "json_agg_strict",
    "json_build_array",
    "json_build_object",
    "jsonb_agg",
    "jsonb_agg_strict",
    "jsonb_build_array",
    "jsonb_build_object",
    "lag",
    "last_value",
    "lead",
    "mode",
    "num_nonnulls",
    "num_nulls",
    "percent_rank",
    "pg_collation_for",
    "pg_column_compression",
    "pg_column_size",
    "pg_column_toast_chunk_id",
    "pg_typeof",
    "quote_literal",
    "quote_nullable",
    "rank",
    "record_out",
    "record_send",
    "row_to_json",
    "to_json",
    "to_jsonb",
];

/// Column checks per physical relation, and names on other qualifiers.
type PgAttributes = (Vec<(TableRef, String)>, Vec<String>);

fn deny(message: &'static str) -> crate::mcp::ExecutionError {
    error("unsupported_safe_read", message)
}

pub(crate) fn analyze(source: &DataSource, sql: &str) -> Result<ReadPlan> {
    let statement = parse_one(source, sql)?;
    let Statement::Query(query) = &statement else {
        return Err(deny(
            "Only SELECT queries are supported by read-only access.",
        ));
    };
    let mut checker = Checker::new(source);
    checker.query(query, &HashSet::new(), 0)?;
    checker.reject_whole_rows()?;
    checker
        .tables
        .sort_by(|left, right| (&left.schema, &left.name).cmp(&(&right.schema, &right.name)));
    checker.tables.dedup();
    if checker.tables.len() > 16 || checker.locks.len() > 32 {
        return Err(deny(
            "A read query may reference at most 16 physical tables and 32 aliases.",
        ));
    }
    let normalized = statement.to_string();
    if normalized.len() > MAX_SQL_BYTES {
        return Err(deny("The normalized SQL exceeds 64 KiB."));
    }
    let (pg_columns, pg_row_attributes) = checker.pg_attributes()?;
    Ok(ReadPlan {
        sql: normalized,
        tables: checker.tables,
        mysql_locks: checker.locks,
        pg_columns,
        pg_row_attributes,
    })
}

/// Target of a single INSERT, UPDATE or DELETE that references no other relation
/// and calls only allowlisted built-in functions. Used to match a standing
/// table write permission; anything else falls back to explicit approval.
/// Triggers, rules and foreign-key actions defined on the table still run.
/// Returns the target and the SQL regenerated from the checked AST, which is
/// what runs, so tokenizer differences cannot reach the server.
pub(crate) fn table_write_target(source: &DataSource, sql: &str) -> Result<(TableRef, String)> {
    let statement = parse_one(source, sql)?;
    let mut checker = Checker::new(source);
    let target = match &statement {
        Statement::Insert(insert) => checker.insert(insert)?,
        Statement::Update(update) => checker.update(update)?,
        Statement::Delete(delete) => checker.delete(delete)?,
        _ => return Err(deny("Only INSERT, UPDATE or DELETE can use table write access.")),
    };
    checker.reject_whole_rows()?;
    if checker.tables.iter().any(|table| *table != target) {
        return Err(deny("A table write may reference only its target table."));
    }
    // Backstop for clauses the explicit checks above may not cover.
    let mut foreign = false;
    let _ = sqlparser::ast::visit_relations(&statement, |name| {
        let parts = checker.name(name).unwrap_or_default();
        let last = parts.last().cloned().unwrap_or_default();
        foreign |= match parts.len() {
            2 => parts[0] != target.schema || parts[1] != target.name,
            1 => last != target.name,
            _ => true,
        };
        std::ops::ControlFlow::<()>::Continue(())
    });
    let mut unsafe_function = false;
    let _ = sqlparser::ast::visit_expressions(&statement, |expr| {
        if let Expr::Function(function) = expr {
            let name = function.name.to_string().to_ascii_lowercase();
            let name = name.strip_prefix("pg_catalog.").unwrap_or(&name);
            unsafe_function |= !SAFE_FUNCTIONS.contains(&name);
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    if foreign || unsafe_function {
        return Err(deny(
            "A table write may reference only its target table and built-in functions.",
        ));
    }
    // Without a catalog check, any name of a whole-row routine is refused.
    if source.driver == DriverKind::Postgres
        && checker
            .attributes
            .iter()
            .any(|(_, name)| PG_ROW_ROUTINES.contains(&name.as_str()))
    {
        return Err(deny(
            "A table write cannot use a qualified name that PostgreSQL may call as a function.",
        ));
    }
    let normalized = statement.to_string();
    if normalized.len() > MAX_SQL_BYTES {
        return Err(deny("The normalized SQL exceeds 64 KiB."));
    }
    Ok((target, normalized))
}

fn parse_one(source: &DataSource, sql: &str) -> Result<Statement> {
    if sql.is_empty() || sql.len() > MAX_SQL_BYTES || sql.contains('\0') {
        return Err(error(
            "invalid_input",
            "Read SQL must contain one query of at most 64 KiB.",
        ));
    }
    // Server-executed comments and dialect-specific string escaping are not
    // allowed to introduce tokens invisible/different to the parser.
    if sql.contains("/*!") || sql.to_ascii_lowercase().contains("/*m!") || sql.contains('\\') {
        return Err(deny(
            "Executable comments and backslash escapes are not supported in read SQL.",
        ));
    }
    let dialect: Box<dyn Dialect> = match source.driver {
        DriverKind::Postgres => Box::new(PostgreSqlDialect {}),
        DriverKind::MySql => Box::new(MySqlDialect {}),
        DriverKind::Sqlite => Box::new(SQLiteDialect {}),
    };
    let mut statements = Parser::new(dialect.as_ref())
        .with_recursion_limit(64)
        .try_with_sql(sql)
        .map_err(|_| deny("Read SQL could not be parsed safely in this database dialect."))?
        .parse_statements()
        .map_err(|_| deny("Read SQL could not be parsed safely in this database dialect."))?;
    if statements.len() != 1 {
        return Err(deny("SQL must contain exactly one statement."));
    }
    Ok(statements.remove(0))
}

struct Checker<'a> {
    source: &'a DataSource,
    tables: Vec<TableRef>,
    locks: Vec<(TableRef, Option<String>)>,
    row_names: HashSet<Vec<String>>,
    scalar_names: Vec<Vec<String>>,
    /// Qualifier and last part of every qualified scalar name.
    attributes: Vec<(Vec<String>, String)>,
    /// Qualifiers naming a physical relation, and the relation.
    physical: Vec<(Vec<String>, TableRef)>,
    /// Qualifiers naming a CTE, subquery or parenthesized join.
    derived: HashSet<Vec<String>>,
    nodes: usize,
}
impl<'a> Checker<'a> {
    fn new(source: &'a DataSource) -> Self {
        Self {
            source,
            tables: Vec::new(),
            locks: Vec::new(),
            row_names: HashSet::new(),
            scalar_names: Vec::new(),
            attributes: Vec::new(),
            physical: Vec::new(),
            derived: HashSet::new(),
            nodes: 0,
        }
    }
    // A PostgreSQL table has a custom composite row type even when every one
    // of its columns is built-in. Implicit casts of a whole-row argument can
    // invoke user routines. Conservatively reject any scalar name that can
    // denote a relation; qualifying an ambiguous column as alias.column is
    // sufficient. Collect across the AST first so nested/forward aliases also
    // fail closed without asking the server to plan unverified expressions.
    fn reject_whole_rows(&self) -> Result<()> {
        if self
            .scalar_names
            .iter()
            .any(|name| self.row_names.contains(name))
        {
            return Err(deny(
                "Whole-row values are not supported in read SQL. Qualify individual columns as alias.column.",
            ));
        }
        Ok(())
    }
    /// Splits qualified names by what their qualifier can denote. A qualifier
    /// that is also derived somewhere, or unresolved, must not name a
    /// whole-row routine; a physical one must name a real column.
    fn pg_attributes(&self) -> Result<PgAttributes> {
        let mut columns = Vec::new();
        let mut row_attributes = Vec::new();
        if self.source.driver != DriverKind::Postgres {
            return Ok((columns, row_attributes));
        }
        for (qualifier, name) in &self.attributes {
            let mut physical = false;
            for (alias, table) in &self.physical {
                if alias == qualifier {
                    physical = true;
                    let check = (table.clone(), name.clone());
                    if !columns.contains(&check) {
                        columns.push(check);
                    }
                }
            }
            if !physical || self.derived.contains(qualifier) {
                if PG_ROW_ROUTINES.contains(&name.as_str()) {
                    return Err(deny(
                        "This qualified name would call a PostgreSQL function on a whole row. Rename the column with an alias.",
                    ));
                }
                if !row_attributes.contains(name) {
                    row_attributes.push(name.clone());
                }
            }
        }
        Ok((columns, row_attributes))
    }
    fn physical_qualifiers(&mut self, name: &[String], alias: Option<String>, table: &TableRef) {
        match alias {
            Some(alias) => self.physical.push((vec![alias], table.clone())),
            None => {
                self.physical.push((name.to_vec(), table.clone()));
                self.physical.push((vec![table.name.clone()], table.clone()));
            }
        }
    }
    /// Resolves the DML target exactly like a physical table in FROM.
    fn target(&mut self, name: &ObjectName, alias: Option<&Ident>) -> Result<TableRef> {
        let name = self.name(name)?;
        if name.len() != 2 {
            return Err(deny("The target table must be qualified as schema.table."));
        }
        let table = TableRef {
            schema: name[0].clone(),
            name: name[1].clone(),
        };
        super::preview(
            self.source,
            &crate::mcp::Operation::DescribeTable {
                source_id: String::new(),
                table: table.clone(),
            },
        )?;
        self.row_names.insert(name.clone());
        self.row_names.insert(vec![table.name.clone()]);
        let alias = alias.map(|alias| self.ident(alias)).transpose()?;
        if let Some(alias) = &alias {
            self.row_names.insert(vec![alias.clone()]);
        }
        self.physical_qualifiers(&name, alias, &table);
        self.tables.push(table.clone());
        Ok(table)
    }
    fn column(&self, name: &ObjectName) -> Result<()> {
        if self.name(name)?.len() != 1 {
            return Err(deny("Write columns must be simple column names."));
        }
        Ok(())
    }
    fn assignments(&mut self, assignments: &[Assignment]) -> Result<()> {
        for Assignment { target, value } in assignments {
            match target {
                AssignmentTarget::ColumnName(name) => self.column(name)?,
                AssignmentTarget::Tuple(_) => {
                    return Err(deny("Tuple assignments are not supported."));
                }
            }
            self.expr(value, &HashSet::new(), 1)?;
        }
        Ok(())
    }
    fn returning(&mut self, returning: &Option<Vec<SelectItem>>) -> Result<()> {
        for item in returning.iter().flatten() {
            match item {
                SelectItem::UnnamedExpr(expr) => self.expr(expr, &HashSet::new(), 1)?,
                SelectItem::ExprWithAlias { expr, alias } => {
                    self.ident(alias)?;
                    self.expr(expr, &HashSet::new(), 1)?;
                }
                SelectItem::Wildcard(options) => self.wildcard(options)?,
                _ => return Err(deny("This RETURNING form is not supported.")),
            }
        }
        Ok(())
    }
    fn insert(&mut self, insert: &Insert) -> Result<TableRef> {
        let Insert {
            insert_token: _,
            optimizer_hints,
            or,
            ignore: _,
            into: _,
            table,
            table_alias,
            columns,
            overwrite,
            source,
            assignments,
            partitioned,
            after_columns,
            has_table_keyword,
            on,
            returning,
            output,
            replace_into,
            priority,
            insert_alias,
            settings,
            format_clause,
            multi_table_insert_type,
            multi_table_into_clauses,
            multi_table_when_clauses,
            multi_table_else_clause,
        } = insert;
        if !optimizer_hints.is_empty()
            || or.is_some()
            || *overwrite
            || partitioned.is_some()
            || !after_columns.is_empty()
            || *has_table_keyword
            || on.is_some()
            || output.is_some()
            || *replace_into
            || priority.is_some()
            || insert_alias.is_some()
            || settings.is_some()
            || format_clause.is_some()
            || multi_table_insert_type.is_some()
            || !multi_table_into_clauses.is_empty()
            || !multi_table_when_clauses.is_empty()
            || multi_table_else_clause.is_some()
        {
            return Err(deny(
                "Upserts, REPLACE and extended INSERT clauses are not supported by table write access.",
            ));
        }
        let TableObject::TableName(name) = table else {
            return Err(deny("INSERT must target a table name."));
        };
        let target = self.target(name, table_alias.as_ref().map(|alias| &alias.alias))?;
        for column in columns {
            self.column(column)?;
        }
        self.assignments(assignments)?;
        match source.as_deref() {
            Some(Query {
                with: None,
                body,
                order_by: None,
                limit_clause: None,
                fetch: None,
                locks,
                for_clause: None,
                settings: None,
                format_clause: None,
                pipe_operators,
            }) if locks.is_empty() && pipe_operators.is_empty() => match body.as_ref() {
                SetExpr::Values(Values { rows, .. }) => {
                    for row in rows {
                        for expr in &row.content {
                            self.expr(expr, &HashSet::new(), 1)?;
                        }
                    }
                }
                _ => {
                    let query = source.as_deref().expect("insert source");
                    self.query(query, &HashSet::new(), 1)?;
                }
            },
            Some(query) => self.query(query, &HashSet::new(), 1)?,
            None if !assignments.is_empty() => {}
            None => return Err(deny("INSERT must provide VALUES or a SELECT.")),
        }
        self.returning(returning)?;
        Ok(target)
    }
    fn update(&mut self, update: &Update) -> Result<TableRef> {
        let Update {
            update_token: _,
            optimizer_hints,
            table,
            assignments,
            from,
            selection,
            returning,
            output,
            or,
            order_by,
            limit,
        } = update;
        if !optimizer_hints.is_empty()
            || from.is_some()
            || output.is_some()
            || or.is_some()
            || !order_by.is_empty()
            || limit.is_some()
            || !table.joins.is_empty()
        {
            return Err(deny(
                "UPDATE with FROM, joins, ORDER BY or LIMIT is not supported by table write access.",
            ));
        }
        let TableFactor::Table {
            name,
            alias,
            args: None,
            with_hints,
            version: None,
            with_ordinality: false,
            partitions,
            json_path: None,
            sample: None,
            index_hints,
        } = &table.relation
        else {
            return Err(deny("UPDATE must target a plain table."));
        };
        if !with_hints.is_empty()
            || !partitions.is_empty()
            || !index_hints.is_empty()
            || alias.as_ref().is_some_and(|alias| !alias.columns.is_empty() || alias.at.is_some())
        {
            return Err(deny("UPDATE must target a plain table."));
        }
        let target = self.target(name, alias.as_ref().map(|alias| &alias.name))?;
        self.assignments(assignments)?;
        if let Some(selection) = selection {
            self.expr(selection, &HashSet::new(), 1)?;
        }
        self.returning(returning)?;
        Ok(target)
    }
    fn delete(&mut self, delete: &Delete) -> Result<TableRef> {
        let Delete {
            delete_token: _,
            optimizer_hints,
            tables,
            from,
            using,
            selection,
            returning,
            output,
            order_by,
            limit,
        } = delete;
        if !optimizer_hints.is_empty()
            || !tables.is_empty()
            || using.is_some()
            || output.is_some()
            || !order_by.is_empty()
            || limit.is_some()
        {
            return Err(deny(
                "DELETE with USING, multiple tables, ORDER BY or LIMIT is not supported by table write access.",
            ));
        }
        let (FromTable::WithFromKeyword(relations) | FromTable::WithoutKeyword(relations)) = from;
        let [TableWithJoins { relation, joins }] = relations.as_slice() else {
            return Err(deny("DELETE must target exactly one table."));
        };
        let TableFactor::Table {
            name,
            alias,
            args: None,
            with_hints,
            version: None,
            with_ordinality: false,
            partitions,
            json_path: None,
            sample: None,
            index_hints,
        } = relation
        else {
            return Err(deny("DELETE must target a plain table."));
        };
        if !joins.is_empty()
            || !with_hints.is_empty()
            || !partitions.is_empty()
            || !index_hints.is_empty()
            || alias.as_ref().is_some_and(|alias| !alias.columns.is_empty() || alias.at.is_some())
        {
            return Err(deny("DELETE must target a plain table."));
        }
        let target = self.target(name, alias.as_ref().map(|alias| &alias.name))?;
        if let Some(selection) = selection {
            self.expr(selection, &HashSet::new(), 1)?;
        }
        self.returning(returning)?;
        Ok(target)
    }
    fn tick(&mut self, depth: usize) -> Result<()> {
        self.nodes += 1;
        if self.nodes > 2048 || depth > 48 {
            Err(deny(
                "Read SQL exceeds the supported expression complexity.",
            ))
        } else {
            Ok(())
        }
    }
    fn ident(&self, ident: &Ident) -> Result<String> {
        dialect_identifier(&ident.value, self.source.driver)?;
        if ident.quote_style.is_none()
            && (ident.value.starts_with('@') || ident.value.starts_with('$'))
        {
            return Err(deny(
                "Session variables and parameters are not supported in read SQL.",
            ));
        }
        if !matches!(ident.quote_style, None | Some('"') | Some('`') | Some('[')) {
            return Err(deny("This identifier quoting style is unsupported."));
        }
        Ok(
            if ident.quote_style.is_none() && self.source.driver == DriverKind::Postgres {
                ident.value.to_ascii_lowercase()
            } else {
                ident.value.clone()
            },
        )
    }
    fn name(&self, name: &ObjectName) -> Result<Vec<String>> {
        name.0
            .iter()
            .map(|part| match part {
                ObjectNamePart::Identifier(ident) => self.ident(ident),
                _ => Err(deny("Dynamic object names are not supported.")),
            })
            .collect()
    }
    fn alias(&self, alias: &TableAlias) -> Result<String> {
        let TableAlias {
            explicit: _,
            name,
            columns,
            at,
        } = alias;
        if at.is_some() || columns.iter().any(|column| column.data_type.is_some()) {
            return Err(deny("Typed or extended table aliases are not supported."));
        }
        for column in columns {
            self.ident(&column.name)?;
        }
        self.ident(name)
    }
    fn query(&mut self, query: &Query, inherited: &HashSet<String>, depth: usize) -> Result<()> {
        self.tick(depth)?;
        let Query {
            with,
            body,
            order_by,
            limit_clause,
            fetch,
            locks,
            for_clause,
            settings,
            format_clause,
            pipe_operators,
        } = query;
        if fetch.is_some()
            || !locks.is_empty()
            || for_clause.is_some()
            || settings.is_some()
            || format_clause.is_some()
            || !pipe_operators.is_empty()
        {
            return Err(deny(
                "Locking, FETCH, formatting and query setting clauses are not supported in read SQL.",
            ));
        }
        // Lexical CTE scope: a CTE sees inherited and earlier CTEs, never itself
        // or later siblings. Inner WITH declarations never escape their query.
        let mut scope = inherited.clone();
        if let Some(With {
            with_token: _,
            recursive,
            cte_tables,
        }) = with
        {
            if *recursive {
                return Err(deny(
                    "Recursive CTEs are not supported by read-only access.",
                ));
            }
            for Cte {
                alias,
                query,
                from,
                materialized: _,
                closing_paren_token: _,
            } in cte_tables
            {
                if from.is_some() {
                    return Err(deny("Extended CTE syntax is not supported."));
                }
                let name = self.alias(alias)?;
                self.row_names.insert(vec![name.clone()]);
                self.query(query, &scope, depth + 1)?;
                scope.insert(name);
            }
        }
        self.body(body, &scope, depth + 1)?;
        if let Some(OrderBy { kind, interpolate }) = order_by {
            if interpolate.is_some() {
                return Err(deny("ORDER BY interpolation is not supported."));
            }
            let OrderByKind::Expressions(expressions) = kind else {
                return Err(deny("ORDER BY ALL is not supported."));
            };
            for OrderByExpr {
                expr,
                options,
                with_fill,
            } in expressions
            {
                let OrderByOptions {
                    sort,
                    nulls_first: _,
                } = options;
                if with_fill.is_some() || matches!(sort, Some(OrderBySort::Using(_))) {
                    return Err(deny(
                        "Custom sort operators and WITH FILL are not supported.",
                    ));
                }
                self.expr(expr, &scope, depth + 1)?;
            }
        }
        if let Some(clause) = limit_clause {
            match clause {
                LimitClause::LimitOffset {
                    limit,
                    offset,
                    limit_by,
                } => {
                    if !limit_by.is_empty() {
                        return Err(deny("LIMIT BY is not supported."));
                    }
                    if let Some(limit) = limit {
                        self.numeric_limit(limit)?;
                    }
                    if let Some(offset) = offset {
                        self.numeric_limit(&offset.value)?;
                    }
                }
                LimitClause::OffsetCommaLimit { offset, limit } => {
                    self.numeric_limit(offset)?;
                    self.numeric_limit(limit)?;
                }
            }
        }
        Ok(())
    }
    fn numeric_limit(&self, expr: &Expr) -> Result<()> {
        if matches!(expr,Expr::Value(value) if matches!(&value.value,Value::Number(number,false) if number.parse::<u32>().is_ok_and(|n|n<=1_000_000)))
        {
            Ok(())
        } else {
            Err(deny(
                "LIMIT and OFFSET must be non-negative integers no larger than 1000000.",
            ))
        }
    }
    fn body(&mut self, body: &SetExpr, scope: &HashSet<String>, depth: usize) -> Result<()> {
        self.tick(depth)?;
        match body {
            SetExpr::Select(select) => self.select(select, scope, depth + 1),
            SetExpr::Query(query) => self.query(query, scope, depth + 1),
            SetExpr::SetOperation {
                left,
                op,
                set_quantifier,
                right,
            } if matches!(
                op,
                SetOperator::Union | SetOperator::Except | SetOperator::Intersect
            ) && matches!(
                set_quantifier,
                SetQuantifier::All | SetQuantifier::Distinct | SetQuantifier::None
            ) =>
            {
                self.body(left, scope, depth + 1)?;
                self.body(right, scope, depth + 1)
            }
            _ => Err(deny(
                "Only SELECT bodies and ordinary UNION, EXCEPT or INTERSECT are supported. Data-modifying CTEs are forbidden.",
            )),
        }
    }
    fn select(&mut self, select: &Select, scope: &HashSet<String>, depth: usize) -> Result<()> {
        let Select {
            select_token: _,
            optimizer_hints,
            distinct,
            select_modifiers,
            top,
            top_before_distinct: _,
            projection,
            exclude,
            into,
            from,
            lateral_views,
            prewhere,
            selection,
            connect_by,
            group_by,
            cluster_by,
            distribute_by,
            sort_by,
            having,
            named_window,
            qualify,
            window_before_qualify: _,
            value_table_mode,
            flavor,
        } = select;
        if !optimizer_hints.is_empty()
            || select_modifiers
                .as_ref()
                .is_some_and(SelectModifiers::is_any_set)
            || top.is_some()
            || exclude.is_some()
            || into.is_some()
            || !lateral_views.is_empty()
            || prewhere.is_some()
            || !connect_by.is_empty()
            || !cluster_by.is_empty()
            || !distribute_by.is_empty()
            || !sort_by.is_empty()
            || !named_window.is_empty()
            || qualify.is_some()
            || value_table_mode.is_some()
            || !matches!(flavor, SelectFlavor::Standard)
        {
            return Err(deny(
                "SELECT INTO, optimizer hints, windows and extended SELECT clauses are not supported in read SQL.",
            ));
        }
        if distinct
            .as_ref()
            .is_some_and(|value| !matches!(value, Distinct::Distinct))
        {
            return Err(deny("DISTINCT ON is not supported."));
        }
        for item in projection {
            match item {
                SelectItem::UnnamedExpr(expr) => self.expr(expr, scope, depth + 1)?,
                SelectItem::ExprWithAlias { expr, alias } => {
                    self.ident(alias)?;
                    self.expr(expr, scope, depth + 1)?;
                }
                SelectItem::Wildcard(options) => self.wildcard(options)?,
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(name),
                    options,
                ) => {
                    self.name(name)?;
                    self.wildcard(options)?;
                }
                _ => return Err(deny("This projection form is not supported in read SQL.")),
            }
        }
        for relation in from {
            self.relation(relation, scope, depth + 1)?;
        }
        if let Some(expression) = selection {
            self.expr(expression, scope, depth + 1)?;
        }
        match group_by {
            GroupByExpr::Expressions(expressions, modifiers) if modifiers.is_empty() => {
                for expression in expressions {
                    self.expr(expression, scope, depth + 1)?;
                }
            }
            _ => return Err(deny("Extended GROUP BY is not supported.")),
        }
        if let Some(expression) = having {
            self.expr(expression, scope, depth + 1)?;
        }
        Ok(())
    }
    fn wildcard(&self, options: &WildcardAdditionalOptions) -> Result<()> {
        let WildcardAdditionalOptions {
            wildcard_token: _,
            opt_ilike,
            opt_exclude,
            opt_except,
            opt_replace,
            opt_rename,
            opt_alias,
        } = options;
        if opt_ilike.is_some()
            || opt_exclude.is_some()
            || opt_except.is_some()
            || opt_replace.is_some()
            || opt_rename.is_some()
            || opt_alias.is_some()
        {
            Err(deny("Extended wildcard syntax is not supported."))
        } else {
            Ok(())
        }
    }
    fn relation(
        &mut self,
        relation: &TableWithJoins,
        scope: &HashSet<String>,
        depth: usize,
    ) -> Result<()> {
        let TableWithJoins { relation, joins } = relation;
        self.factor(relation, scope, depth + 1)?;
        for Join {
            relation,
            global,
            join_operator,
        } in joins
        {
            if *global {
                return Err(deny("GLOBAL joins are not supported."));
            }
            self.factor(relation, scope, depth + 1)?;
            let constraint = match join_operator {
                JoinOperator::Join(value)
                | JoinOperator::Inner(value)
                | JoinOperator::Left(value)
                | JoinOperator::LeftOuter(value)
                | JoinOperator::Right(value)
                | JoinOperator::RightOuter(value)
                | JoinOperator::FullOuter(value)
                | JoinOperator::CrossJoin(value) => value,
                _ => return Err(deny("This JOIN kind is not supported.")),
            };
            match constraint {
                JoinConstraint::On(expr) => self.expr(expr, scope, depth + 1)?,
                JoinConstraint::Using(columns) => {
                    for column in columns {
                        if self.name(column)?.len() != 1 {
                            return Err(deny("JOIN USING requires simple column names."));
                        }
                    }
                }
                JoinConstraint::Natural | JoinConstraint::None => {}
            }
        }
        Ok(())
    }
    fn factor(
        &mut self,
        factor: &TableFactor,
        scope: &HashSet<String>,
        depth: usize,
    ) -> Result<()> {
        self.tick(depth)?;
        match factor {
            TableFactor::Table {
                name,
                alias,
                args,
                with_hints,
                version,
                with_ordinality,
                partitions,
                json_path,
                sample,
                index_hints,
            } => {
                if args.is_some()
                    || !with_hints.is_empty()
                    || version.is_some()
                    || *with_ordinality
                    || !partitions.is_empty()
                    || json_path.is_some()
                    || sample.is_some()
                    || !index_hints.is_empty()
                {
                    return Err(deny(
                        "Table functions, samples, partitions and hints are not supported.",
                    ));
                }
                let alias = alias.as_ref().map(|alias| self.alias(alias)).transpose()?;
                let name = self.name(name)?;
                if let Some(alias) = &alias {
                    self.row_names.insert(vec![alias.clone()]);
                }
                self.row_names.insert(name.clone());
                if let Some(last) = name.last() {
                    self.row_names.insert(vec![last.clone()]);
                }
                if name.len() == 1 && scope.contains(&name[0]) {
                    self.derived.insert(vec![alias.unwrap_or_else(|| name[0].clone())]);
                    return Ok(());
                }
                if name.len() != 2 {
                    return Err(deny(
                        "Every physical table must be qualified as schema.table; unqualified names are allowed only for in-scope CTEs.",
                    ));
                }
                let table = TableRef {
                    schema: name[0].clone(),
                    name: name[1].clone(),
                };
                // Reuse exactly the structured read's namespace restrictions.
                super::preview(
                    self.source,
                    &crate::mcp::Operation::DescribeTable {
                        source_id: String::new(),
                        table: table.clone(),
                    },
                )?;
                self.tables.push(table.clone());
                self.physical_qualifiers(&name, alias.clone(), &table);
                if !self.locks.contains(&(table.clone(), alias.clone())) {
                    self.locks.push((table, alias));
                }
                Ok(())
            }
            TableFactor::Derived {
                lateral,
                subquery,
                alias,
                sample,
            } => {
                if *lateral || sample.is_some() {
                    return Err(deny("LATERAL and sampled subqueries are not supported."));
                }
                if let Some(alias) = alias {
                    let alias = self.alias(alias)?;
                    self.row_names.insert(vec![alias.clone()]);
                    self.derived.insert(vec![alias]);
                }
                self.query(subquery, scope, depth + 1)
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                if let Some(alias) = alias {
                    let alias = self.alias(alias)?;
                    self.row_names.insert(vec![alias.clone()]);
                    self.derived.insert(vec![alias]);
                }
                self.relation(table_with_joins, scope, depth + 1)
            }
            _ => Err(deny(
                "Only physical tables, CTEs and SELECT subqueries are supported in FROM.",
            )),
        }
    }
    fn expr(&mut self, expr: &Expr, scope: &HashSet<String>, depth: usize) -> Result<()> {
        self.tick(depth)?;
        match expr {
            Expr::Identifier(ident) => {
                self.scalar_names.push(vec![self.ident(ident)?]);
            }
            Expr::CompoundIdentifier(parts) => {
                let mut name = parts
                    .iter()
                    .map(|ident| self.ident(ident))
                    .collect::<Result<Vec<_>>>()?;
                self.scalar_names.push(name.clone());
                if let Some(last) = name.pop()
                    && !name.is_empty()
                {
                    self.attributes.push((name, last));
                }
            }
            Expr::Value(value) => {
                if !matches!(
                    value.value,
                    Value::Number(_, false)
                        | Value::SingleQuotedString(_)
                        | Value::Boolean(_)
                        | Value::Null
                ) {
                    return Err(deny("This literal type is not supported in read SQL."));
                }
            }
            Expr::Nested(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr)
            | Expr::IsTrue(expr)
            | Expr::IsFalse(expr)
            | Expr::IsNotTrue(expr)
            | Expr::IsNotFalse(expr)
            | Expr::IsUnknown(expr)
            | Expr::IsNotUnknown(expr) => self.expr(expr, scope, depth + 1)?,
            Expr::UnaryOp {
                op: UnaryOperator::Plus | UnaryOperator::Minus | UnaryOperator::Not,
                expr,
            } => self.expr(expr, scope, depth + 1)?,
            Expr::BinaryOp {
                left,
                op:
                    BinaryOperator::Plus
                    | BinaryOperator::Minus
                    | BinaryOperator::Multiply
                    | BinaryOperator::Divide
                    | BinaryOperator::Modulo
                    | BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::And
                    | BinaryOperator::Or
                    | BinaryOperator::StringConcat,
                right,
            } => {
                self.expr(left, scope, depth + 1)?;
                self.expr(right, scope, depth + 1)?;
            }
            Expr::IsDistinctFrom(left, right) | Expr::IsNotDistinctFrom(left, right) => {
                self.expr(left, scope, depth + 1)?;
                self.expr(right, scope, depth + 1)?;
            }
            Expr::Between {
                expr,
                negated: _,
                low,
                high,
            } => {
                self.expr(expr, scope, depth + 1)?;
                self.expr(low, scope, depth + 1)?;
                self.expr(high, scope, depth + 1)?;
            }
            Expr::InList {
                expr,
                list,
                negated: _,
            } => {
                self.expr(expr, scope, depth + 1)?;
                for item in list {
                    self.expr(item, scope, depth + 1)?;
                }
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated: _,
            } => {
                self.expr(expr, scope, depth + 1)?;
                self.query(subquery, scope, depth + 1)?;
            }
            Expr::Subquery(query)
            | Expr::Exists {
                subquery: query,
                negated: _,
            } => self.query(query, scope, depth + 1)?,
            Expr::Like {
                negated: _,
                any,
                expr,
                pattern,
                escape_char,
            }
            | Expr::ILike {
                negated: _,
                any,
                expr,
                pattern,
                escape_char,
            } => {
                if *any {
                    return Err(deny("LIKE ANY is not supported."));
                }
                self.expr(expr, scope, depth + 1)?;
                self.expr(pattern, scope, depth + 1)?;
                if let Some(escape) = escape_char {
                    self.expr(escape, scope, depth + 1)?;
                }
            }
            Expr::Case {
                case_token: _,
                end_token: _,
                operand,
                conditions,
                else_result,
            } => {
                if let Some(operand) = operand {
                    self.expr(operand, scope, depth + 1)?;
                }
                for CaseWhen { condition, result } in conditions {
                    self.expr(condition, scope, depth + 1)?;
                    self.expr(result, scope, depth + 1)?;
                }
                if let Some(result) = else_result {
                    self.expr(result, scope, depth + 1)?;
                }
            }
            Expr::Function(function) => self.function(function, scope, depth + 1)?,
            _ => {
                return Err(deny(
                    "This expression is not supported in read SQL. Custom operators, casts, collations, variables and executable functions are forbidden.",
                ));
            }
        }
        Ok(())
    }
    fn function(
        &mut self,
        function: &Function,
        scope: &HashSet<String>,
        depth: usize,
    ) -> Result<()> {
        let Function {
            name,
            uses_odbc_syntax,
            parameters,
            args,
            within_group,
            filter,
            null_treatment,
            over,
        } = function;
        let parts = self.name(name)?;
        let name = match parts.as_slice() {
            [name] => name,
            [schema, name]
                if self.source.driver == DriverKind::Postgres && schema == "pg_catalog" =>
            {
                name
            }
            _ => {
                return Err(deny(
                    "Only approved built-in functions are allowed in read SQL.",
                ));
            }
        };
        if !SAFE_FUNCTIONS.contains(&name.to_ascii_lowercase().as_str())
            || *uses_odbc_syntax
            || !matches!(parameters, FunctionArguments::None)
            || !within_group.is_empty()
            || null_treatment.is_some()
            || over.is_some()
        {
            return Err(deny(
                "The function or function modifier is not in the read-only built-in allowlist.",
            ));
        }
        let FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: _,
            args,
            clauses,
        }) = args
        else {
            return Err(deny(
                "Only ordinary built-in function argument lists are supported.",
            ));
        };
        if !clauses.is_empty() || args.len() > 16 {
            return Err(deny(
                "Function argument clauses or more than 16 arguments are not supported.",
            ));
        }
        for arg in args {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                    self.expr(expr, scope, depth + 1)?
                }
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                    if name.eq_ignore_ascii_case("count") => {}
                _ => {
                    return Err(deny(
                        "Named function arguments and qualified wildcards are not supported.",
                    ));
                }
            }
        }
        if let Some(filter) = filter {
            self.expr(filter, scope, depth + 1)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pg() -> DataSource {
        DataSource::new(DriverKind::Postgres)
    }
    #[test]
    fn table_write_accepts_only_statements_confined_to_their_target() {
        let orders = TableRef {
            schema: "shop".into(),
            name: "orders".into(),
        };
        for sql in [
            "INSERT INTO shop.orders (id, status) VALUES (1, 'draft'), (2, lower('READY'))",
            "UPDATE shop.orders SET status = 'ready' WHERE id = 42",
            "UPDATE shop.orders o SET total = o.total + 1 WHERE o.id IN (SELECT id FROM shop.orders WHERE status = 'draft')",
            "DELETE FROM shop.orders WHERE status = 'cancelled' RETURNING id",
            "INSERT INTO shop.orders (id, status) SELECT id + 1000, status FROM shop.orders WHERE id < 10",
        ] {
            assert_eq!(table_write_target(&pg(), sql).unwrap().0, orders, "{sql}");
        }
        for sql in [
            "UPDATE shop.orders SET status = 'x' FROM shop.customers c WHERE c.id = 1",
            "DELETE FROM shop.orders WHERE customer_id IN (SELECT id FROM shop.customers)",
            "INSERT INTO shop.orders (id) SELECT id FROM shop.customers",
            "UPDATE shop.orders SET status = evil()",
            "UPDATE shop.orders SET status = public.lower('x')",
            "DELETE FROM orders",
            "INSERT INTO shop.orders (id) VALUES (1) ON CONFLICT (id) DO UPDATE SET id = 2",
            "WITH gone AS (DELETE FROM shop.customers RETURNING id) DELETE FROM shop.orders",
            "UPDATE shop.orders SET status = 'a'; DELETE FROM shop.customers",
            "DROP TABLE shop.orders",
            "TRUNCATE shop.orders",
            "SELECT * FROM shop.orders",
            "DELETE FROM shop.orders o WHERE o IS NULL",
            "UPDATE shop.orders o SET status = o.row_to_json WHERE id = 1",
            "DELETE FROM shop.orders WHERE orders.count > 1",
        ] {
            assert!(table_write_target(&pg(), sql).is_err(), "{sql}");
        }
    }
    #[test]
    fn accepts_joins_ctes_aggregates_subqueries_and_preserves_physical_scope() {
        let plan=analyze(&pg(),"WITH totals AS (SELECT customer_id, sum(total) AS total FROM shop.orders GROUP BY customer_id) SELECT c.name, t.total FROM shop.customers c JOIN totals t ON c.id=t.customer_id WHERE t.total > (SELECT avg(total) FROM shop.orders) ORDER BY t.total DESC LIMIT 20").unwrap();
        assert_eq!(
            plan.tables,
            vec![
                TableRef {
                    schema: "shop".into(),
                    name: "customers".into()
                },
                TableRef {
                    schema: "shop".into(),
                    name: "orders".into()
                }
            ]
        );
        assert!(plan.sql.contains("JOIN totals"));
    }
    #[test]
    fn fails_closed_for_all_executable_escape_forms() {
        for sql in [
            "DELETE FROM shop.orders",
            "WITH x AS (DELETE FROM shop.orders RETURNING *) SELECT * FROM x",
            "SELECT pg_sleep(1)",
            "SELECT public.abs(1)",
            "SELECT * INTO stolen FROM shop.orders",
            "SELECT * FROM shop.orders FOR UPDATE",
            "SELECT * FROM pg_catalog.pg_authid",
            "SELECT 1::public.evil",
            "SELECT 1 OPERATOR(public.+) 2",
            "SELECT name COLLATE public.evil FROM shop.customers",
            "SELECT * FROM public.run_me()",
            "SELECT * FROM orders",
            "SELECT current_user",
            "SELECT 1; SELECT 2",
            "SELECT 1 /*!50000 INTO OUTFILE '/tmp/secret' */",
            "SELECT 1 /*M! INTO OUTFILE '/tmp/secret' */",
            "WITH RECURSIVE x AS (SELECT 1) SELECT * FROM x",
        ] {
            assert!(analyze(&pg(), sql).is_err(), "{sql}");
        }
    }
    #[test]
    fn cte_scope_is_lexical_and_does_not_mask_outside_relations() {
        for sql in [
            "SELECT * FROM (WITH pg_authid AS (SELECT 1) SELECT * FROM pg_authid) q, pg_authid",
            "WITH x AS (SELECT * FROM x) SELECT * FROM x",
            "WITH x AS (SELECT * FROM later), later AS (SELECT 1) SELECT * FROM x",
        ] {
            assert!(analyze(&pg(), sql).is_err(), "{sql}");
        }
        assert!(
            analyze(
                &pg(),
                "WITH x AS (SELECT 1 AS id), y AS (SELECT id FROM x) SELECT * FROM y"
            )
            .is_ok()
        );
    }
    #[test]
    fn rejects_composite_row_values_before_server_type_resolution() {
        for sql in [
            "SELECT round(t,0) FROM shop.items t",
            "SELECT abs(items) FROM shop.items",
            "SELECT round(shop.items,0) FROM shop.items",
            "WITH x AS (SELECT * FROM shop.items) SELECT abs(x) FROM x",
            "SELECT abs(q) FROM (SELECT id FROM shop.items) q",
            "SELECT (SELECT abs(t)) FROM shop.items t",
        ] {
            assert!(analyze(&pg(), sql).is_err(), "{sql}");
        }
        assert!(analyze(&pg(), "SELECT round(t.amount,0) FROM shop.items t").is_ok());
    }
    #[test]
    fn table_write_runs_sql_regenerated_from_the_checked_ast() {
        let (_, sql) = table_write_target(
            &pg(),
            "UPDATE shop.orders /* note */ SET status = 'it''s' -- trailing\nWHERE id = 1",
        )
        .unwrap();
        assert_eq!(sql, "UPDATE shop.orders SET status = 'it''s' WHERE id = 1");
    }
    #[test]
    fn whole_row_function_notation_is_refused_or_left_to_column_checks() {
        for sql in [
            "SELECT q.to_json FROM (SELECT id FROM shop.items) q",
            "WITH c AS (SELECT id FROM shop.items) SELECT c.num_nulls FROM c",
            "WITH c AS (SELECT id FROM shop.items) SELECT x.count FROM c x",
            "SELECT unknown.pg_typeof FROM shop.items",
        ] {
            let failure = analyze(&pg(), sql).err().expect(sql);
            assert!(failure.message.contains("whole row"), "{sql}");
        }
        let items = TableRef {
            schema: "shop".into(),
            name: "items".into(),
        };
        // Names on a physical relation are resolved against its columns by the driver.
        let plan = analyze(
            &pg(),
            "SELECT t.count, shop.items.rank, t.row_to_json FROM shop.items t, shop.items",
        )
        .unwrap();
        assert_eq!(
            plan.pg_columns,
            vec![
                (items.clone(), "count".into()),
                (items.clone(), "rank".into()),
                (items, "row_to_json".into())
            ]
        );
        let plan = analyze(
            &pg(),
            "WITH c AS (SELECT id AS total FROM shop.items) SELECT c.total FROM c",
        )
        .unwrap();
        assert!(plan.pg_columns.is_empty());
        assert_eq!(plan.pg_row_attributes, vec!["total".to_owned()]);
    }
    #[test]
    fn rejects_postgres_identifier_truncation_in_every_name_position() {
        let prefix = "a".repeat(63);
        let long = format!("{prefix}b");
        for sql in [
            format!("SELECT round({prefix},0) FROM shop.items AS {long}"),
            format!("SELECT round({prefix},0) FROM (SELECT id FROM shop.items) AS {long}"),
            format!("WITH {long} AS (SELECT id FROM shop.items) SELECT * FROM {prefix}"),
            format!("SELECT * FROM shop.{long}"),
            format!("SELECT t.{long} FROM shop.items t"),
            format!("SELECT id AS {long} FROM shop.items"),
            format!("SELECT id AS \"{}\" FROM shop.items", "ż".repeat(32)),
        ] {
            assert!(analyze(&pg(), &sql).is_err(), "{sql}");
        }
        assert!(
            analyze(
                &pg(),
                &format!("SELECT id AS \"{}a\" FROM shop.items", "ż".repeat(31))
            )
            .is_ok()
        );
    }
    #[test]
    fn rejects_queries_beyond_input_and_parser_budgets() {
        assert!(analyze(&pg(), &format!("SELECT '{}'", "x".repeat(MAX_SQL_BYTES))).is_err());
        assert!(
            analyze(
                &pg(),
                &format!("SELECT {}1{}", "(".repeat(512), ")".repeat(512))
            )
            .is_err()
        );
    }
}

// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::ops::Deref;
use std::sync::Arc;

use itertools::Itertools;
use risingwave_common::catalog::{
    ColumnDesc, Field, INFORMATION_SCHEMA_SCHEMA_NAME, PG_CATALOG_SCHEMA_NAME,
};
use risingwave_common::error::{ErrorCode, Result, RwError};
use risingwave_common::session_config::USER_NAME_WILD_CARD;
use risingwave_sqlparser::ast::{Statement, TableAlias};
use risingwave_sqlparser::parser::Parser;

use crate::binder::relation::BoundSubquery;
use crate::binder::{Binder, Relation};
use crate::catalog::root_catalog::SchemaPath;
use crate::catalog::source_catalog::SourceCatalog;
use crate::catalog::system_catalog::SystemCatalog;
use crate::catalog::table_catalog::{TableCatalog, TableKind};
use crate::catalog::view_catalog::ViewCatalog;
use crate::catalog::{CatalogError, IndexCatalog, TableId};
use crate::user::UserId;

#[derive(Debug, Clone)]
pub struct BoundBaseTable {
    pub table_id: TableId,
    pub table_catalog: TableCatalog,
    pub table_indexes: Vec<Arc<IndexCatalog>>,
}

/// `BoundTableSource` is used by DML statement on table source like insert, update.
#[derive(Debug)]
pub struct BoundTableSource {
    pub name: String,       // explain-only
    pub source_id: TableId, // TODO: refactor to source id
    pub associated_mview_id: TableId,
    pub columns: Vec<ColumnDesc>,
    pub append_only: bool,
    pub owner: UserId,
}

#[derive(Debug, Clone)]
pub struct BoundSystemTable {
    pub table_id: TableId,
    pub sys_table_catalog: SystemCatalog,
}

#[derive(Debug, Clone)]
pub struct BoundSource {
    pub catalog: SourceCatalog,
}

impl From<&SourceCatalog> for BoundSource {
    fn from(s: &SourceCatalog) -> Self {
        Self { catalog: s.clone() }
    }
}

impl Binder {
    /// Binds table or source, or logical view according to what we get from the catalog.
    pub fn bind_relation_by_name_inner(
        &mut self,
        schema_name: Option<&str>,
        table_name: &str,
        alias: Option<TableAlias>,
    ) -> Result<Relation> {
        fn is_system_schema(schema_name: &str) -> bool {
            schema_name == PG_CATALOG_SCHEMA_NAME || schema_name == INFORMATION_SCHEMA_SCHEMA_NAME
        }

        // define some helper functions converting catalog to bound relation
        let resolve_sys_table_relation = |sys_table_catalog: &SystemCatalog| {
            let table = BoundSystemTable {
                table_id: sys_table_catalog.id(),
                sys_table_catalog: sys_table_catalog.clone(),
            };
            (
                Relation::SystemTable(Box::new(table)),
                sys_table_catalog
                    .columns
                    .iter()
                    .map(|c| (c.is_hidden, Field::from(&c.column_desc)))
                    .collect_vec(),
            )
        };

        let resolve_source_relation = |source_catalog: &SourceCatalog| {
            (
                Relation::Source(Box::new(source_catalog.into())),
                source_catalog
                    .columns
                    .iter()
                    .map(|c| (c.is_hidden, Field::from(&c.column_desc)))
                    .collect_vec(),
            )
        };

        // start to bind
        let (ret, columns) = {
            match schema_name {
                Some(schema_name) => {
                    let schema_path = SchemaPath::Name(schema_name);
                    if is_system_schema(schema_name) {
                        if let Ok(sys_table_catalog) = self.catalog.get_sys_table_by_name(
                            &self.db_name,
                            schema_name,
                            table_name,
                        ) {
                            resolve_sys_table_relation(sys_table_catalog)
                        } else {
                            return Err(ErrorCode::NotImplemented(
                                format!(
                                    r###"{}.{} is not supported, please use `SHOW` commands for now.
`SHOW TABLES`,
`SHOW MATERIALIZED VIEWS`,
`DESCRIBE <table>`,
`SHOW COLUMNS FROM [table]`
"###,
                                    schema_name, table_name
                                ),
                                1695.into(),
                            )
                            .into());
                        }
                    } else if let Ok((table_catalog, schema_name)) =
                        self.catalog
                            .get_table_by_name(&self.db_name, schema_path, table_name)
                    {
                        self.resolve_table_relation(table_catalog, schema_name)?
                    } else if let Ok((source_catalog, _)) =
                        self.catalog
                            .get_source_by_name(&self.db_name, schema_path, table_name)
                    {
                        resolve_source_relation(source_catalog)
                    } else if let Ok((view_catalog, _)) =
                        self.catalog
                            .get_view_by_name(&self.db_name, schema_path, table_name)
                    {
                        self.resolve_view_relation(&view_catalog.clone())?
                    } else {
                        return Err(CatalogError::NotFound(
                            "table or source",
                            table_name.to_string(),
                        )
                        .into());
                    }
                }
                None => (|| {
                    let user_name = &self.auth_context.user_name;

                    for path in self.search_path.path() {
                        if is_system_schema(path) {
                            if let Ok(sys_table_catalog) =
                                self.catalog
                                    .get_sys_table_by_name(&self.db_name, path, table_name)
                            {
                                return Ok(resolve_sys_table_relation(sys_table_catalog));
                            }
                        } else {
                            let schema_name = if path == USER_NAME_WILD_CARD {
                                user_name
                            } else {
                                path
                            };

                            if let Ok(schema) =
                                self.catalog.get_schema_by_name(&self.db_name, schema_name)
                            {
                                if let Some(table_catalog) = schema.get_table_by_name(table_name) {
                                    return self.resolve_table_relation(table_catalog, schema_name);
                                } else if let Some(source_catalog) =
                                    schema.get_source_by_name(table_name)
                                {
                                    return Ok(resolve_source_relation(source_catalog));
                                } else if let Some(view_catalog) =
                                    schema.get_view_by_name(table_name)
                                {
                                    return self.resolve_view_relation(&view_catalog.clone());
                                }
                            }
                        }
                    }

                    Err(CatalogError::NotFound("table or source", table_name.to_string()).into())
                })()?,
            }
        };

        self.bind_table_to_context(columns, table_name.to_string(), alias)?;
        Ok(ret)
    }

    fn resolve_table_relation(
        &self,
        table_catalog: &TableCatalog,
        schema_name: &str,
    ) -> Result<(Relation, Vec<(bool, Field)>)> {
        let table_id = table_catalog.id();
        let table_catalog = table_catalog.clone();
        let columns = table_catalog
            .columns
            .iter()
            .map(|c| (c.is_hidden, Field::from(&c.column_desc)))
            .collect_vec();
        let table_indexes = self.resolve_table_indexes(schema_name, table_id)?;

        let table = BoundBaseTable {
            table_id,
            table_catalog,
            table_indexes,
        };

        Ok::<_, RwError>((Relation::BaseTable(Box::new(table)), columns))
    }

    fn resolve_view_relation(
        &mut self,
        view_catalog: &ViewCatalog,
    ) -> Result<(Relation, Vec<(bool, Field)>)> {
        let ast = Parser::parse_sql(&view_catalog.sql)
            .expect("a view's sql should be parsed successfully");
        assert!(ast.len() == 1, "a view should contain only one statement");
        let query = match ast.into_iter().next().unwrap() {
            Statement::Query(q) => q,
            _ => unreachable!("a view should contain a query statement"),
        };
        let query = self.bind_query(*query).map_err(|e| {
            ErrorCode::BindError(format!(
                "failed to bind view {}, sql: {}\nerror: {}",
                view_catalog.name, view_catalog.sql, e
            ))
        })?;
        let columns = view_catalog.columns.clone();
        Ok((
            Relation::Subquery(Box::new(BoundSubquery { query })),
            columns.iter().map(|c| (false, c.clone())).collect_vec(),
        ))
    }

    fn resolve_table_indexes(
        &self,
        schema_name: &str,
        table_id: TableId,
    ) -> Result<Vec<Arc<IndexCatalog>>> {
        Ok(self
            .catalog
            .get_schema_by_name(&self.db_name, schema_name)?
            .get_indexes_by_table_id(&table_id))
    }

    pub(crate) fn bind_table(
        &mut self,
        schema_name: Option<&str>,
        table_name: &str,
        alias: Option<TableAlias>,
    ) -> Result<BoundBaseTable> {
        let db_name = &self.db_name;
        let schema_path = match schema_name {
            Some(schema_name) => SchemaPath::Name(schema_name),
            None => SchemaPath::Path(&self.search_path, &self.auth_context.user_name),
        };
        let (table_catalog, schema_name) =
            self.catalog
                .get_table_by_name(db_name, schema_path, table_name)?;
        let table_catalog = table_catalog.deref().clone();

        let table_id = table_catalog.id();
        let table_indexes = self.resolve_table_indexes(schema_name, table_id)?;

        let columns = table_catalog.columns.clone();

        self.bind_table_to_context(
            columns
                .iter()
                .map(|c| (c.is_hidden, (&c.column_desc).into())),
            table_name.to_string(),
            alias,
        )?;

        Ok(BoundBaseTable {
            table_id,
            table_catalog,
            table_indexes,
        })
    }

    pub(crate) fn bind_table_source(
        &mut self,
        schema_name: Option<&str>,
        source_name: &str,
    ) -> Result<BoundTableSource> {
        let db_name = &self.db_name;
        let schema_path = match schema_name {
            Some(schema_name) => SchemaPath::Name(schema_name),
            None => SchemaPath::Path(&self.search_path, &self.auth_context.user_name),
        };
        let (associate_table, schema_name) =
            self.catalog
                .get_table_by_name(db_name, schema_path, source_name)?;
        match associate_table.kind() {
            TableKind::TableOrSource => {}
            TableKind::Index => {
                return Err(ErrorCode::InvalidInputSyntax(format!(
                    "cannot change index \"{source_name}\""
                ))
                .into())
            }
            TableKind::MView => {
                return Err(ErrorCode::InvalidInputSyntax(format!(
                    "cannot change materialized view \"{source_name}\""
                ))
                .into())
            }
        }
        let associate_table_id = associate_table.id();

        let (source, _) = self.catalog.get_source_by_name(
            &self.db_name,
            SchemaPath::Name(schema_name),
            source_name,
        )?;

        let source_id = TableId::new(source.id);

        let append_only = source.append_only;
        let columns = source
            .columns
            .iter()
            .filter(|c| !c.is_hidden)
            .map(|c| c.column_desc.clone())
            .collect();

        let owner = source.owner;

        // Note(bugen): do not bind context here.

        Ok(BoundTableSource {
            name: source_name.to_string(),
            source_id,
            associated_mview_id: associate_table_id,
            columns,
            append_only,
            owner,
        })
    }
}

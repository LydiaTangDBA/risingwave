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

use std::collections::HashSet;
use std::sync::Arc;

use itertools::Itertools;
use pgwire::pg_field_descriptor::PgFieldDescriptor;
use pgwire::pg_response::{PgResponse, StatementType};
use pgwire::types::Row;
use risingwave_common::catalog::ColumnDesc;
use risingwave_common::error::Result;
use risingwave_common::types::DataType;
use risingwave_sqlparser::ast::{display_comma_separated, ObjectName};

use super::RwPgResponse;
use crate::binder::{Binder, Relation};
use crate::catalog::{CatalogError, IndexCatalog};
use crate::handler::util::col_descs_to_rows;
use crate::session::OptimizerContext;

pub fn handle_describe(context: OptimizerContext, table_name: ObjectName) -> Result<RwPgResponse> {
    let session = context.session_ctx;
    let mut binder = Binder::new(&session);
    let relation = binder.bind_relation_by_name(table_name.clone(), None)?;
    // For Source, it doesn't have table catalog so use get source to get column descs.
    let (columns, indices): (Vec<ColumnDesc>, Vec<Arc<IndexCatalog>>) = {
        let (catalogs, indices) = match relation {
            Relation::Source(s) => (s.catalog.columns, vec![]),
            Relation::BaseTable(t) => (t.table_catalog.columns, t.table_indexes),
            Relation::SystemTable(t) => (t.sys_table_catalog.columns, vec![]),
            _ => {
                return Err(
                    CatalogError::NotFound("table or source", table_name.to_string()).into(),
                );
            }
        };
        (
            catalogs
                .iter()
                .filter(|c| !c.is_hidden)
                .map(|c| c.column_desc.clone())
                .collect(),
            indices,
        )
    };

    // Convert all column descs to rows
    let mut rows = col_descs_to_rows(columns);

    // Convert all indexes to rows
    rows.extend(indices.iter().map(|index| {
        let index_table = index.index_table.clone();

        let index_columns = index_table
            .pk
            .iter()
            .filter(|x| !index_table.columns[x.index].is_hidden)
            .map(|x| index_table.columns[x.index].name().to_string())
            .collect_vec();

        let pk_column_index_set = index_table
            .pk
            .iter()
            .map(|x| x.index)
            .collect::<HashSet<_>>();

        let include_columns = index_table
            .columns
            .iter()
            .enumerate()
            .filter(|(i, _)| !pk_column_index_set.contains(i))
            .filter(|(_, x)| !x.is_hidden)
            .map(|(_, x)| x.name().to_string())
            .collect_vec();

        let distributed_by_columns = index_table
            .distribution_key
            .iter()
            .map(|&x| index_table.columns[x].name().to_string())
            .collect_vec();

        Row::new(vec![
            Some(index.name.clone().into()),
            if include_columns.is_empty() {
                Some(
                    format!(
                        "index({}) distributed by({})",
                        display_comma_separated(&index_columns),
                        display_comma_separated(&distributed_by_columns),
                    )
                    .into(),
                )
            } else {
                Some(
                    format!(
                        "index({}) include({}) distributed by({})",
                        display_comma_separated(&index_columns),
                        display_comma_separated(&include_columns),
                        display_comma_separated(&distributed_by_columns),
                    )
                    .into(),
                )
            },
        ])
    }));

    // TODO: recover the original user statement
    Ok(PgResponse::new_for_stream(
        StatementType::DESCRIBE_TABLE,
        Some(rows.len() as i32),
        rows.into(),
        vec![
            PgFieldDescriptor::new(
                "Name".to_owned(),
                DataType::VARCHAR.to_oid(),
                DataType::VARCHAR.type_len(),
            ),
            PgFieldDescriptor::new(
                "Type".to_owned(),
                DataType::VARCHAR.to_oid(),
                DataType::VARCHAR.type_len(),
            ),
        ],
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ops::Index;

    use futures_async_stream::for_await;

    use crate::test_utils::LocalFrontend;

    #[tokio::test]
    async fn test_describe_handler() {
        let frontend = LocalFrontend::new(Default::default()).await;
        frontend
            .run_sql("create table t (v1 int, v2 int);")
            .await
            .unwrap();

        frontend
            .run_sql("create index idx1 on t (v1,v2);")
            .await
            .unwrap();

        let sql = "describe t";
        let mut pg_response = frontend.run_sql(sql).await.unwrap();

        let mut columns = HashMap::new();
        #[for_await]
        for row_set in pg_response.values_stream() {
            let row_set = row_set.unwrap();
            for row in row_set {
                columns.insert(
                    std::str::from_utf8(row.index(0).as_ref().unwrap())
                        .unwrap()
                        .to_string(),
                    std::str::from_utf8(row.index(1).as_ref().unwrap())
                        .unwrap()
                        .to_string(),
                );
            }
        }

        let expected_columns: HashMap<String, String> = maplit::hashmap! {
            "v1".into() => "Int32".into(),
            "v2".into() => "Int32".into(),
            "idx1".into() => "index(v1, v2) distributed by(v1, v2)".into(),
        };

        assert_eq!(columns, expected_columns);
    }
}

// Copyright 2024 Zinc Labs Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::{str::FromStr, sync::Arc};

use arrow_schema::Field;
use config::{
    get_config,
    meta::{
        search::{SearchType, Session as SearchSession, StorageType},
        stream::{FileKey, FileMeta, StreamType},
    },
    utils::{parquet::new_parquet_writer, schema_ext::SchemaExt},
    PARQUET_BATCH_SIZE,
};
use datafusion::{
    arrow::{
        datatypes::{DataType, Schema},
        record_batch::RecordBatch,
    },
    common::{tree_node::TreeNode, Column},
    datasource::{
        empty::EmptyTable,
        file_format::{json::JsonFormat, parquet::ParquetFormat},
        listing::{ListingOptions, ListingTableConfig, ListingTableUrl},
        object_store::{DefaultObjectStoreRegistry, ObjectStoreRegistry},
    },
    error::{DataFusionError, Result},
    execution::{
        cache::cache_manager::CacheManagerConfig,
        context::SessionConfig,
        memory_pool::{FairSpillPool, GreedyMemoryPool},
        runtime_env::{RuntimeConfig, RuntimeEnv},
        session_state::SessionStateBuilder,
    },
    physical_plan::{collect, collect_partitioned},
    prelude::{Expr, SessionContext},
};
use hashbrown::HashMap;
#[cfg(feature = "enterprise")]
use o2_enterprise::enterprise::{common::infra::config::O2_CONFIG, search::WorkGroup};

use super::{
    file_type::{FileType, GetExt},
    storage::file_list,
    table_provider::{memtable::NewMemTable, NewListingTable},
    udf::transform_udf::get_all_transform,
};
use crate::service::search::{
    datafusion::{plan::UpdateOffsetExec, ExtLimit},
    sql::{Sql, RE_SELECT_WILDCARD},
};

const DATAFUSION_MIN_MEM: usize = 1024 * 1024 * 256; // 256MB
const DATAFUSION_MIN_PARTITION: usize = 2; // CPU cores

pub async fn sql(
    session: &SearchSession,
    schema: Arc<Schema>,
    rules: HashMap<String, DataType>,
    sql: &Arc<Sql>,
    files: &[FileKey],
    in_records_batches: Option<Vec<RecordBatch>>,
    file_type: FileType,
) -> Result<Vec<Vec<RecordBatch>>> {
    if files.is_empty() && in_records_batches.is_none() {
        return Ok(vec![]);
    }

    let start = std::time::Instant::now();
    let trace_id = session.id.clone();
    let mut ctx = if !file_type.eq(&FileType::ARROW) {
        register_table(
            session,
            schema.clone(),
            "tbl",
            files,
            file_type.clone(),
            rules.clone(),
            false,
            &sql.meta.order_by,
            Some(sql.meta.limit as usize),
        )
        .await?
    } else {
        let ctx = prepare_datafusion_context(
            session.work_group.clone(),
            &session.search_type,
            false,
            false,
            session.target_partitions,
            None,
        )
        .await?;
        let record_batches = in_records_batches.unwrap();
        let mem_table = Arc::new(NewMemTable::try_new(
            schema.clone(),
            vec![record_batches],
            rules.clone(),
        )?);
        // Register the MemTable as a table in the DataFusion context
        ctx.register_table("tbl", mem_table)?;
        ctx
    };

    // register UDF
    register_udf(&mut ctx, &sql.org_id).await;

    // query sql
    let result = exec_query(&ctx, session, sql).await?;

    // drop table
    ctx.deregister_table("tbl")?;
    log::info!(
        "[trace_id {trace_id}] Query all took {} ms",
        start.elapsed().as_millis()
    );

    Ok(result)
}

async fn exec_query(
    ctx: &SessionContext,
    session: &SearchSession,
    sql: &Arc<Sql>,
) -> Result<Vec<Vec<RecordBatch>>> {
    let sql: &str = sql.origin_sql.as_ref();
    let start = std::time::Instant::now();
    let trace_id = session.id.clone();
    let cfg = get_config();

    // Debug SQL
    if cfg.common.print_key_sql {
        log::info!("[trace_id {trace_id}] Query sql: {}", sql);
    }

    // Hack for limit 0
    if sql.ends_with("LIMIT 0") {
        return Ok(vec![]);
    }

    let plan = ctx.state().create_logical_plan(sql).await?;
    if cfg.common.print_key_sql {
        println!("+---------------------------+----------+");
        println!("logic plan");
        println!("+---------------------------+----------+");
        println!("{:?}", plan);
    }
    let physical_plan = ctx.state().create_physical_plan(&plan).await?;
    if cfg.common.print_key_sql {
        let plan = datafusion::physical_plan::displayable(physical_plan.as_ref())
            .set_show_schema(false)
            .indent(true)
            .to_string();
        println!("+---------------------------+----------+");
        println!("physical plan");
        println!("+---------------------------+----------+");
        println!("{}", plan);
    }

    let partial_paln = match super::plan::get_partial_plan(&physical_plan)? {
        Some(plan) => plan,
        None => super::plan::get_empty_partial_plan(&physical_plan),
    };
    if cfg.common.print_key_sql {
        let plan = datafusion::physical_plan::displayable(partial_paln.as_ref())
            .set_show_schema(false)
            .indent(false)
            .to_string();
        println!("+---------------------------+----------+");
        println!("partial plan");
        println!("+---------------------------+----------+");
        println!("{}", plan);
    }

    let data = match collect_partitioned(partial_paln, ctx.task_ctx()).await {
        Ok(v) => v,
        Err(e) => {
            log::error!(
                "[trace_id {trace_id}] query sql execute failed, session: {:?}, sql: {}, err: {:?}",
                session,
                sql,
                e
            );
            return Err(e);
        }
    };

    log::info!(
        "[trace_id {trace_id}] Query took {} ms",
        start.elapsed().as_millis()
    );
    Ok(data)
}

#[allow(clippy::too_many_arguments)]
pub async fn merge_partitions(
    org_id: &str,
    offset: i64,
    limit: i64,
    sql: &str,
    schema: Arc<Schema>,
    batches: Vec<RecordBatch>,
) -> Result<Vec<RecordBatch>> {
    if batches.is_empty() {
        return Ok(vec![]);
    }

    let cfg = get_config();
    // repartition batches
    let chunk_size = std::cmp::max(1, (batches.len() + cfg.limit.cpu_num) / cfg.limit.cpu_num);
    let mut new_batches: Vec<Vec<RecordBatch>> = Vec::with_capacity(batches.len() / chunk_size);
    for batch in batches {
        if new_batches.last().is_none() || new_batches.last().unwrap().len() >= chunk_size {
            let mut v = Vec::with_capacity(chunk_size);
            v.push(batch);
            new_batches.push(v);
        } else {
            new_batches.last_mut().unwrap().push(batch);
        }
    }
    let batches = new_batches;

    let mut ctx =
        prepare_datafusion_context(None, &SearchType::Normal, false, false, 0, None).await?;

    // register UDF
    register_udf(&mut ctx, org_id).await;

    // Debug SQL
    if cfg.common.print_key_sql {
        log::info!("Merge sql: {sql}");
    }

    let memtable = Arc::new(EmptyTable::new(schema).with_partitions(cfg.limit.cpu_num));
    ctx.register_table("tbl", memtable)?;

    let plan = ctx.state().create_logical_plan(sql).await?;
    if cfg.common.print_key_sql {
        println!("+---------------------------+----------+");
        println!("logic plan");
        println!("+---------------------------+----------+");
        println!("{:?}", plan);
    }
    let physical_plan = ctx.state().create_physical_plan(&plan).await?;
    if cfg.common.print_key_sql {
        let plan = datafusion::physical_plan::displayable(physical_plan.as_ref())
            .set_show_schema(false)
            .indent(true)
            .to_string();
        println!("+---------------------------+----------+");
        println!("physical plan");
        println!("+---------------------------+----------+");
        println!("{}", plan);
    }

    let final_plan = match super::plan::get_final_plan(&physical_plan, &batches) {
        Ok((Some(plan), v)) => {
            if v {
                plan
            } else {
                super::plan::get_empty_final_plan(
                    &plan,
                    &batches,
                    ctx.state().config().batch_size(),
                )
            }
        }
        _ => physical_plan.clone(),
    };
    let mut update_offset_rewriter = UpdateOffsetExec::new(offset as usize, limit as usize);
    let final_plan = final_plan.rewrite(&mut update_offset_rewriter)?.data;
    if cfg.common.print_key_sql {
        let plan = datafusion::physical_plan::displayable(final_plan.as_ref())
            .set_show_schema(false)
            .indent(true)
            .to_string();
        println!("+---------------------------+----------+");
        println!("final plan");
        println!("+---------------------------+----------+");
        println!("{}", plan);
        println!("+---------------------------+----------+");
    }
    let data = collect(final_plan, ctx.task_ctx()).await?;
    Ok(data)
}

pub async fn convert_parquet_file(
    trace_id: &str,
    buf: &mut Vec<u8>,
    schema: Arc<Schema>,
    bloom_filter_fields: &[String],
    full_text_search_fields: &[String],
    rules: HashMap<String, DataType>,
    file_type: FileType,
) -> Result<()> {
    let cfg = get_config();
    let start = std::time::Instant::now();

    let query_sql = format!(
        "SELECT * FROM tbl ORDER BY {} DESC",
        cfg.common.column_timestamp
    ); //  select_wildcard -> without_optimizer

    // query data
    let ctx = prepare_datafusion_context(None, &SearchType::Normal, true, false, 0, None).await?;

    // Configure listing options
    let listing_options = match file_type {
        FileType::PARQUET => {
            let file_format = ParquetFormat::default();
            ListingOptions::new(Arc::new(file_format))
                .with_file_extension(FileType::PARQUET.get_ext())
                .with_target_partitions(ctx.state().config().target_partitions())
        }
        FileType::JSON => {
            let file_format = JsonFormat::default();
            ListingOptions::new(Arc::new(file_format))
                .with_file_extension(FileType::JSON.get_ext())
                .with_target_partitions(ctx.state().config().target_partitions())
        }
        _ => {
            return Err(DataFusionError::Execution(format!(
                "Unsupported file type scheme {file_type:?}",
            )));
        }
    };

    let prefix = match ListingTableUrl::parse(format!("tmpfs:///{trace_id}/")) {
        Ok(url) => url,
        Err(e) => {
            return Err(datafusion::error::DataFusionError::Execution(format!(
                "ListingTableUrl error: {e}"
            )));
        }
    };

    let config = ListingTableConfig::new(prefix)
        .with_listing_options(listing_options)
        .with_schema(schema.clone());

    let table = NewListingTable::try_new(config, rules)?;
    ctx.register_table("tbl", Arc::new(table))?;

    // get all sorted data
    let df = match ctx.sql(&query_sql).await {
        Ok(df) => df,
        Err(e) => {
            log::error!(
                "convert sql execute failed, sql: {}, err: {:?}",
                query_sql,
                e
            );
            return Err(e);
        }
    };
    let schema: Schema = df.schema().into();
    let schema = Arc::new(schema);
    let batches = df.collect().await?;
    let file_meta = FileMeta::default();
    let mut writer = new_parquet_writer(
        buf,
        &schema,
        bloom_filter_fields,
        full_text_search_fields,
        &file_meta,
    );
    for batch in batches {
        writer.write(&batch).await?;
    }
    writer.close().await?;
    ctx.deregister_table("tbl")?;
    drop(ctx);

    log::info!(
        "convert_parquet_file took {} ms",
        start.elapsed().as_millis()
    );

    Ok(())
}

pub async fn merge_parquet_files(
    trace_id: &str,
    stream_type: StreamType,
    stream_name: &str,
    schema: Arc<Schema>,
) -> Result<(Arc<Schema>, Vec<RecordBatch>)> {
    let start = std::time::Instant::now();
    let cfg = get_config();

    // get all sorted data
    let query_sql = if stream_type == StreamType::Index {
        // TODO: NOT IN is not efficient, need to optimize it: NOT EXIST
        format!(
            "SELECT * FROM tbl WHERE file_name NOT IN (SELECT file_name FROM tbl WHERE deleted is True) ORDER BY {} DESC",
            cfg.common.column_timestamp
        )
    } else if cfg.limit.distinct_values_hourly
        && stream_type == StreamType::Metadata
        && stream_name == "distinct_values"
    {
        format!(
            "SELECT MIN({}) AS {}, SUM(count) as count, field_name, field_value, filter_name, filter_value, stream_name, stream_type FROM tbl GROUP BY field_name, field_value, filter_name, filter_value, stream_name, stream_type ORDER BY {} DESC",
            cfg.common.column_timestamp, cfg.common.column_timestamp, cfg.common.column_timestamp
        )
    } else {
        format!(
            "SELECT * FROM tbl ORDER BY {} DESC",
            cfg.common.column_timestamp
        )
    };

    // create datafusion context
    let select_wildcard = RE_SELECT_WILDCARD.is_match(query_sql.as_str());
    let without_optimizer = select_wildcard && stream_type != StreamType::Index;
    let ctx =
        prepare_datafusion_context(None, &SearchType::Normal, without_optimizer, false, 0, None)
            .await?;

    // Configure listing options
    let file_format = ParquetFormat::default();
    let listing_options = ListingOptions::new(Arc::new(file_format))
        .with_file_extension(FileType::PARQUET.get_ext())
        .with_target_partitions(ctx.state().config().target_partitions());
    let prefix = ListingTableUrl::parse(format!("tmpfs:///{trace_id}/"))?;
    let config = ListingTableConfig::new(prefix)
        .with_listing_options(listing_options)
        .with_schema(schema.clone());
    let table = Arc::new(NewListingTable::try_new(config, HashMap::default())?);
    ctx.register_table("tbl", table.clone())?;

    let df = ctx.sql(&query_sql).await?;
    let schema: Schema = df.schema().into();
    let schema = Arc::new(schema);
    let batches = df.collect().await?;

    ctx.deregister_table("tbl")?;
    drop(ctx);

    log::info!(
        "merge_parquet_files took {} ms",
        start.elapsed().as_millis()
    );

    Ok((schema, batches))
}

pub fn create_session_config(
    search_type: &SearchType,
    sort_by_timestamp_desc: bool,
    target_partitions: usize,
    limit: Option<usize>,
) -> Result<SessionConfig> {
    let cfg = get_config();
    let target_partitions = if target_partitions == 0 {
        cfg.limit.cpu_num
    } else {
        std::cmp::max(DATAFUSION_MIN_PARTITION, target_partitions)
    };
    let mut config = SessionConfig::from_env()?
        .with_batch_size(PARQUET_BATCH_SIZE)
        .with_target_partitions(target_partitions)
        .with_information_schema(true);
    config = config.set_bool(
        "datafusion.execution.listing_table_ignore_subdirectory",
        false,
    );
    config = config.set_str("datafusion.sql_parser.dialect", "postgres");
    if search_type == &SearchType::Normal {
        config = config.set_bool("datafusion.execution.parquet.pushdown_filters", true);
        config = config.set_bool("datafusion.execution.parquet.reorder_filters", true);
    }
    if cfg.common.bloom_filter_enabled {
        config = config.set_bool("datafusion.execution.parquet.bloom_filter_on_read", true);
    }
    if cfg.common.bloom_filter_disabled_on_search {
        config = config.set_bool("datafusion.execution.parquet.bloom_filter_on_read", false);
    }
    if sort_by_timestamp_desc {
        println!("---------------- sort_by_timestamp_desc ----------------");
        config = config.set_bool("datafusion.execution.split_file_groups_by_statistics", true);
        config = config.with_round_robin_repartition(false);
        config = config.with_coalesce_batches(false);
        if let Some(limit) = limit {
            config = config.with_coalesce_batches(limit == 0 || limit >= PARQUET_BATCH_SIZE);
            config.set_extension::<ExtLimit>(Arc::new(ExtLimit(limit)));
        }
    }
    Ok(config)
}

pub async fn create_runtime_env(memory_limit: usize) -> Result<RuntimeEnv> {
    let object_store_registry = DefaultObjectStoreRegistry::new();

    let memory = super::storage::memory::FS::new();
    let memory_url = url::Url::parse("memory:///").unwrap();
    object_store_registry.register_store(&memory_url, Arc::new(memory));

    let wal = super::storage::wal::FS::new();
    let wal_url = url::Url::parse("wal:///").unwrap();
    object_store_registry.register_store(&wal_url, Arc::new(wal));

    let tmpfs = super::storage::tmpfs::Tmpfs::new();
    let tmpfs_url = url::Url::parse("tmpfs:///").unwrap();
    object_store_registry.register_store(&tmpfs_url, Arc::new(tmpfs));

    let cfg = get_config();
    let mut rn_config =
        RuntimeConfig::new().with_object_store_registry(Arc::new(object_store_registry));
    if cfg.limit.datafusion_file_stat_cache_max_entries > 0 {
        let cache_config = CacheManagerConfig::default();
        let cache_config = cache_config.with_files_statistics_cache(Some(
            super::storage::file_statistics_cache::GLOBAL_CACHE.clone(),
        ));
        rn_config = rn_config.with_cache_manager(cache_config);
    }

    let memory_size = std::cmp::max(DATAFUSION_MIN_MEM, memory_limit);
    let mem_pool = super::MemoryPoolType::from_str(&cfg.memory_cache.datafusion_memory_pool)
        .map_err(|e| {
            DataFusionError::Execution(format!("Invalid datafusion memory pool type: {}", e))
        })?;
    match mem_pool {
        super::MemoryPoolType::Greedy => {
            rn_config = rn_config.with_memory_pool(Arc::new(GreedyMemoryPool::new(memory_size)))
        }
        super::MemoryPoolType::Fair => {
            rn_config = rn_config.with_memory_pool(Arc::new(FairSpillPool::new(memory_size)))
        }
        super::MemoryPoolType::None => {}
    };
    RuntimeEnv::new(rn_config)
}

pub async fn prepare_datafusion_context(
    _work_group: Option<String>,
    search_type: &SearchType,
    without_optimizer: bool,
    sort_by_timestamp_desc: bool,
    target_partitions: usize,
    limit: Option<usize>,
) -> Result<SessionContext, DataFusionError> {
    let cfg = get_config();
    #[cfg(not(feature = "enterprise"))]
    let (memory_size, target_partition) = (cfg.memory_cache.datafusion_max_size, target_partitions);
    #[cfg(feature = "enterprise")]
    let (mut memory_size, mut target_partition) =
        (cfg.memory_cache.datafusion_max_size, target_partitions);
    #[cfg(feature = "enterprise")]
    if let Some(wg) = _work_group {
        if let Ok(wg) = WorkGroup::from_str(&wg) {
            let (cpu, mem) = wg.get_dynamic_resource().await.map_err(|e| {
                DataFusionError::Execution(format!("Failed to get dynamic resource: {}", e))
            })?;
            if O2_CONFIG.search_group.cpu_limit_enabled {
                target_partition = target_partition * cpu as usize / 100;
            }
            memory_size = memory_size * mem as usize / 100;
            log::debug!(
                "[datafusion:{}] target_partition: {}, memory_size: {}",
                wg,
                target_partition,
                memory_size
            );
        }
    }

    let session_config =
        create_session_config(search_type, sort_by_timestamp_desc, target_partition, limit)?;
    let runtime_env = Arc::new(create_runtime_env(memory_size).await?);
    if without_optimizer {
        let state = SessionStateBuilder::new()
            .with_config(session_config)
            .with_runtime_env(runtime_env)
            .with_optimizer_rules(vec![])
            .with_analyzer_rules(vec![])
            .build();
        Ok(SessionContext::new_with_state(state))
    } else {
        Ok(SessionContext::new_with_config_rt(
            session_config,
            runtime_env,
        ))
    }
}

async fn register_udf(ctx: &mut SessionContext, _org_id: &str) {
    ctx.register_udf(super::udf::match_udf::MATCH_UDF.clone());
    ctx.register_udf(super::udf::match_udf::MATCH_IGNORE_CASE_UDF.clone());
    ctx.register_udf(super::udf::regexp_udf::REGEX_MATCH_UDF.clone());
    ctx.register_udf(super::udf::regexp_udf::REGEX_NOT_MATCH_UDF.clone());
    ctx.register_udf(super::udf::regexp_udf::REGEXP_MATCH_TO_FIELDS_UDF.clone());
    ctx.register_udf(super::udf::time_range_udf::TIME_RANGE_UDF.clone());
    ctx.register_udf(super::udf::date_format_udf::DATE_FORMAT_UDF.clone());
    ctx.register_udf(super::udf::string_to_array_v2_udf::STRING_TO_ARRAY_V2_UDF.clone());
    ctx.register_udf(super::udf::arrzip_udf::ARR_ZIP_UDF.clone());
    ctx.register_udf(super::udf::arrindex_udf::ARR_INDEX_UDF.clone());
    ctx.register_udf(super::udf::arr_descending_udf::ARR_DESCENDING_UDF.clone());
    ctx.register_udf(super::udf::arrjoin_udf::ARR_JOIN_UDF.clone());
    ctx.register_udf(super::udf::arrcount_udf::ARR_COUNT_UDF.clone());
    ctx.register_udf(super::udf::arrsort_udf::ARR_SORT_UDF.clone());
    ctx.register_udf(super::udf::cast_to_arr_udf::CAST_TO_ARR_UDF.clone());
    ctx.register_udf(super::udf::spath_udf::SPATH_UDF.clone());
    ctx.register_udf(super::udf::to_arr_string_udf::TO_ARR_STRING.clone());

    {
        let udf_list = get_all_transform(_org_id).await;
        for udf in udf_list {
            ctx.register_udf(udf.clone());
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn register_table(
    session: &SearchSession,
    schema: Arc<Schema>,
    table_name: &str,
    files: &[FileKey],
    file_type: FileType,
    rules: HashMap<String, DataType>,
    without_optimizer: bool,
    sort_key: &[(String, bool)],
    limit: Option<usize>,
) -> Result<SessionContext> {
    let cfg = get_config();
    // only sort by timestamp desc
    let sort_by_timestamp_desc =
        sort_key.len() == 1 && sort_key[0].0 == cfg.common.column_timestamp && sort_key[0].1;

    let ctx = prepare_datafusion_context(
        session.work_group.clone(),
        &session.search_type,
        without_optimizer,
        sort_by_timestamp_desc,
        session.target_partitions,
        limit,
    )
    .await?;

    // Configure listing options
    let mut listing_options = match file_type {
        FileType::PARQUET => {
            let file_format = ParquetFormat::default();
            ListingOptions::new(Arc::new(file_format))
                .with_file_extension(FileType::PARQUET.get_ext())
                .with_target_partitions(ctx.state().config().target_partitions())
                .with_collect_stat(true)
        }
        FileType::JSON => {
            let file_format = JsonFormat::default();
            ListingOptions::new(Arc::new(file_format))
                .with_file_extension(FileType::JSON.get_ext())
                .with_target_partitions(ctx.state().config().target_partitions())
                .with_collect_stat(true)
        }
        _ => {
            return Err(DataFusionError::Execution(format!(
                "Unsupported file type scheme {file_type:?}",
            )));
        }
    };

    if sort_by_timestamp_desc {
        // specify sort columns for parquet file
        listing_options = listing_options.with_file_sort_order(vec![vec![Expr::Sort(
            datafusion::logical_expr::SortExpr {
                expr: Box::new(Expr::Column(Column::new_unqualified(
                    cfg.common.column_timestamp.clone(),
                ))),
                asc: false,
                nulls_first: false,
            },
        )]]);
    }

    let schema_key = schema.hash_key();
    let prefix = if session.storage_type == StorageType::Memory {
        file_list::set(&session.id, &schema_key, files).await;
        format!("memory:///{}/schema={}/", session.id, schema_key)
    } else if session.storage_type == StorageType::Wal {
        file_list::set(&session.id, &schema_key, files).await;
        format!("wal:///{}/schema={}/", session.id, schema_key)
    } else if session.storage_type == StorageType::Tmpfs {
        format!("tmpfs:///{}/", session.id)
    } else {
        return Err(DataFusionError::Execution(format!(
            "Unsupported file type scheme {file_type:?}",
        )));
    };
    let prefix = match ListingTableUrl::parse(prefix) {
        Ok(url) => url,
        Err(e) => {
            return Err(datafusion::error::DataFusionError::Execution(format!(
                "ListingTableUrl error: {e}",
            )));
        }
    };

    let mut config = ListingTableConfig::new(prefix).with_listing_options(listing_options);
    if cfg.common.feature_query_infer_schema
        && (cfg.limit.query_optimization_num_fields > 0
            && schema.fields().len() > cfg.limit.query_optimization_num_fields)
    {
        config = config.infer_schema(&ctx.state()).await?;
    } else {
        let timestamp_field = schema.field_with_name(&cfg.common.column_timestamp);
        let schema = if timestamp_field.is_ok() && timestamp_field.unwrap().is_nullable() {
            let new_fields = schema
                .fields()
                .iter()
                .map(|x| {
                    if x.name() == &cfg.common.column_timestamp {
                        Arc::new(Field::new(
                            cfg.common.column_timestamp.clone(),
                            DataType::Int64,
                            false,
                        ))
                    } else {
                        x.clone()
                    }
                })
                .collect::<Vec<_>>();
            Arc::new(Schema::new(new_fields))
        } else {
            schema
        };
        config = config.with_schema(schema);
    }
    let mut table = NewListingTable::try_new(config, rules)?;
    if session.storage_type != StorageType::Tmpfs {
        table = table.with_cache(ctx.runtime_env().cache_manager.get_file_statistic_cache());
    }
    ctx.register_table(table_name, Arc::new(table))?;

    Ok(ctx)
}

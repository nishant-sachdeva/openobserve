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

use std::str::FromStr;

use chrono::{Duration, FixedOffset, Utc};
use config::{
    get_config,
    meta::{
        stream::StreamType,
        usage::{TriggerData, TriggerDataStatus, TriggerDataType},
    },
    utils::json,
};
use cron::Schedule;
use futures::future::try_join_all;
use proto::cluster_rpc;

use crate::{
    common::meta::{alerts::FrequencyType, dashboards::reports::ReportFrequencyType},
    service::{
        alerts::alert::{get_alert_start_end_time, get_row_column_map},
        db,
        ingestion::ingestion_service,
        usage::publish_triggers_usage,
    },
};

pub async fn run() -> Result<(), anyhow::Error> {
    log::debug!("Pulling jobs from scheduler");
    let cfg = get_config();
    // Scheduler pulls only those triggers that match the conditions-
    // - trigger.next_run_at <= now
    // - !(trigger.is_realtime && !trigger.is_silenced)
    // - trigger.status == "Waiting"
    let triggers = db::scheduler::pull(
        cfg.limit.alert_schedule_concurrency,
        cfg.limit.alert_schedule_timeout,
        cfg.limit.report_schedule_timeout,
    )
    .await?;

    log::info!("Pulled {} jobs from scheduler", triggers.len());

    let mut tasks = Vec::new();
    for trigger in triggers {
        let task = tokio::task::spawn(async move {
            if let Err(e) = handle_triggers(trigger).await {
                log::error!("[SCHEDULER] Error handling trigger: {}", e);
            }
        });
        tasks.push(task);
    }
    if let Err(e) = try_join_all(tasks).await {
        log::error!("[SCHEDULER] Error handling triggers: {}", e);
    }
    Ok(())
}

pub async fn handle_triggers(trigger: db::scheduler::Trigger) -> Result<(), anyhow::Error> {
    match trigger.module {
        db::scheduler::TriggerModule::Report => handle_report_triggers(trigger).await,
        db::scheduler::TriggerModule::Alert => handle_alert_triggers(trigger).await,
        db::scheduler::TriggerModule::DerivedStream => {
            handle_derived_stream_triggers(trigger).await
        }
    }
}

async fn handle_alert_triggers(trigger: db::scheduler::Trigger) -> Result<(), anyhow::Error> {
    log::debug!(
        "Inside handle_alert_triggers: processing trigger: {}",
        &trigger.module_key
    );
    let columns = trigger.module_key.split('/').collect::<Vec<&str>>();
    assert_eq!(columns.len(), 3);
    let org_id = &trigger.org;
    let stream_type: StreamType = columns[0].into();
    let stream_name = columns[1];
    let alert_name = columns[2];
    let is_realtime = trigger.is_realtime;
    let is_silenced = trigger.is_silenced;

    if is_realtime && is_silenced {
        log::debug!(
            "Realtime alert need wakeup, {}/{}",
            org_id,
            &trigger.module_key
        );
        // wakeup the trigger
        let new_trigger = db::scheduler::Trigger {
            next_run_at: Utc::now().timestamp_micros(),
            is_realtime: true,
            is_silenced: false,
            status: db::scheduler::TriggerStatus::Waiting,
            ..trigger.clone()
        };
        db::scheduler::update_trigger(new_trigger).await?;
        return Ok(());
    }

    let alert = match super::alert::get(org_id, stream_type, stream_name, alert_name).await? {
        Some(alert) => alert,
        None => {
            return Err(anyhow::anyhow!(
                "alert not found: {}/{}/{}/{}",
                org_id,
                stream_name,
                stream_type,
                alert_name
            ));
        }
    };

    let mut new_trigger = db::scheduler::Trigger {
        next_run_at: Utc::now().timestamp_micros(),
        is_realtime: false,
        is_silenced: false,
        status: db::scheduler::TriggerStatus::Waiting,
        retries: 0,
        ..trigger.clone()
    };

    if !alert.enabled {
        // update trigger, check on next week
        new_trigger.next_run_at += Duration::try_days(7).unwrap().num_microseconds().unwrap();
        new_trigger.is_silenced = true;
        db::scheduler::update_trigger(new_trigger).await?;
        return Ok(());
    }

    // evaluate alert
    let (ret, end_time) = alert.evaluate(None).await?;
    if ret.is_some() {
        log::info!(
            "Alert conditions satisfied, org: {}, module_key: {}",
            &new_trigger.org,
            &new_trigger.module_key
        );
    }
    if ret.is_some() && alert.trigger_condition.silence > 0 {
        if alert.trigger_condition.frequency_type == FrequencyType::Cron {
            let schedule = Schedule::from_str(&alert.trigger_condition.cron)?;
            let silence =
                Utc::now() + Duration::try_minutes(alert.trigger_condition.silence).unwrap();
            let silence = silence.with_timezone(
                FixedOffset::east_opt(alert.tz_offset * 60)
                    .as_ref()
                    .unwrap(),
            );
            // Check for the cron timestamp after the silence period
            new_trigger.next_run_at = schedule.after(&silence).next().unwrap().timestamp_micros();
        } else {
            new_trigger.next_run_at += Duration::try_minutes(alert.trigger_condition.silence)
                .unwrap()
                .num_microseconds()
                .unwrap();
        }
        new_trigger.is_silenced = true;
    } else if alert.trigger_condition.frequency_type == FrequencyType::Cron {
        let schedule = Schedule::from_str(&alert.trigger_condition.cron)?;
        // tz_offset is in minutes
        let tz_offset = FixedOffset::east_opt(alert.tz_offset * 60).unwrap();
        new_trigger.next_run_at = schedule
            .upcoming(tz_offset)
            .next()
            .unwrap()
            .timestamp_micros();
    } else {
        new_trigger.next_run_at += Duration::try_seconds(alert.trigger_condition.frequency)
            .unwrap()
            .num_microseconds()
            .unwrap();
    }

    let mut trigger_data_stream = TriggerData {
        _timestamp: trigger.start_time.unwrap_or_default(),
        org: trigger.org,
        module: TriggerDataType::Alert,
        key: trigger.module_key.clone(),
        next_run_at: new_trigger.next_run_at,
        is_realtime: trigger.is_realtime,
        is_silenced: trigger.is_silenced,
        status: TriggerDataStatus::Completed,
        start_time: end_time
            - Duration::try_minutes(alert.trigger_condition.period)
                .unwrap()
                .num_microseconds()
                .unwrap(),
        end_time,
        retries: trigger.retries,
        error: None,
    };

    // send notification
    if let Some(data) = ret {
        let vars = get_row_column_map(&data);
        let (alert_start_time, alert_end_time) =
            get_alert_start_end_time(&vars, alert.trigger_condition.period);
        trigger_data_stream.start_time = alert_start_time;
        trigger_data_stream.end_time = alert_end_time;
        match alert.send_notification(&data).await {
            Ok((true, _)) => {
                log::info!(
                    "Alert notification sent, org: {}, module_key: {}",
                    &new_trigger.org,
                    &new_trigger.module_key
                );
                db::scheduler::update_trigger(new_trigger).await?;
            }
            Ok((false, msg)) => {
                log::error!(
                    "Some notifications for alert {}/{} could not be sent: {msg}",
                    &new_trigger.org,
                    &new_trigger.module_key
                );
                // Notification is already sent to some destinations,
                // hence no need to retry
                trigger_data_stream.error = Some(msg);
                db::scheduler::update_trigger(new_trigger).await?;
            }
            Err(e) => {
                log::error!(
                    "Error sending alert notification: org: {}, module_key: {}",
                    &new_trigger.org,
                    &new_trigger.module_key
                );
                if trigger.retries + 1 >= get_config().limit.scheduler_max_retries {
                    // It has been tried the maximum time, just update the
                    // next_run_at to the next expected trigger time
                    log::debug!(
                        "This alert trigger: {}/{} has reached maximum retries",
                        &new_trigger.org,
                        &new_trigger.module_key
                    );
                    db::scheduler::update_trigger(new_trigger).await?;
                } else {
                    // Otherwise update its status only
                    db::scheduler::update_status(
                        &new_trigger.org,
                        new_trigger.module,
                        &new_trigger.module_key,
                        db::scheduler::TriggerStatus::Waiting,
                        trigger.retries + 1,
                    )
                    .await?;
                }
                trigger_data_stream.status = TriggerDataStatus::Failed;
                trigger_data_stream.error =
                    Some(format!("error sending notification for alert: {e}"));
            }
        }
    } else {
        log::debug!(
            "Alert conditions not satisfied, org: {}, module_key: {}",
            &new_trigger.org,
            &new_trigger.module_key
        );
        db::scheduler::update_trigger(new_trigger).await?;
        trigger_data_stream.status = TriggerDataStatus::ConditionNotSatisfied;
    }

    // publish the triggers as stream
    publish_triggers_usage(trigger_data_stream).await;

    Ok(())
}

async fn handle_report_triggers(trigger: db::scheduler::Trigger) -> Result<(), anyhow::Error> {
    log::debug!(
        "Inside handle_report_trigger,org: {}, module_key: {}",
        &trigger.org,
        &trigger.module_key
    );
    let org_id = &trigger.org;
    // For report, trigger.module_key is the report name
    let report_name = &trigger.module_key;

    let mut report = db::dashboards::reports::get(org_id, report_name).await?;
    let mut new_trigger = db::scheduler::Trigger {
        next_run_at: Utc::now().timestamp_micros(),
        is_realtime: false,
        is_silenced: false,
        status: db::scheduler::TriggerStatus::Waiting,
        retries: 0,
        ..trigger.clone()
    };

    if !report.enabled {
        log::debug!(
            "Report not enabled: org: {}, report: {}",
            org_id,
            report_name
        );
        // update trigger, check on next week
        new_trigger.next_run_at += Duration::try_days(7).unwrap().num_microseconds().unwrap();
        db::scheduler::update_trigger(new_trigger).await?;
        return Ok(());
    }
    let mut run_once = false;

    // Update trigger, set `next_run_at` to the
    // frequency interval of this report
    match report.frequency.frequency_type {
        ReportFrequencyType::Hours => {
            new_trigger.next_run_at += Duration::try_hours(report.frequency.interval)
                .unwrap()
                .num_microseconds()
                .unwrap();
        }
        ReportFrequencyType::Days => {
            new_trigger.next_run_at += Duration::try_days(report.frequency.interval)
                .unwrap()
                .num_microseconds()
                .unwrap();
        }
        ReportFrequencyType::Weeks => {
            new_trigger.next_run_at += Duration::try_weeks(report.frequency.interval)
                .unwrap()
                .num_microseconds()
                .unwrap();
        }
        ReportFrequencyType::Months => {
            // Assumes each month to be of 30 days.
            new_trigger.next_run_at += Duration::try_days(report.frequency.interval * 30)
                .unwrap()
                .num_microseconds()
                .unwrap();
        }
        ReportFrequencyType::Once => {
            // Check on next week
            new_trigger.next_run_at += Duration::try_days(7).unwrap().num_microseconds().unwrap();
            // Disable the report
            report.enabled = false;
            run_once = true;
        }
        ReportFrequencyType::Cron => {
            let schedule = Schedule::from_str(&report.frequency.cron)?;
            // tz_offset is in minutes
            let tz_offset = FixedOffset::east_opt(report.tz_offset * 60).unwrap();
            new_trigger.next_run_at = schedule
                .upcoming(tz_offset)
                .next()
                .unwrap()
                .timestamp_micros();
        }
    }

    let mut trigger_data_stream = TriggerData {
        _timestamp: trigger.start_time.unwrap_or_default(),
        org: trigger.org.clone(),
        module: TriggerDataType::Report,
        key: trigger.module_key.clone(),
        next_run_at: new_trigger.next_run_at,
        is_realtime: trigger.is_realtime,
        is_silenced: trigger.is_silenced,
        status: TriggerDataStatus::Completed,
        start_time: trigger.start_time.unwrap_or_default(),
        end_time: trigger.end_time.unwrap_or_default(),
        retries: trigger.retries,
        error: None,
    };

    let now = Utc::now().timestamp_micros();
    match report.send_subscribers().await {
        Ok(_) => {
            log::debug!("Report send_subscribers done, report: {}", report_name);
            // Report generation successful, update the trigger
            if run_once {
                new_trigger.status = db::scheduler::TriggerStatus::Completed;
            }
            db::scheduler::update_trigger(new_trigger).await?;
            log::debug!("Update trigger for report: {}", report_name);
            trigger_data_stream.end_time = Utc::now().timestamp_micros();
        }
        Err(e) => {
            log::error!("Error sending report to subscribers: {e}");
            if trigger.retries + 1 >= get_config().limit.scheduler_max_retries && !run_once {
                // It has been tried the maximum time, just update the
                // next_run_at to the next expected trigger time
                log::debug!(
                    "This report trigger: {org_id}/{report_name} has reached maximum possible retries"
                );
                db::scheduler::update_trigger(new_trigger).await?;
            } else {
                if run_once {
                    report.enabled = true;
                }
                // Otherwise update its status only
                db::scheduler::update_status(
                    &new_trigger.org,
                    new_trigger.module,
                    &new_trigger.module_key,
                    db::scheduler::TriggerStatus::Waiting,
                    trigger.retries + 1,
                )
                .await?;
            }
            trigger_data_stream.end_time = Utc::now().timestamp_micros();
            trigger_data_stream.status = TriggerDataStatus::Failed;
            trigger_data_stream.error = Some(format!("error processing report: {e}"));
        }
    }

    report.last_triggered_at = Some(now);
    // Check if the report has been disabled in the mean time
    let old_report = db::dashboards::reports::get(org_id, report_name).await?;
    if !old_report.enabled {
        report.enabled = old_report.enabled;
    }
    let result = db::dashboards::reports::set_without_updating_trigger(org_id, &report).await;
    if result.is_err() {
        log::error!(
            "Failed to update report: {report_name} after trigger: {}",
            result.err().unwrap()
        );
    }
    publish_triggers_usage(trigger_data_stream).await;

    Ok(())
}

async fn handle_derived_stream_triggers(
    trigger: db::scheduler::Trigger,
) -> Result<(), anyhow::Error> {
    log::debug!(
        "Inside handle_derived_stream_triggers processing trigger: {}",
        trigger.module_key
    );

    // module_key format: stream_type/stream_name/pipeline_name/derived_stream_name
    let columns = trigger.module_key.split('/').collect::<Vec<_>>();
    assert_eq!(columns.len(), 4);
    let org_id = &trigger.org;
    let stream_type: StreamType = columns[0].into();
    let stream_name = columns[1];
    let pipeline_name = columns[2];
    let name = columns[3];

    let is_real_time = trigger.is_realtime;
    let is_silenced = trigger.is_silenced;
    if is_real_time && is_silenced {
        log::debug!(
            "Realtime derived_stream needs to wake up, {}/{}",
            org_id,
            trigger.module_key
        );
        let new_trigger = db::scheduler::Trigger {
            next_run_at: Utc::now().timestamp_micros(),
            is_silenced: false,
            status: db::scheduler::TriggerStatus::Waiting,
            ..trigger.clone()
        };
        db::scheduler::update_trigger(new_trigger).await?;
        return Ok(());
    }

    let Ok(pipeline) = db::pipelines::get(org_id, stream_type, stream_name, pipeline_name).await
    else {
        return Err(anyhow::anyhow!(
            "Pipeline associated with trigger not found: {}/{}/{}/{}",
            org_id,
            stream_name,
            stream_type,
            pipeline_name
        ));
    };

    let Some(derived_stream) = pipeline
        .derived_streams
        .and_then(|ds| ds.into_iter().find(|ds| ds.name == name))
    else {
        return Err(anyhow::anyhow!(
            "DerivedStream associated with the trigger not found in pipeline: {}/{}/{}/{}",
            org_id,
            stream_name,
            stream_type,
            name,
        ));
    };

    let mut new_trigger = db::scheduler::Trigger {
        next_run_at: Utc::now().timestamp_micros(),
        is_silenced: false,
        status: db::scheduler::TriggerStatus::Waiting,
        ..trigger.clone()
    };

    // evaluate trigger and configure trigger next run time
    let (ret, _) = derived_stream.evaluate(None).await?;
    if ret.is_some() {
        log::info!(
            "DerivedStream conditions satisfied, org: {}, module_key: {}",
            new_trigger.org,
            new_trigger.module_key
        );
    }
    if ret.is_some() && derived_stream.trigger_condition.silence > 0 {
        if derived_stream.trigger_condition.frequency_type == FrequencyType::Cron {
            let schedule = Schedule::from_str(&derived_stream.trigger_condition.cron)?;
            let silence = Utc::now()
                + Duration::try_minutes(derived_stream.trigger_condition.silence).unwrap();
            let silence = silence.with_timezone(
                FixedOffset::east_opt(derived_stream.tz_offset * 60)
                    .as_ref()
                    .unwrap(),
            );
            // Check for the cron timestamp after the silence period
            new_trigger.next_run_at = schedule.after(&silence).next().unwrap().timestamp_micros();
        } else {
            new_trigger.next_run_at +=
                Duration::try_minutes(derived_stream.trigger_condition.silence)
                    .unwrap()
                    .num_microseconds()
                    .unwrap();
        }
        new_trigger.is_silenced = true;
    } else if derived_stream.trigger_condition.frequency_type == FrequencyType::Cron {
        let schedule = Schedule::from_str(&derived_stream.trigger_condition.cron)?;
        // tz_offset is in minutes
        let tz_offset = FixedOffset::east_opt(derived_stream.tz_offset * 60).unwrap();
        new_trigger.next_run_at = schedule
            .upcoming(tz_offset)
            .next()
            .unwrap()
            .timestamp_micros();
    } else {
        new_trigger.next_run_at +=
            Duration::try_minutes(derived_stream.trigger_condition.frequency)
                .unwrap()
                .num_microseconds()
                .unwrap();
    }

    let mut trigger_data_stream = TriggerData {
        _timestamp: trigger.start_time.unwrap_or_default(),
        org: trigger.org,
        module: TriggerDataType::DerivedStream,
        key: trigger.module_key.clone(),
        next_run_at: new_trigger.next_run_at,
        is_realtime: trigger.is_realtime,
        is_silenced: trigger.is_silenced,
        status: TriggerDataStatus::Completed,
        start_time: trigger.start_time.unwrap_or_default(),
        end_time: trigger.end_time.unwrap_or_default(),
        retries: trigger.retries,
        error: None,
    };

    // ingest evaluation result into destination
    if let Some(data) = ret {
        let local_val = data
            .into_iter()
            .map(json::Value::Object)
            .collect::<Vec<_>>();
        // Ingest result into destination stream
        let (org_id, stream_name, stream_type): (String, String, i32) = {
            (
                derived_stream.destination.org_id.into(),
                derived_stream.destination.stream_name.into(),
                cluster_rpc::StreamType::from(derived_stream.destination.stream_type).into(),
            )
        };
        let req = cluster_rpc::IngestionRequest {
            org_id: org_id.clone(),
            stream_name: stream_name.clone(),
            stream_type,
            data: Some(cluster_rpc::IngestionData::from(local_val)),
            ingestion_type: Some(cluster_rpc::IngestionType::Json.into()), /* TODO(taiming): finalize IngestionType for derived_stream */
        };
        match ingestion_service::ingest(&org_id, req).await {
            Ok(_) => {
                log::info!(
                    "DerivedStream result ingested to destination {org_id}/{stream_name}/{stream_type}",
                );
                db::scheduler::update_trigger(new_trigger).await?;
            }
            Err(e) => {
                log::error!(
                    "Error in ingesting DerivedStream result to destination {:?}, org: {}, module_key: {}",
                    e,
                    new_trigger.org,
                    new_trigger.module_key
                );
                if trigger.retries + 1 >= get_config().limit.scheduler_max_retries {
                    // It has been tried the maximum time, just update the
                    // next_run_at to the next expected trigger time
                    log::debug!(
                        "This DerivedStream trigger: {}/{} has reached maximum retries",
                        &new_trigger.org,
                        &new_trigger.module_key
                    );
                    db::scheduler::update_trigger(new_trigger).await?;
                } else {
                    // Otherwise update its status only
                    db::scheduler::update_status(
                        &new_trigger.org,
                        new_trigger.module,
                        &new_trigger.module_key,
                        db::scheduler::TriggerStatus::Waiting,
                        trigger.retries + 1,
                    )
                    .await?;
                }
                trigger_data_stream.status = TriggerDataStatus::Failed;
                trigger_data_stream.error =
                    Some(format!("error sending notification for alert: {e}"));
            }
        }
    } else {
        log::debug!(
            "DerivedStream conditions not satisfied, org: {}, module_key: {}",
            &new_trigger.org,
            &new_trigger.module_key
        );
        db::scheduler::update_trigger(new_trigger).await?;
        trigger_data_stream.status = TriggerDataStatus::ConditionNotSatisfied;
    }

    // publish the triggers as stream
    trigger_data_stream.end_time = Utc::now().timestamp_micros();
    publish_triggers_usage(trigger_data_stream).await;

    Ok(())
}

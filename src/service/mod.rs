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

use once_cell::sync::Lazy;
use regex::Regex;

pub mod alerts;
pub mod compact;
pub mod dashboards;
pub mod db;
pub mod enrichment;
pub mod enrichment_table;
pub mod exporter;
pub mod file_list;
pub mod functions;
pub mod ingestion;
pub mod kv;
pub mod logs;
pub mod metadata;
pub mod metrics;
pub mod organization;
pub mod pipelines;
pub mod promql;
pub mod schema;
pub mod search;
pub mod session;
pub mod stream;
pub mod syslogs_route;
pub mod traces;
pub mod usage;
pub mod users;

const MAX_KEY_LENGTH: usize = 100;

static RE_CORRECT_STREAM_NAME: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-zA-Z0-9_]+").unwrap());

// format partition key
pub fn format_partition_key(input: &str) -> String {
    let mut output = String::with_capacity(std::cmp::min(input.len(), MAX_KEY_LENGTH));
    for c in input.chars() {
        if output.len() > MAX_KEY_LENGTH {
            break;
        }
        if c.is_alphanumeric() || c == '=' || c == '-' || c == '_' {
            output.push(c);
        }
    }
    output
}

// format stream name
pub fn format_stream_name(stream_name: &str) -> String {
    RE_CORRECT_STREAM_NAME
        .replace_all(stream_name, "_")
        .to_string()
}

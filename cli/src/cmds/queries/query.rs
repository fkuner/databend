// Copyright 2020 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::borrow::Borrow;
use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;

use async_trait::async_trait;
use clap::App;
use clap::AppSettings;
use clap::Arg;
use clap::ArgMatches;
use comfy_table::Cell;
use comfy_table::Color;
use comfy_table::Table;
use common_base::ProgressValues;
use lexical_util::num::AsPrimitive;
use num_format::Locale;
use num_format::ToFormattedString;

use crate::cmds::clusters::cluster::ClusterProfile;
use crate::cmds::command::Command;
use crate::cmds::Config;
use crate::cmds::Status;
use crate::cmds::Writer;
use crate::error::CliError;
use crate::error::Result;

#[derive(Clone)]
pub struct QueryCommand {
    #[allow(dead_code)]
    conf: Config,
    clap: App<'static>,
}

impl QueryCommand {
    pub fn create(conf: Config) -> Self {
        let clap = QueryCommand::generate();
        QueryCommand { conf, clap }
    }
    pub fn generate() -> App<'static> {
        let app = App::new("query")
            .setting(AppSettings::DisableVersionFlag)
            .about("Query on databend cluster")
            .arg(
                Arg::new("profile")
                    .long("profile")
                    .about("Profile to run queries")
                    .required(false)
                    .possible_values(&["local"])
                    .default_value("local"),
            )
            .arg(
                Arg::new("query")
                    .about("Query statements to run")
                    .takes_value(true)
                    .required(true),
            );
        app
    }

    pub(crate) async fn exec_match(
        &self,
        writer: &mut Writer,
        args: Option<&ArgMatches>,
    ) -> Result<()> {
        match args {
            Some(matches) => {
                let profile = matches.value_of_t("profile");
                match profile {
                    Ok(ClusterProfile::Local) => {
                        return self.local_exec_match(writer, matches).await;
                    }
                    Ok(ClusterProfile::Cluster) => {
                        todo!()
                    }
                    Err(_) => writer.write_err("currently profile only support cluster or local"),
                }
            }
            None => {
                println!("none ");
            }
        }
        Ok(())
    }

    async fn local_exec_match(&self, writer: &mut Writer, args: &ArgMatches) -> Result<()> {
        match self.local_exec_precheck(args) {
            Ok(_) => {
                writer.write_ok("Query precheck passed!");
                let status = Status::read(self.conf.clone())?;
                let queries = match args.value_of("query") {
                    Some(val) => {
                        if Path::new(val).exists() {
                            let buffer =
                                std::fs::read(Path::new(val)).expect("cannot read query from file");
                            String::from_utf8_lossy(&*buffer).to_string()
                        } else if val.starts_with("http://") || val.starts_with("https://") {
                            let res = reqwest::get(val)
                                .await
                                .expect("cannot fetch query from url")
                                .text()
                                .await
                                .expect("cannot fetch response body");
                            res
                        } else {
                            val.to_string()
                        }
                    }
                    None => {
                        let mut buffer = String::new();
                        std::io::stdin()
                            .read_to_string(&mut buffer)
                            .expect("cannot read from stdin");
                        buffer
                    }
                };

                let res = build_query_endpoint(&status);

                if let Ok((cli, url)) = res {
                    for query in queries
                        .split(';')
                        .filter(|elem| !elem.trim().is_empty())
                        .map(|elem| format!("{};", elem))
                        .collect::<Vec<String>>()
                    {
                        writer.write_ok(
                            format!("Execute query {} on {}", query.clone(), url).as_str(),
                        );
                        if let Err(e) =
                            query_writer(&cli, url.as_str(), query.clone(), writer).await
                        {
                            writer.write_err(
                                format!("query {} execution error: {:?}", query, e).as_str(),
                            );
                        }
                    }
                } else {
                    writer.write_err(
                        format!(
                            "Query command error: cannot parse query url with error: {:?}",
                            res.unwrap_err()
                        )
                        .as_str(),
                    );
                }

                Ok(())
            }
            Err(e) => {
                writer.write_err(&*format!("Query command precheck failed, error {:?}", e));
                Ok(())
            }
        }
    }

    /// precheck whether current local profile applicable for local host machine
    fn local_exec_precheck(&self, _args: &ArgMatches) -> Result<()> {
        let status = Status::read(self.conf.clone())?;
        if !status.has_local_configs() {
            return Err(CliError::Unknown(format!(
                "Query command error: cannot find local configs in {}, please run `bendctl cluster create --profile local` to create a new local cluster",
                status.local_config_dir
            )));
        }

        Ok(())
    }
}

async fn query_writer(
    cli: &reqwest::Client,
    url: &str,
    query: String,
    writer: &mut Writer,
) -> Result<()> {
    let start = std::time::Instant::now();
    match execute_query(cli, url, query).await {
        Ok((res, stats)) => {
            let elapsed = start.elapsed();
            writer.writeln(res.trim_fmt().as_str());
            if let Some(stat) = stats {
                let time = elapsed.as_millis() as f64 / 1000f64;
                let byte_per_sec = byte_unit::Byte::from_unit(
                    stat.read_bytes as f64 / time,
                    byte_unit::ByteUnit::B,
                )
                .expect("cannot parse byte")
                .get_appropriate_unit(false);
                writer.write_ok(
                    format!(
                        "read rows: {}, read bytes: {}, rows/sec: {} (rows/sec), bytes/sec: {} ({}/sec)",
                        stat.read_rows.to_formatted_string(&Locale::en),
                        byte_unit::Byte::from_bytes(stat.read_bytes as u128)
                            .get_appropriate_unit(false)
                            .to_string(),
                        (stat.read_rows as f64 / time).as_u128().to_formatted_string(&Locale::en),
                        byte_per_sec.get_value(),
                        byte_per_sec.get_unit().to_string()
                    )
                        .as_str(),
                );
            }
        }
        Err(e) => {
            writer.write_err(
                format!(
                    "Query command error: cannot execute query with error: {:?}",
                    e
                )
                .as_str(),
            );
        }
    }
    Ok(())
}

// TODO(zhihanz) mTLS support
pub fn build_query_endpoint(status: &Status) -> Result<(reqwest::Client, String)> {
    let query_configs = status.get_local_query_configs();

    let (_, query) = query_configs.get(0).expect("cannot find query configs");
    let client = reqwest::Client::builder()
        .build()
        .expect("Cannot build query client");

    let url = {
        if query.config.query.api_tls_server_key.is_empty()
            || query.config.query.api_tls_server_cert.is_empty()
        {
            let address = format!(
                "{}:{}",
                query.config.query.http_handler_host, query.config.query.http_handler_port
            )
            .parse::<SocketAddr>()
            .expect("cannot build query socket address");
            format!("http://{}:{}/v1/statement", address.ip(), address.port())
        } else {
            todo!()
        }
    };
    Ok((client, url))
}

async fn execute_query(
    cli: &reqwest::Client,
    url: &str,
    query: String,
) -> Result<(Table, Option<ProgressValues>)> {
    let ans = cli
        .post(url)
        .body(query.clone())
        .send()
        .await
        .expect("cannot post to http handler")
        .json::<databend_query::servers::http::v1::statement::HttpQueryResult>()
        .await;
    if let Err(e) = ans {
        return Err(CliError::Unknown(format!(
            "Cannot retrieve query result: {:?}",
            e
        )));
    } else {
        let ans = ans.unwrap();
        let mut table = Table::new();
        table.load_preset("||--+-++|    ++++++");
        if let Some(column) = ans.columns {
            table.set_header(
                column
                    .fields()
                    .iter()
                    .map(|field| Cell::new(field.name().as_str()).fg(Color::Green)),
            );
        }
        if let Some(rows) = ans.data {
            for row in rows {
                table.add_row(row.iter().map(|elem| Cell::new(elem.to_string())));
            }
        }
        Ok((table, ans.stats))
    }
}

#[async_trait]
impl Command for QueryCommand {
    fn name(&self) -> &str {
        "query"
    }

    fn about(&self) -> &str {
        "Query on databend cluster"
    }

    fn is(&self, s: &str) -> bool {
        s.contains(self.name())
    }

    async fn exec(&self, writer: &mut Writer, args: String) -> Result<()> {
        let words = shellwords::split(args.as_str());
        if words.is_err() {
            writer.write_err("cannot parse words");
            return Ok(());
        }
        match self.clap.clone().try_get_matches_from(words.unwrap()) {
            Ok(matches) => {
                return self.exec_match(writer, Some(matches.borrow())).await;
            }
            Err(err) => {
                println!("Cannot get subcommand matches: {}", err);
            }
        }

        Ok(())
    }
}
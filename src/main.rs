// SPDX-FileCopyrightText: 2022 Profian Inc. <opensource@profian.com>
// SPDX-License-Identifier: AGPL-3.0-only

#![forbid(unsafe_code)]
#![warn(clippy::all, rust_2018_idioms, unused_lifetimes)]

mod auth;
mod data;
mod jobs;
mod ports;
mod redirect;
mod reference;
mod secret;
mod templates;

use crate::data::Data;
use crate::reference::Ref;
use crate::templates::{HtmlTemplate, IdxTemplate, JobTemplate};

use std::fs::read;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::Range;
use std::time::Duration;

use axum::extract::Multipart;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Router, Server};

use anyhow::{bail, Context as _};
use clap::Parser;
use tokio::fs::read_to_string;
use tokio::time::{sleep, timeout};
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// TODO: raise this when this is fixed: https://github.com/profianinc/benefice/issues/75
const READ_TIMEOUT: Duration = Duration::from_millis(500);
const TOML_MAX: usize = 256 * 1024; // 256 KiB

/// Demo workload executor.
///
/// Any command-line options listed here may be specified by one or
/// more configuration files, which can be used by passing the
/// name of the file on the command-line with the syntax `@config.toml`.
/// The configuration file must contain valid TOML table mapping argument
/// names to their values.
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Address to bind to.
    #[clap(long, default_value_t = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 3000))]
    addr: SocketAddr,

    /// Externally accessible root URL.
    /// For example: https://benefice.example.com
    #[clap(long)]
    url: auth::Url,

    /// Maximum jobs.
    #[clap(long, default_value_t = num_cpus::get())]
    jobs: usize,

    /// Default file size limit (in MiB).
    #[clap(long, default_value_t = 10)]
    size_limit_default: usize,

    /// Starred file size limit (in MiB).
    #[clap(long, default_value_t = 50)]
    size_limit_starred: usize,

    /// Default job timeout (in seconds).
    #[clap(long, default_value_t = 5 * 60)]
    timeout_default: u64,

    /// Starred job timeout (in seconds).
    #[clap(long, default_value_t = 15 * 60)]
    timeout_starred: u64,

    /// If duplicate listen port mitigations should be enabled.
    /// This will track ports opened by benefice to prevent
    /// users from listening on the same port twice.
    #[clap(long, default_value_t = true)]
    shared_port_protections: bool,

    /// The lowest listen port allowed in an Enarx.toml.
    #[clap(long, default_value_t = 2_000)]
    port_min: u16,

    /// The highest listen port allowed in an Enarx.toml.
    #[clap(long, default_value_t = 30_000)]
    port_max: u16,

    /// Command to execute, normally path to `enarx` binary.
    /// This command will be executed as: `<cmd> run --wasmcfgfile <path-to-config> <path-to-wasm>`
    #[clap(long, default_value = "enarx")]
    command: String,

    /// OpenID Connect issuer URL.
    #[clap(long, default_value = "https://auth.profian.com/")]
    oidc_issuer: auth::Url,

    /// OpenID Connect client ID.
    #[clap(long)]
    oidc_client: String,

    /// Path to a file containing OpenID Connect secret.
    #[clap(long)]
    oidc_secret: Option<secret::SecretFile>,
}

impl Args {
    fn split(self) -> (Limits, auth::Oidc, Other) {
        let limits = Limits {
            size_limit_default: self.size_limit_default,
            size_limit_starred: self.size_limit_starred,
            timeout_default: Duration::from_secs(self.timeout_default),
            timeout_starred: Duration::from_secs(self.timeout_starred),
        };

        let oidc = auth::Oidc {
            server: self.url,
            issuer: self.oidc_issuer,
            client: self.oidc_client,
            secret: self.oidc_secret.map(|sf| sf.into()),
            ttl: Duration::from_secs(24 * 60 * 60),
        };

        let other = Other {
            addr: self.addr,
            jobs: self.jobs,
            shared_port_protections: self.shared_port_protections,
            port_range: self.port_min..self.port_max,
            cmd: self.command,
        };

        (limits, oidc, other)
    }
}

#[derive(Copy, Clone, Debug)]
struct Limits {
    size_limit_default: usize,
    size_limit_starred: usize,
    timeout_default: Duration,
    timeout_starred: Duration,
}

impl Limits {
    pub fn decide(&self, star: bool) -> (Duration, usize) {
        let size = match star {
            false => self.size_limit_default,
            true => self.size_limit_starred,
        };

        let ttl = match star {
            false => self.timeout_default,
            true => self.timeout_starred,
        };

        (ttl, size)
    }
}

#[derive(Clone, Debug)]
struct Other {
    addr: SocketAddr,
    jobs: usize,
    shared_port_protections: bool,
    port_range: Range<u16>,
    cmd: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (limits, oidc, other) = std::env::args()
        .try_fold(Vec::new(), |mut args, arg| {
            if let Some(path) = arg.strip_prefix('@') {
                let conf = read(path).context(format!("failed to read config file at `{path}`"))?;
                match toml::from_slice(&conf)
                    .context(format!("failed to parse config file at `{path}` as TOML"))?
                {
                    toml::Value::Table(kv) => kv.into_iter().try_for_each(|(k, v)| {
                        match v {
                            toml::Value::String(v) => args.push(format!("--{k}={v}")),
                            toml::Value::Integer(v) => args.push(format!("--{k}={v}")),
                            toml::Value::Float(v) => args.push(format!("--{k}={v}")),
                            toml::Value::Boolean(v) => {
                                if v {
                                    args.push(format!("--{k}"))
                                }
                            }
                            _ => bail!(
                                "unsupported value type for field `{k}` in config file at `{path}`"
                            ),
                        }
                        Ok(())
                    })?,
                    _ => bail!("invalid config file format in file at `{path}`"),
                }
            } else {
                args.push(arg);
            }
            Ok(args)
        })
        .map(Args::parse_from)
        .context("Failed to parse arguments")?
        .split();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "example_tracing_aka_logging=debug,tower_http=debug".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let app = Router::new()
        .route(
            "/out",
            post(move |user| reader(user, jobs::Standard::Output)),
        )
        .route(
            "/err",
            post(move |user| reader(user, jobs::Standard::Error)),
        )
        .route(
            "/",
            get(move |user| root_get(user, limits))
                .post(move |user, mp| {
                    root_post(
                        user,
                        mp,
                        other.cmd,
                        limits,
                        other.shared_port_protections,
                        other.port_range,
                        other.jobs,
                    )
                })
                .delete(root_delete),
        );

    let app = oidc.routes::<Data>(app).await?;

    Server::bind(&other.addr)
        .serve(app.layer(TraceLayer::new_for_http()).into_make_service())
        .await?;
    Ok(())
}

async fn root_get(user: Option<Ref<auth::User<Data>>>, limits: Limits) -> impl IntoResponse {
    let (user, star) = match user {
        None => (false, false),
        Some(user) => {
            if user.read().await.data.job().is_some() {
                return HtmlTemplate(JobTemplate).into_response();
            }

            (true, user.read().await.is_starred("enarx/enarx").await)
        }
    };

    let (ttl, size) = limits.decide(star);

    let tmpl = IdxTemplate {
        toml: enarx_config::CONFIG_TEMPLATE,
        user,
        star,
        size,
        ttl: ttl.as_secs(),
    };

    HtmlTemplate(tmpl).into_response()
}

// TODO: create tests for endpoints: #38
async fn root_post(
    user: Ref<auth::User<Data>>,
    mut multipart: Multipart,
    command: String,
    limits: Limits,
    shared_port_protections: bool,
    port_range: Range<u16>,
    jobs: usize,
) -> impl IntoResponse {
    let (ttl, size) = limits.decide(user.read().await.is_starred("enarx/enarx").await);

    if user.read().await.data.job().is_some() {
        return Err(Redirect::to("/").into_response());
    }

    if jobs::Job::count() >= jobs {
        return Err(redirect::too_many_workloads().into_response());
    }

    let mut wasm = None;
    let mut toml = None;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?
    {
        match field.name() {
            Some("wasm") => {
                if Some("application/wasm") != field.content_type() {
                    return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
                }

                if wasm.is_some() {
                    return Err(StatusCode::BAD_REQUEST.into_response());
                }

                let mut len = 0;
                let mut out = tempfile::NamedTempFile::new()
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;

                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|_| StatusCode::BAD_REQUEST.into_response())?
                {
                    len += chunk.len();
                    if len > size * 1024 * 1024 {
                        return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
                    }

                    out.write_all(&chunk)
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
                }

                wasm = Some(out);
            }

            Some("toml") => {
                if field.content_type().is_some() {
                    return Err(StatusCode::BAD_REQUEST.into_response());
                }

                if toml.is_some() {
                    return Err(StatusCode::BAD_REQUEST.into_response());
                }

                let mut len = 0;
                let mut out = tempfile::NamedTempFile::new()
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;

                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|_| StatusCode::BAD_REQUEST.into_response())?
                {
                    len += chunk.len();
                    if len > TOML_MAX {
                        return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
                    }

                    out.write_all(&chunk)
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
                }

                toml = Some(out);
            }

            _ => continue,
        }
    }

    let wasm = wasm.ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?;
    let toml = toml.ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?;

    let enarx_config_string = read_to_string(&toml).await.map_err(|e| {
        debug!("failed to read enarx config file: {e}");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;
    let ports = ports::get_listen_ports(&enarx_config_string).map_err(|e| {
        debug!("failed to get ports from enarx config: {e}");
        StatusCode::BAD_REQUEST.into_response()
    })?;

    // Check if the port is outside of the range of allowed ports
    let illegal_ports = ports
        .iter()
        .filter(|port| !port_range.contains(port))
        .cloned()
        .collect::<Vec<_>>();

    if !illegal_ports.is_empty() {
        return Err(redirect::illegal_ports(&illegal_ports, port_range).into_response());
    }

    if shared_port_protections {
        // Check if a port is already in use by another running workload
        ports::try_reserve(&ports)
            .await
            .map_err(|port_conflicts| redirect::port_conflicts(&port_conflicts).into_response())?;
    }

    // Create the new job and get an identifier.
    let uuid = {
        let mut lock = user.write().await;

        if lock.data.job().is_some() {
            return Err(Redirect::to("/").into_response());
        }

        if jobs::Job::count() >= jobs {
            return Err(redirect::too_many_workloads().into_response());
        }

        let job = jobs::Job::new(command, wasm, toml, ports).map_err(|e| {
            error!("failed to spawn process: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;

        let uuid = job.uuid;
        lock.data = Data::new(Some(job));
        uuid
    };

    // Set the job timeout.
    let weak = Ref::downgrade(&user);
    tokio::spawn(async move {
        sleep(ttl).await;

        if let Some(user) = weak.upgrade() {
            debug!("timeout for: {}", uuid);
            let mut lock = user.write().await;
            if lock.data.job().as_ref().map(|j| j.uuid) == Some(uuid) {
                lock.data.kill_job().await;
            }
        }
    });

    info!(
        "job started. job_id={uuid}, user_id={}",
        user.read().await.uid
    );
    Ok((StatusCode::SEE_OTHER, [("Location", "/")]))
}

async fn root_delete(user: Ref<auth::User<Data>>) -> StatusCode {
    let mut lock = user.write().await;

    if let Some(uuid) = lock.data.job().as_ref().map(|j| j.uuid) {
        debug!("killing: {}", uuid);
        lock.data.kill_job().await;
    }

    StatusCode::OK
}

async fn reader(user: Ref<auth::User<Data>>, kind: jobs::Standard) -> Result<Vec<u8>, StatusCode> {
    let mut buf = [0; 4096];

    match user.write().await.data.job_mut() {
        None => Err(StatusCode::NOT_FOUND),
        Some(job) => {
            let future = job.read(kind, &mut buf);
            match timeout(READ_TIMEOUT, future).await {
                Ok(Err(..)) => Err(StatusCode::INTERNAL_SERVER_ERROR),
                Ok(Ok(size)) => Ok(buf[..size].to_vec()),
                Err(..) => Ok(Vec::new()),
            }
        }
    }
}

//! `donto-memory` binary entry point — clap subcommands dispatching
//! into the core library.

use anyhow::Result;
use clap::{Parser, Subcommand};
use donto_memory_core::{
    module::register_default_modules, overlays, sleep_path, substrate::SubstrateClient,
    Settings,
};
use donto_memory::api;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "donto-memory", version, about = "Agentic-memory runtime on donto")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Apply consumer-overlay migrations + register them with the substrate.
    Migrate {
        /// dontosrv base URL. Defaults to env DONTO_MEMORY_DONTOSRV_URL.
        #[arg(long)]
        substrate_url: Option<String>,
        /// Postgres DSN. Defaults to env DONTO_MEMORY_DONTO_DSN.
        #[arg(long)]
        dsn: Option<String>,
        /// Path to the migrations directory.
        #[arg(long, default_value = "migrations")]
        dir: PathBuf,
        /// Apply SQL only; skip overlay registration.
        #[arg(long)]
        skip_register: bool,
    },
    /// Run the FastAPI-equivalent axum HTTP server.
    Api {
        /// host:port. Defaults to env DONTO_MEMORY_API_BIND.
        #[arg(long)]
        bind: Option<String>,
    },
    /// Run the sleep-path reconsolidation worker.
    Worker {
        /// Run a single pass through the queue and exit.
        #[arg(long)]
        once: bool,
    },
    /// Print the substrate's contract-version response to stdout
    /// (useful for verifying the substrate URL is correct).
    Substrate,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,donto_memory=info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Migrate {
            substrate_url,
            dsn,
            dir,
            skip_register,
        } => migrate(substrate_url, dsn, dir, skip_register).await,
        Command::Api { bind } => api(bind).await,
        Command::Worker { once } => worker(once).await,
        Command::Substrate => substrate_probe().await,
    }
}

async fn migrate(
    substrate_url: Option<String>,
    dsn: Option<String>,
    dir: PathBuf,
    skip_register: bool,
) -> Result<()> {
    let mut settings = Settings::from_env();
    if let Some(u) = substrate_url {
        settings.dontosrv_url = u;
    }
    if let Some(d) = dsn {
        settings.donto_dsn = Some(d);
    }
    let dsn_str = settings
        .donto_dsn
        .clone()
        .ok_or_else(|| anyhow::anyhow!("DONTO_MEMORY_DONTO_DSN required (or pass --dsn)"))?;
    let count = overlays::run_migrations(&dsn_str, &dir).await?;
    println!("✓ {count} migration(s) applied");
    if !skip_register {
        let n = overlays::register_overlays(&dsn_str, &settings.consumer_iri).await?;
        println!("✓ {n} overlay(s) registered");
    } else {
        println!("(skipped overlay registration: --skip-register)");
    }
    Ok(())
}

async fn api(bind: Option<String>) -> Result<()> {
    let mut settings = Settings::from_env();
    if let Some(b) = bind {
        settings.api_bind = b;
    }
    let _registry = register_default_modules();
    let substrate = SubstrateClient::new(&settings.dontosrv_url)?;
    if let Err(e) = substrate.assert_contract_floor(&settings.substrate_contract_floor).await {
        tracing::warn!(error = %e, "substrate handshake warning");
    }
    let dsn = settings
        .donto_dsn
        .clone()
        .ok_or_else(|| anyhow::anyhow!("DONTO_MEMORY_DONTO_DSN required"))?;
    let pool = overlays::pool_from_dsn(&dsn)?;

    // Any "POST /memorize (queued)" audit row without a matching
    // "POST /memorize (async)" / "POST /memorize (async-failed)" was
    // a tokio task that died across a previous restart. Surface them
    // in the audit log so the /jobs page shows the failure and an
    // operator can decide whether to re-memorize the input.
    match mark_orphaned_queued_rows(&pool, &settings.consumer_iri).await {
        Ok(n) if n > 0 => tracing::warn!(orphaned = n, "marked stale (lost) queued memorize rows from prior runs"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "orphan-mark on startup failed; continuing"),
    }

    let app = api::router(api::AppState {
        settings: settings.clone(),
        substrate: substrate.clone(),
        pool,
        async_memorize_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
    });

    let listener = tokio::net::TcpListener::bind(&settings.api_bind).await?;
    tracing::info!(bind = %settings.api_bind, "donto-memory api listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Find every `(queued)` audit row whose queue_id has no matching
/// `(async)` / `(async-failed)` completion, and write a
/// `POST /memorize (lost)` audit row for each. Only touches rows older
/// than 60 seconds so a healthy in-flight task isn't flagged.
///
/// Skips rows marked `"durable": true` — those were enqueued to the
/// Temporal queue, which survives API restarts and resumes the
/// workflow on its own. Only the in-process tokio *fallback* path
/// (durable=false, used when the Temporal gateway was unreachable) can
/// actually be lost to a restart, so only those are swept.
async fn mark_orphaned_queued_rows(
    pool: &deadpool_postgres::Pool,
    consumer_iri: &str,
) -> Result<u64> {
    let c = pool.get().await?;
    let n = c
        .execute(
            "insert into donto_x_memory_job_log
                 (consumer_iri, endpoint, holder, session_id, status_code,
                  elapsed_ms, request, response, error)
             select $1, 'POST /memorize (lost)', q.holder, q.session_id, 500, 0,
                    q.request,
                    jsonb_build_object(
                      'orphaned_queue_id', q.response->>'queue_id',
                      'queued_at', to_jsonb(q.created_at),
                      'note', 'background extraction task did not complete; likely killed by API restart'
                    ),
                    'orphaned (likely lost to API restart before completion)'
               from donto_x_memory_job_log q
              where q.endpoint = 'POST /memorize (queued)'
                and q.created_at < now() - interval '60 seconds'
                and coalesce(q.response->>'durable', 'false') <> 'true'
                and not exists (
                  select 1 from donto_x_memory_job_log a
                   where a.endpoint in ('POST /memorize (async)', 'POST /memorize (async-failed)')
                     and a.response->>'queue_id' = q.response->>'queue_id'
                )
                and not exists (
                  select 1 from donto_x_memory_job_log l
                   where l.endpoint = 'POST /memorize (lost)'
                     and l.response->>'orphaned_queue_id' = q.response->>'queue_id'
                )",
            &[&consumer_iri],
        )
        .await?;
    Ok(n)
}

async fn worker(once: bool) -> Result<()> {
    let settings = Settings::from_env();
    let _registry = register_default_modules();
    let substrate = SubstrateClient::new(&settings.dontosrv_url)?;
    substrate.assert_contract_floor(&settings.substrate_contract_floor).await?;
    let dsn = settings
        .donto_dsn
        .clone()
        .ok_or_else(|| anyhow::anyhow!("DONTO_MEMORY_DONTO_DSN required"))?;
    let pool = overlays::pool_from_dsn(&dsn)?;
    sleep_path::run_worker(&settings, &substrate, &pool, once).await?;
    Ok(())
}

async fn substrate_probe() -> Result<()> {
    let settings = Settings::from_env();
    let substrate = SubstrateClient::new(&settings.dontosrv_url)?;
    let info = substrate.contract_version().await?;
    println!("{}", serde_json::to_string_pretty(&info)?);
    Ok(())
}

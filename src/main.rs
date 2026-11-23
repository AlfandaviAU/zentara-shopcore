use std::process::ExitCode;

use shopcore::{db, manage, router, AppState, Config};
use tracing_subscriber::EnvFilter;

const USAGE: &str =
    "usage: shopcore <serve|init-db|seed|import-products <csv>|export-sales <file>|purge-sessions>";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let known = [
        "serve",
        "init-db",
        "seed",
        "import-products",
        "export-sales",
        "purge-sessions",
    ];
    if !known.contains(&cmd) {
        eprintln!("{USAGE}");
        return ExitCode::from(1);
    }
    if matches!(cmd, "import-products" | "export-sales") && args.len() < 2 {
        eprintln!("usage: shopcore {cmd} <file>");
        return ExitCode::from(1);
    }
    match run(cmd, &args[1..]).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(cmd: &str, args: &[String]) -> anyhow::Result<()> {
    let cfg = Config::from_env();
    let pool = db::connect(&cfg).await?;
    match cmd {
        "init-db" => {
            db::init_db(&pool).await?;
            println!("tables ready");
        }
        "seed" => {
            manage::seed(&pool, &cfg).await?;
            println!("seeded");
        }
        "import-products" => manage::import_products(&pool, &args[0]).await?,
        "export-sales" => manage::export_sales(&pool, &args[0]).await?,
        "purge-sessions" => {
            let n = manage::purge_sessions(&pool).await?;
            println!("purged sessions ({n})");
        }
        "serve" => serve(cfg, pool).await?,
        _ => unreachable!(),
    }
    Ok(())
}

async fn serve(cfg: Config, pool: sqlx::PgPool) -> anyhow::Result<()> {
    // self-sufficient start: make sure tables exist even if init-db was skipped
    db::init_db(&pool).await?;
    let addr = cfg.bind_addr.clone();
    let app = router(AppState::new(cfg, pool));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "shopcore listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}

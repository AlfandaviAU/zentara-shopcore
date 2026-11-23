//! The maintenance commands (legacy `manage.py`) and `serve`, run as the real binary.
//! Needs `TEST_DATABASE_URL`; skipped otherwise. Runs as its own test binary, so
//! `purge-sessions` cannot interfere with the API tests.

use std::process::{Command, Output};
use std::time::Duration;

fn db() -> Option<String> {
    std::env::var("TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn shopcore(db: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shopcore"))
        .args(args)
        .env("DATABASE_URL", db)
        .env("RUST_LOG", "warn")
        .output()
        .expect("run shopcore")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

#[test]
fn usage_errors_exit_1() {
    let out = Command::new(env!("CARGO_BIN_EXE_shopcore"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage"));
    let out = Command::new(env!("CARGO_BIN_EXE_shopcore"))
        .arg("bogus")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let out = Command::new(env!("CARGO_BIN_EXE_shopcore"))
        .arg("import-products")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn maintenance_commands_end_to_end() {
    let Some(db) = db() else {
        eprintln!("skipped: set TEST_DATABASE_URL");
        return;
    };
    let o = shopcore(&db, &["init-db"]);
    assert!(
        o.status.success() && stdout(&o).contains("tables ready"),
        "{o:?}"
    );
    let o = shopcore(&db, &["seed"]);
    assert!(o.status.success() && stdout(&o).contains("seeded"), "{o:?}");

    let dir = std::env::temp_dir().join(format!("shopcore-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sku = format!("CLI-{}", std::process::id());
    let csv = dir.join("p.csv");
    std::fs::write(
        &csv,
        format!("sku,name,price,stock\n{sku},\"Cli, Item\",2.5,4\n{sku},dup,1,1\n"),
    )
    .unwrap();
    let o = shopcore(&db, &["import-products", csv.to_str().unwrap()]);
    assert!(stdout(&o).contains("imported 1 products"), "{o:?}");
    // a second import updates the existing SKU instead of duplicating it
    std::fs::write(&csv, format!("sku,name,price,stock\n{sku},Cli Item,3,9\n")).unwrap();
    let o = shopcore(&db, &["import-products", csv.to_str().unwrap()]);
    assert!(stdout(&o).contains("imported 1 products"), "{o:?}");
    std::fs::write(&csv, "sku,name,price,stock\nX,bad,notanumber,1\n").unwrap();
    let o = shopcore(&db, &["import-products", csv.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "invalid file is rejected");
    let o = shopcore(&db, &["import-products", "/nonexistent.csv"]);
    assert_eq!(o.status.code(), Some(1));

    let out = dir.join("sales.csv");
    let o = shopcore(&db, &["export-sales", out.to_str().unwrap()]);
    assert!(
        stdout(&o).starts_with(&format!("wrote {}", out.display())),
        "{o:?}"
    );
    assert!(std::fs::read_to_string(&out)
        .unwrap()
        .starts_with("order_no,status,total,created_at\n"));

    let o = shopcore(&db, &["purge-sessions"]);
    assert!(
        o.status.success() && stdout(&o).contains("purged sessions"),
        "{o:?}"
    );
}

#[test]
fn serve_starts_answers_and_shuts_down_gracefully() {
    let Some(db) = db() else {
        eprintln!("skipped: set TEST_DATABASE_URL");
        return;
    };
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_shopcore"))
        .arg("serve")
        .env("DATABASE_URL", &db)
        .env("BIND_ADDR", format!("127.0.0.1:{port}"))
        .env("RUST_LOG", "warn")
        .spawn()
        .unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let healthy = rt.block_on(async {
        for _ in 0..100 {
            if let Ok(r) = reqwest::get(format!("http://127.0.0.1:{port}/health")).await {
                return r.status().is_success();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    });
    // SIGTERM exercises the graceful-shutdown path
    // (shell built-in `kill`: slim images may not ship /bin/kill)
    let sent = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -TERM {}", child.id()))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !sent {
        let _ = child.kill();
    }
    let status = child.wait().unwrap();
    assert!(healthy, "server did not become healthy");
    assert!(
        status.success(),
        "graceful shutdown exits 0, got {status:?}"
    );
}

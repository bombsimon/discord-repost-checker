use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::collections::{HashMap, HashSet};

#[derive(Serialize, Deserialize)]
struct LegacyRepostChecker {
    reposts: HashMap<String, Vec<(String, chrono::DateTime<chrono::Local>)>>,
    ignore_urls: HashSet<String>,
    always_enabled_urls: HashSet<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load existing JSON data
    let legacy_data: LegacyRepostChecker = match std::fs::File::open("checkpoint.json") {
        Ok(file) => serde_json::from_reader(file)?,
        Err(_) => {
            println!("No checkpoint.json found, nothing to migrate");
            return Ok(());
        }
    };

    // Check if database already exists - if so, assume migration already completed
    if std::path::Path::new("repost_checker.db").exists() {
        println!("Database already exists, migration already completed");
        return Ok(());
    }

    // Connect to SQLite database
    let options = SqliteConnectOptions::new()
        .filename("repost_checker.db")
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await?;

    // Create tables
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS reposts (
            url TEXT NOT NULL,
            user_id TEXT NOT NULL,
            posted_at TEXT NOT NULL
        );
        
        CREATE TABLE IF NOT EXISTS ignore_hosts (
            host TEXT PRIMARY KEY
        );
        
        CREATE TABLE IF NOT EXISTS always_enabled_hosts (
            host TEXT PRIMARY KEY
        );
        
        CREATE INDEX IF NOT EXISTS idx_reposts_url ON reposts(url);
        "#,
    )
    .execute(&pool)
    .await?;

    // Migrate reposts
    for (url, posts) in legacy_data.reposts {
        for (user_id, posted_at) in posts {
            let posted_at_str = posted_at.format("%Y-%m-%d %H:%M:%S").to_string();
            sqlx::query("INSERT INTO reposts (url, user_id, posted_at) VALUES (?, ?, ?)")
                .bind(&url)
                .bind(user_id.to_string())
                .bind(&posted_at_str)
                .execute(&pool)
                .await?;
        }
    }

    // Migrate ignore_hosts
    for host in legacy_data.ignore_urls {
        sqlx::query("INSERT OR IGNORE INTO ignore_hosts (host) VALUES (?)")
            .bind(&host)
            .execute(&pool)
            .await?;
    }

    // Migrate always_enabled_hosts
    for host in legacy_data.always_enabled_urls {
        sqlx::query("INSERT OR IGNORE INTO always_enabled_hosts (host) VALUES (?)")
            .bind(&host)
            .execute(&pool)
            .await?;
    }

    println!("Migration completed successfully!");
    println!("You can now delete checkpoint.json and use the SQLite database");

    Ok(())
}

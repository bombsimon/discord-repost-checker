use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;

const DB_FILE: &str = "reposts.db";
const CHECKPOINT_FILE: &str = "checkpoint.json";

#[derive(Debug, Deserialize)]
struct RepostChecker {
    reposts: HashMap<String, Vec<(String, String)>>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(CHECKPOINT_FILE)?;
    let reader = BufReader::new(file);
    let repost_checker: RepostChecker = serde_json::from_reader(reader)?;

    let conn = rusqlite::Connection::open(DB_FILE)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS reposts (
            url TEXT NOT NULL,
            user_id INTEGER NOT NULL,
            posted_at TEXT NOT NULL
        )",
        (),
    )?;

    for (url, reposts) in repost_checker.reposts {
        for (user_id, posted_at) in reposts {
            let user_id: u64 = user_id.parse()?;
            conn.execute(
                "INSERT INTO reposts (url, user_id, posted_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![url, user_id, posted_at],
            )?;
        }
    }

    println!("Migration completed successfully!");

    Ok(())
}

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Game {
    pub id: i64,
    pub name: String,
    pub exe_names: Vec<String>,
    pub path: Option<PathBuf>,
    pub enabled: bool,
}

pub fn list(conn: &Connection) -> Result<Vec<Game>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, exe_names_json, path, enabled FROM games ORDER BY name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |r| {
        let exes: String = r.get(2)?;
        let path: Option<String> = r.get(3)?;
        Ok(Game {
            id: r.get(0)?,
            name: r.get(1)?,
            exe_names: serde_json::from_str(&exes).unwrap_or_default(),
            path: path.map(PathBuf::from),
            enabled: r.get::<_, i64>(4)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Insert a game, or update exe names / path of the existing game with this name.
/// Returns the game id. `enabled` is left untouched for existing games.
pub fn upsert_by_name(
    conn: &Connection,
    name: &str,
    exe_names: &[String],
    path: Option<&Path>,
) -> Result<i64> {
    let exes = serde_json::to_string(exe_names)?;
    let path = path.map(|p| p.to_string_lossy().into_owned());
    conn.execute(
        "INSERT INTO games (name, exe_names_json, path) VALUES (?1, ?2, ?3)
         ON CONFLICT(name) DO UPDATE SET exe_names_json = excluded.exe_names_json, path = excluded.path",
        params![name, exes, path],
    )?;
    Ok(conn.query_row("SELECT id FROM games WHERE name = ?1", [name], |r| r.get(0))?)
}

pub fn set_enabled(conn: &Connection, id: i64, enabled: bool) -> Result<()> {
    conn.execute(
        "UPDATE games SET enabled = ?1 WHERE id = ?2",
        params![enabled as i64, id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;

    #[test]
    fn games_upsert_list_enable() {
        let conn = open_memory().unwrap();
        let id = upsert_by_name(&conn, "Rocket League", &["RocketLeague.exe".into()], None).unwrap();
        let id2 = upsert_by_name(&conn, "Rocket League", &["RocketLeague.exe".into(), "rl2.exe".into()], Some(r"C:\g".as_ref())).unwrap();
        assert_eq!(id, id2);
        let l = list(&conn).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].exe_names.len(), 2);
        assert!(l[0].enabled);
        set_enabled(&conn, id, false).unwrap();
        assert!(!list(&conn).unwrap()[0].enabled);
    }
}

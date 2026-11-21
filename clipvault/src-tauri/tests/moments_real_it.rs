//! Manual real-data check of the Moments import. Ignored by default.
//!
//! CV_GG_DB=<copy of database.db> CV_OUT=<empty scratch dir> CV_FFMPEG=<ffmpeg.exe> \
//!   CV_THUMBS=none|newest3|all cargo test --test moments_real_it -- --ignored --nocapture
//! Writes only into CV_OUT (a scratch library), never the real ClipVault data dir.

use clipvault_lib::db::{self, clips};
use clipvault_lib::import_moments::{import_moments, ImportOptions};
use clipvault_lib::settings::Settings;
use std::path::PathBuf;

#[test]
#[ignore]
fn real_import() {
    let gg = PathBuf::from(std::env::var("CV_GG_DB").unwrap());
    let out = PathBuf::from(std::env::var("CV_OUT").unwrap());
    let ffmpeg = PathBuf::from(std::env::var("CV_FFMPEG").unwrap());
    let mode = std::env::var("CV_THUMBS").unwrap_or_else(|_| "none".into());
    let conn = db::open(&out.join("library.db")).unwrap();
    let opts = ImportOptions {
        generate_thumbnails: mode != "none",
        thumbs_dir: out.join("thumbs"),
        thumbnail_limit: if mode == "newest3" { Some(3) } else { None },
    };
    let mut settings = Settings::default();
    let r = import_moments(&gg, &conn, &mut settings, &ffmpeg, &opts, |_, _| {}).unwrap();
    println!(
        "REPORT imported={} skipped_existing={} missing={} games={} settings_changed={}",
        r.imported,
        r.skipped_existing,
        r.missing_files.len(),
        r.games_imported,
        r.settings_changed
    );
    let all = clips::list(&conn, Default::default()).unwrap();
    println!("library clips={} with_thumbnail={}", all.len(), all.iter().filter(|c| c.thumbnail_path.is_some()).count());
    for c in all.iter().filter(|c| c.game == "Rocket League").take(3) {
        println!(
            "RL {:?} recorded_at={} markers={:?} thumb={:?} trim={:?}/{:?}",
            c.title, c.recorded_at, c.markers, c.thumbnail_path.is_some(), c.trim_in, c.trim_out
        );
    }
    for g in db::games::list(&conn).unwrap().iter().take(0) {
        println!("{g:?}");
    }
    println!("games in library={}", db::games::list(&conn).unwrap().len());
}

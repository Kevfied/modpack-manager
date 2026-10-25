// Replays the user's real connect.json through the publish path:
// resume the saved host, dump what the UI would see, then publish
// with the same args the dialog sends.
use app_lib::sync;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn main() {
    let store = PathBuf::from(r"C:\Users\ACER\AppData\Local\Temp\pubtest\connect.json");
    let hs: sync::SharedHost = Arc::new(Mutex::new(None));
    let cs: sync::SharedClient = Arc::new(Mutex::new(None));
    let synced_root = PathBuf::from(r"C:\Users\ACER\AppData\Local\Temp\pubtest\_synced");
    std::fs::create_dir_all(&synced_root).unwrap();

    sync::resume(&hs, &cs, &store, &synced_root);
    println!("== after resume ==");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let http = reqwest::Client::new();
    rt.block_on(async {
        match sync::connect_status(&hs, &cs, &http, &store).await {
            Ok(s) => println!("status: {}", serde_json::to_string_pretty(&s).unwrap()),
            Err(e) => println!("status ERR: {e:#}"),
        }
    });

    println!("== publish (dialog args) ==");
    let r = sync::host_publish(
        &hs, &store, "ui test", None, false, false,
        "modderSync", "1.0", "1.21.1", "neoforge", "",
    );
    match r {
        Ok(m) => println!("publish OK: {m}"),
        Err(e) => println!("publish ERR: {e:#}"),
    }
    // And the force path:
    let r2 = sync::host_publish(
        &hs, &store, "forced", None, false, true,
        "", "", "", "", "",
    );
    match r2 {
        Ok(m) => println!("force OK: {m}"),
        Err(e) => println!("force ERR: {e:#}"),
    }

    // What does the dialog's suggest actually return on the real folder?
    println!("== suggest_pack_meta on real folder ==");
    let res = app_lib::scanner::scan_folder_meta(
        r"C:\Users\ACER\Documents\Mods\modderSync",
    )
    .unwrap();
    let (mc, loader) = sync::suggest_meta(&res.mods);
    println!("mods: {} -> mc={mc:?} loader={loader:?}", res.mods.len());
    for m in res.mods.iter().take(6) {
        println!(
            "  {} loaders={:?} gv={:?} err={:?}",
            m.filename, m.loaders, m.game_versions, m.parse_error
        );
    }
}

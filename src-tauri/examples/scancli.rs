// Quick CLI to sanity-check the scanner + Modrinth resolution:
//   cargo run --example scancli -- <folder> [--resolve]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(dir) = args.get(1) else {
        eprintln!("usage: scancli <folder> [--resolve]");
        std::process::exit(2);
    };
    let t0 = std::time::Instant::now();
    let res = app_lib::scanner::scan_folder_progress(dir, |done, total| {
        eprintln!("  [{done}/{total}] after {:?}", t0.elapsed());
    })
    .unwrap();
    eprintln!("scan took {:?}", t0.elapsed());
    println!("{} jar(s), {} bytes", res.mods.len(), res.total_size);
    for m in &res.mods {
        println!(
            "- {} | id={:?} name={:?} ver={:?} loaders={:?} mc={:?} err={:?}",
            m.filename, m.mod_id, m.name, m.version, m.loaders, m.game_versions, m.parse_error
        );
    }
    if args.iter().any(|a| a == "--resolve") {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = app_lib::modrinth::ModrinthClient::new();
            let hashes: Vec<String> = res.mods.iter().map(|m| m.sha1.clone()).filter(|h| !h.is_empty()).collect();
            let map = client.resolve_hashes(&hashes).await.unwrap();
            println!("\n{} matched on Modrinth:", map.len());
            for r in map.values() {
                println!(
                    "- {:?} {:?} loaders={:?} mc={:?}",
                    r.project_title, r.version_number, r.loaders, r.game_versions
                );
            }
        });
    }
}

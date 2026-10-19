//! End-to-end test of pack-file loading: builds a minimal .mrpack and .zip in
//! a temp dir, installs them into a temp .minecraft, and prints what landed.
//! Usage: cargo run --example packtest
use std::io::Write;
use std::path::PathBuf;
use zip::write::SimpleFileOptions;

fn make_mrpack(path: &PathBuf) {
    let index = serde_json::json!({
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": "1.0",
        "name": "Test Pack",
        "summary": "packtest",
        "files": [],
        "dependencies": {
            "minecraft": "1.21.1",
            "fabric-loader": "0.15.11"
        }
    });
    let f = std::fs::File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let opt = SimpleFileOptions::default();
    zw.start_file("modrinth.index.json", opt).unwrap();
    zw.write_all(index.to_string().as_bytes()).unwrap();
    zw.start_file("overrides/config/test.cfg", opt).unwrap();
    zw.write_all(b"test=true").unwrap();
    zw.finish().unwrap();
}

fn make_mods_zip(path: &PathBuf) {
    let f = std::fs::File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let opt = SimpleFileOptions::default();
    zw.start_file("mods/fake-mod.jar", opt).unwrap();
    zw.write_all(b"PK fake jar").unwrap();
    zw.start_file("config/opt.cfg", opt).unwrap();
    zw.write_all(b"x=1").unwrap();
    zw.finish().unwrap();
}

fn check(cond: bool, msg: &str) {
    println!("  [{}] {}", if cond { "OK" } else { "FAIL" }, msg);
}

fn main() {
    let base = std::env::temp_dir().join("mm-packtest");
    let _ = std::fs::remove_dir_all(&base);
    let mc = base.join(".minecraft");
    let root = base.join("instances");
    std::fs::create_dir_all(&mc).unwrap();
    std::fs::create_dir_all(&root).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let client = app_lib::modrinth::ModrinthClient::new();

        // --- .mrpack → vanilla ---
        println!("== mrpack -> vanilla ==");
        let mrpack = base.join("test.mrpack");
        make_mrpack(&mrpack);
        match app_lib::launchers::install_mrpack_vanilla(
            &client,
            mrpack.to_str().unwrap(),
            &mc,
            &root,
        )
        .await
        {
            Ok(log) => {
                println!("{log}");
                let inst = root.join("test-pack");
                check(inst.join("config/test.cfg").is_file(), "override applied");
                check(
                    mc.join("versions/fabric-loader-0.15.11-1.21.1").is_dir(),
                    "fabric installed in versions/",
                );
                check(mc.join("launcher_profiles.json").is_file(), "profile written");
                check(mrpack.is_file(), "source mrpack preserved");
            }
            Err(e) => println!("  FAIL install: {e}"),
        }

        // --- .zip → vanilla ---
        println!("== zip -> vanilla ==");
        let zpath = base.join("mods.zip");
        make_mods_zip(&zpath);
        let cfg = app_lib::pack::PackConfig {
            name: "Zip Test".into(),
            version: "1.0".into(),
            summary: String::new(),
            mc_version: "1.21.1".into(),
            loader: "fabric".into(),
            loader_version: "0.15.11".into(),
            selected_paths: vec![],
        };
        match app_lib::launchers::install_zip_vanilla(
            &client,
            zpath.to_str().unwrap(),
            &mc,
            &root,
            &cfg,
        )
        .await
        {
            Ok(log) => {
                println!("{log}");
                let inst = root.join("zip-test");
                check(inst.join("mods/fake-mod.jar").is_file(), "jar extracted");
                check(inst.join("config/opt.cfg").is_file(), "config extracted");
            }
            Err(e) => println!("  FAIL install: {e}"),
        }

        // --- .mrpack → prism ---
        println!("== mrpack -> prism ==");
        let prism2 = base.join("prism-data2");
        match app_lib::launchers::install_mrpack_prism(
            mrpack.to_str().unwrap(),
            prism2.to_str().unwrap(),
        )
        .await
        {
            Ok(log) => {
                println!("{log}");
                let inst = prism2.join("instances/test-pack");
                check(inst.join(".minecraft/config/test.cfg").is_file(), "override staged");
                check(inst.join("mmc-pack.json").is_file(), "mmc-pack written");
                let pack: serde_json::Value = serde_json::from_str(
                    &std::fs::read_to_string(inst.join("mmc-pack.json")).unwrap(),
                )
                .unwrap();
                check(
                    pack["components"]
                        .as_array()
                        .map(|c| c.iter().any(|x| x["uid"] == "net.fabricmc.fabric-loader"))
                        .unwrap_or(false),
                    "fabric component in mmc-pack",
                );
            }
            Err(e) => println!("  FAIL install: {e}"),
        }

        // --- .zip → prism (fake data dir) ---
        println!("== zip -> prism ==");
        let prism = base.join("prism-data");
        match app_lib::launchers::install_zip_prism(
            zpath.to_str().unwrap(),
            prism.to_str().unwrap(),
            &cfg,
        ) {
            Ok(log) => {
                println!("{log}");
                let inst = prism.join("instances/zip-test");
                check(inst.join(".minecraft/mods/fake-mod.jar").is_file(), "jar staged");
                check(inst.join("mmc-pack.json").is_file(), "mmc-pack written");
                check(inst.join("instance.cfg").is_file(), "instance.cfg written");
            }
            Err(e) => println!("  FAIL install: {e}"),
        }
    });

    println!("done — temp dir: {}", base.display());
}

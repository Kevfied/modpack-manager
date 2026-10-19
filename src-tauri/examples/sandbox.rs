//! Disposable end-to-end test of the Connect sync system.
//! Runs host + client in one process over 127.0.0.1; everything lives in
//! %TEMP%/mm-sandbox-<pid> which is deleted at the end — nothing on the real
//! machine is touched.
//!
//! Usage: cargo run --example sandbox

use app_lib::{appupdate, compat, launchers, modrinth, pack, scanner, sync};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use zip::write::SimpleFileOptions;

/// Build a signed update bundle dir: payload + latest.json + latest.json.minisig.
/// `overrides` rewrites top-level manifest fields (for hostile-manifest tests).
fn make_bundle(
    dir: &Path,
    ver: &str,
    fname: &str,
    payload: &[u8],
    kp: &minisign::KeyPair,
    overrides: &[(&str, &str)],
) -> String {
    use sha2::Digest;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(fname), payload).unwrap();
    let mut j = serde_json::json!({
        "app": "com.modpackmanager.app",
        "version": ver,
        "channel": "stable",
        "arch": std::env::consts::ARCH,
        "notes": "test notes",
        "pubDate": "2024-01-01T00:00:00Z",
        "file": fname,
        "sha256": hex::encode(sha2::Sha256::digest(payload)),
        "size": payload.len(),
    });
    for (k, v) in overrides {
        j[*k] = serde_json::json!(v);
    }
    let man = j.to_string();
    let sig = minisign::sign(
        Some(&kp.pk),
        &kp.sk,
        std::io::Cursor::new(man.as_bytes()),
        None,
        None,
    )
    .unwrap()
    .to_string();
    std::fs::write(dir.join("latest.json"), &man).unwrap();
    std::fs::write(dir.join("latest.json.minisig"), &sig).unwrap();
    man
}

static mut FAILURES: u32 = 0;

fn check(cond: bool, msg: &str) {
    println!("  [{}] {}", if cond { "OK" } else { "FAIL" }, msg);
    if !cond {
        unsafe { FAILURES += 1 };
    }
}

/// Fake jar: a zip with a fabric.mod.json declaring `id`.
fn make_jar(path: &Path, id: &str, extra: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let f = std::fs::File::create(path).unwrap();
    let mut z = zip::ZipWriter::new(f);
    let opt = SimpleFileOptions::default();
    z.start_file("fabric.mod.json", opt).unwrap();
    z.write_all(
        format!(r#"{{"id":"{id}","name":"{id}","version":"{extra}","depends":{{"minecraft":"1.21.1"}}}}"#)
            .as_bytes(),
    )
    .unwrap();
    z.start_file("data.bin", opt).unwrap();
    z.write_all(extra.as_bytes()).unwrap();
    z.finish().unwrap();
}

fn main() {
    let base = std::env::temp_dir().join(format!("mm-sandbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let host_mods = base.join("host-mods");
    let host_mods2 = base.join("host-mods-2");
    let store_path = base.join("connect.json");
    let sync_root = base.join("instances").join("_synced");
    let inst_root = base.join("instances");
    let mc_dir = base.join(".minecraft");
    let prism_dir = base.join("prism-data");
    std::fs::create_dir_all(&mc_dir).unwrap();

    let hs: sync::SharedHost = Arc::new(Mutex::new(None));
    let cs: sync::SharedClient = Arc::new(Mutex::new(None));
    let http = reqwest::Client::new();
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Seed the host folder with 3 jars.
    make_jar(&host_mods.join("alpha.jar"), "alpha", "1.0");
    make_jar(&host_mods.join("beta.jar"), "beta", "1.0");
    make_jar(&host_mods.join("gamma.jar"), "gamma", "1.0");

    let addr = format!("127.0.0.1:{}", sync::HTTP_PORT);

    rt.block_on(async {
        // ---- 1. host start + /info ----
        println!("== host start ==");
        let log = sync::host_start(
            &hs,
            &store_path,
            "sandbox-net",
            "letmein",
            host_mods.to_str().unwrap(),
            "Sandbox Pack",
            "1.21.1",
            "fabric",
            "0.15.11",
        )
        .unwrap();
        println!("{log}");
        std::thread::sleep(std::time::Duration::from_millis(300)); // let server bind
        let info: serde_json::Value = http
            .get(format!("http://{addr}/info"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        check(info["name"] == "sandbox-net", "server binds + /info answers");
        check(info["latestVersion"] == 0, "STRICT: nothing published until button pressed");
        sync::host_publish(&hs, &store_path, "first", None, false).unwrap();
        let info: serde_json::Value = http
            .get(format!("http://{addr}/info"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        check(info["latestVersion"] == 1, "manual publish creates v1");

        // ---- 2. join code gate ----
        println!("== join code ==");
        let cs2: sync::SharedClient = Arc::new(Mutex::new(None));
        let bad = sync::join_network(&cs2, &store_path, &sync_root, &addr, "wrong", "bob")
            .await;
        check(bad.is_err(), "wrong code rejected");
        let ok = sync::join_network(&cs, &store_path, &sync_root, &addr, "letmein", "alice")
            .await;
        check(ok.is_ok(), "right code accepted");
        if let Err(e) = &ok {
            println!("    join err: {e}");
        }

        // ---- 3. pending update shows ----
        println!("== update check ==");
        let pend = sync::check_pending(&cs, &http).await.unwrap();
        check(pend.is_some(), "pending update v1 visible");
        let (m, _, _) = pend.unwrap();
        check(m.version == 1 && m.files.len() == 3, "manifest has 3 files");

        // ---- 4. apply v1 ----
        println!("== apply v1 ==");
        let log = sync::apply_update(&cs, &http, &store_path).await.unwrap();
        println!("{log}");
        let nid = { cs.lock().unwrap().as_ref().unwrap().synced_dir.clone() };
        check(nid.join("mods/alpha.jar").is_file(), "alpha.jar downloaded");
        check(nid.join("mods/beta.jar").is_file(), "beta.jar downloaded");
        check(nid.join("pack.json").is_file(), "pack.json written");
        let pj: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(nid.join("pack.json")).unwrap())
                .unwrap();
        check(pj["mcVersion"] == "1.21.1" && pj["loader"] == "fabric", "MC/loader meta in pack.json");

        // ---- 5. delta apply (edit 1, add 1, delete 1) ----
        println!("== delta apply ==");
        make_jar(&host_mods.join("alpha.jar"), "alpha", "2.0-changed");
        make_jar(&host_mods.join("delta.jar"), "delta", "1.0");
        std::fs::remove_file(host_mods.join("gamma.jar")).unwrap();
        sync::host_publish(&hs, &store_path, "delta test", None, false).unwrap();
        let (m2, dl, del) = sync::check_pending(&cs, &http).await.unwrap().unwrap();
        check(m2.version == 2, "v2 pending");
        check(dl == 2, &format!("2 files to download (got {dl})"));
        check(del == 1, &format!("1 file to delete (got {del})"));
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        check(!nid.join("mods/gamma.jar").exists(), "gamma.jar removed (mirror)");
        check(nid.join("mods/delta.jar").is_file(), "delta.jar added");

        // ---- 6. decline ----
        println!("== decline ==");
        sync::host_publish(&hs, &store_path, "", None, false).unwrap(); // no changes → guard
        let g = sync::host_publish(&hs, &store_path, "", None, false).unwrap();
        check(g.contains("nothing changed"), "no-change publish guarded");
        make_jar(&host_mods.join("beta.jar"), "beta", "2.0-changed");
        sync::host_publish(&hs, &store_path, "v3 skip me", None, false).unwrap();
        sync::decline_update(&cs, &store_path, 3).unwrap();
        check(
            sync::check_pending(&cs, &http).await.unwrap().is_none(),
            "declined v3 hidden",
        );
        // declined list is persisted to connect.json
        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
        check(
            store["client"]["declined"].as_array().unwrap().contains(&serde_json::json!(3)),
            "declined versions persisted to disk",
        );
        make_jar(&host_mods.join("eps.jar"), "eps", "1.0");
        sync::host_publish(&hs, &store_path, "v4", None, false).unwrap();
        check(
            sync::check_pending(&cs, &http).await.unwrap().is_some(),
            "newer v4 still shows after declining v3",
        );
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        check(
            sync::check_pending(&cs, &http).await.unwrap().is_none(),
            "no pending after applying latest",
        );
        // ---- 6a2. re-push same content to a member who declined (targeted) ----
        println!("== targeted re-push after decline ==");
        let alice_mid0 = cs.lock().unwrap().as_ref().unwrap().member_id;
        make_jar(&host_mods.join("decoy.jar"), "decoy", "1.0");
        sync::host_publish(&hs, &store_path, "v5 decoy", None, false).unwrap();
        sync::decline_update(&cs, &store_path, 5).unwrap();
        check(
            sync::check_pending(&cs, &http).await.unwrap().is_none(),
            "declined v5 hidden",
        );
        // Same files, but targeted at alice → must publish (not "nothing changed")
        let t2 = sync::host_publish(&hs, &store_path, "try again alice", Some(alice_mid0), false)
            .unwrap();
        check(t2.contains("v6"), &format!("targeted re-push allowed ({t2})"));
        check(
            sync::check_pending(&cs, &http).await.unwrap().is_some(),
            "declined member sees targeted re-push",
        );
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        // her heartbeat retires the target
        let _ = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "letmein")
            .json(&serde_json::json!({"memberId": alice_mid0, "name": "alice", "appliedVersion": 6}))
            .send()
            .await
            .unwrap();

        // ---- 6b. change folder mid-host ----
        println!("== change folder ==");
        make_jar(&host_mods2.join("alpha.jar"), "alpha", "2.0-changed"); // same hash
        make_jar(&host_mods2.join("newpack.jar"), "newpack", "1.0");
        let (same_pct, count) =
            sync::host_set_folder(&hs, &store_path, host_mods2.to_str().unwrap()).unwrap();
        check(count == 2, &format!("new folder has {count} files"));
        check(same_pct < 60, &format!("low overlap flagged ({same_pct}%)"));
        sync::host_publish(&hs, &store_path, "folder swap", None, false).unwrap();
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        check(
            !nid.join("mods/beta.jar").exists() && nid.join("mods/newpack.jar").is_file(),
            "client mirrors the NEW folder exactly",
        );

        // ---- 6c. targeted publish (member-only) ----
        println!("== targeted publish ==");
        let alice_mid = cs.lock().unwrap().as_ref().unwrap().member_id;
        make_jar(&host_mods2.join("secret.jar"), "secret", "1.0");
        let t = sync::host_publish(&hs, &store_path, "only alice", Some(alice_mid), false).unwrap();
        println!("{t}");
        check(t.contains("only"), "publish reports targeted");
        // bob (member 42) sees only the global manifest, not alice's target
        let bob_man: serde_json::Value = http
            .get(format!("http://{addr}/manifest"))
            .header("X-Join-Code", "letmein")
            .header("X-Member-Id", "42")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let global_v = bob_man["version"].as_u64().unwrap();
        check(global_v == 7, &format!("other members still on global v{global_v}"));
        // alice sees the targeted update
        let (tm, _, _) = sync::check_pending(&cs, &http).await.unwrap().unwrap();
        check(tm.version == 8, "targeted update visible only to alice");
        // member list flags her as targeted + behind
        let hb = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "letmein")
            .json(&serde_json::json!({"memberId": 42, "name": "bob", "appliedVersion": global_v}))
            .send()
            .await
            .unwrap();
        let hbv: serde_json::Value = hb.json().await.unwrap();
        let alice = hbv["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == alice_mid)
            .unwrap();
        check(alice["targeted"] == true, "member list flags alice as targeted");
        // alice applies → target retires, she's back on the global stream
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        check(nid.join("mods/secret.jar").is_file(), "targeted file downloaded");
        // her own heartbeat retires the target
        let hb2 = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "letmein")
            .json(&serde_json::json!({"memberId": alice_mid, "name": "alice", "appliedVersion": 8}))
            .send()
            .await
            .unwrap();
        let hbv2: serde_json::Value = hb2.json().await.unwrap();
        let alice2 = hbv2["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == alice_mid)
            .unwrap();
        check(alice2["targeted"] == false && alice2["upToDate"] == true,
            "target retired after apply — alice up to date");
        check(
            sync::check_pending(&cs, &http).await.unwrap().is_none(),
            "no phantom pending after targeted apply",
        );

        // ---- 6d. special characters + path traversal ----
        println!("== filename edge cases ==");
        make_jar(&host_mods2.join("mød+1.jar"), "modplus", "1.0");
        sync::host_publish(&hs, &store_path, "utf8 name", None, false).unwrap();
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        check(
            nid.join("mods/mød+1.jar").is_file(),
            "non-ASCII '+' filename synced intact",
        );
        let r = http
            .get(format!("http://{addr}/file?path=../connect.json"))
            .header("X-Join-Code", "letmein")
            .send()
            .await
            .unwrap();
        check(r.status().as_u16() == 400, "path traversal rejected");
        let r = http
            .get(format!("http://{addr}/manifest"))
            .header("X-Join-Code", "wrong")
            .send()
            .await
            .unwrap();
        check(r.status().as_u16() == 403, "manifest denied without code");

        // ---- 6e. empty publish guard ----
        println!("== empty publish ==");
        let empty_dir = base.join("empty-mods");
        std::fs::create_dir_all(&empty_dir).unwrap();
        sync::host_set_folder(&hs, &store_path, empty_dir.to_str().unwrap()).unwrap();
        let r = sync::host_publish(&hs, &store_path, "empty", None, false);
        check(
            r.is_err() && r.unwrap_err().to_string().contains("EMPTY_PACK"),
            "empty folder publish blocked",
        );
        sync::host_publish(&hs, &store_path, "empty ok", None, true).unwrap();
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        let left = std::fs::read_dir(nid.join("mods"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "jar").unwrap_or(false))
            .count();
        check(left == 0, "empty publish wiped member mods (mirror)");
        // restore real folder so later sections have content
        sync::host_set_folder(&hs, &store_path, host_mods2.to_str().unwrap()).unwrap();
        sync::host_publish(&hs, &store_path, "restore", None, false).unwrap();
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        check(nid.join("mods/alpha.jar").is_file(), "folder restored, files back");
    });

    // ---- 7. tampered local file → flagged for re-download on next apply ----
    rt.block_on(async {
        println!("== tamper detection ==");
        make_jar(&host_mods2.join("zeta.jar"), "zeta", "1.0");
        sync::host_publish(&hs, &store_path, "v7", None, false).unwrap();
        let nid = cs.lock().unwrap().as_ref().unwrap().synced_dir.clone();
        std::fs::write(nid.join("mods/alpha.jar"), b"tampered").unwrap();
        let (_, dl, _) = sync::check_pending(&cs, &http).await.unwrap().unwrap();
        check(dl >= 2, &format!("tampered file flagged for re-download ({dl} to fetch)"));
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        check(
            std::fs::metadata(nid.join("mods/alpha.jar")).unwrap().len() > 9,
            "tampered file restored",
        );

        // ---- 8. .part leftovers cleaned ----
        println!("== .part cleanup ==");
        std::fs::write(nid.join("mods/stale.part"), b"x").unwrap();
        make_jar(&host_mods2.join("eta.jar"), "eta", "1.0");
        sync::host_publish(&hs, &store_path, "v8", None, false).unwrap();
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        check(!nid.join("mods/stale.part").exists(), "stale .part cleaned");

        // ---- 9. member list ----
        println!("== members ==");
        let r = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "letmein")
            .json(&serde_json::json!({"memberId": 42, "name": "bob", "appliedVersion": 0}))
            .send()
            .await
            .unwrap();
        let v: serde_json::Value = r.json().await.unwrap();
        let members = v["members"].as_array().unwrap();
        check(members.len() >= 2, &format!("member list has {} entries", members.len()));
        let bob = members.iter().find(|m| m["name"] == "bob").unwrap();
        check(bob["online"] == true && bob["upToDate"] == false, "bob shows online + behind");

        // ---- 9b. kick + rejoin ----
        println!("== kick ==");
        sync::host_kick(&hs, &store_path, 42).unwrap();
        let r = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "letmein")
            .header("X-Member-Id", "42")
            .json(&serde_json::json!({"memberId": 42, "name": "bob", "appliedVersion": 0}))
            .send()
            .await
            .unwrap();
        let v: serde_json::Value = r.json().await.unwrap();
        check(v["kicked"] == true, "kicked member's heartbeat returns kicked flag");
        check(
            v["members"].as_array().unwrap().iter().all(|m| m["id"] != 42),
            "kicked member removed from list",
        );
        // kicked member is locked out of file/manifest endpoints even WITH the code
        let r = http
            .get(format!("http://{addr}/manifest"))
            .header("X-Join-Code", "letmein")
            .header("X-Member-Id", "42")
            .send()
            .await
            .unwrap();
        check(r.status().as_u16() == 403, "kicked member: manifest blocked despite valid code");
        let r = http
            .get(format!("http://{addr}/file?path=alpha.jar"))
            .header("X-Join-Code", "letmein")
            .header("X-Member-Id", "42")
            .send()
            .await
            .unwrap();
        check(r.status().as_u16() == 403, "kicked member: file download blocked");
        // rejoin clears the kick
        let r = http
            .post(format!("http://{addr}/join"))
            .json(&serde_json::json!({"code": "letmein", "memberId": 42, "name": "bob"}))
            .send()
            .await
            .unwrap();
        check(r.status().is_success(), "kicked member can rejoin with code");
        let r = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "letmein")
            .header("X-Member-Id", "42")
            .json(&serde_json::json!({"memberId": 42, "name": "bob", "appliedVersion": 0}))
            .send()
            .await
            .unwrap();
        let v: serde_json::Value = r.json().await.unwrap();
        check(v["kicked"].is_null(), "rejoined member heartbeats normally");

        // ---- 9c. edit network / code rotation ----
        println!("== edit network ==");
        sync::host_update(&hs, &store_path, "sandbox-net-v2", "newcode").unwrap();
        let info: serde_json::Value = http
            .get(format!("http://{addr}/info"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        check(info["name"] == "sandbox-net-v2", "network renamed live");
        let cs4: sync::SharedClient = Arc::new(Mutex::new(None));
        let old = sync::join_network(&cs4, &store_path, &sync_root, &addr, "letmein", "charlie")
            .await;
        check(old.is_err(), "old code rejected for new joins");
        let new = sync::join_network(&cs4, &store_path, &sync_root, &addr, "newcode", "charlie")
            .await;
        check(new.is_ok(), "new code accepted");
        // existing members keep working via member-id auth despite code rotation
        check(
            sync::check_pending(&cs, &http).await.is_ok(),
            "existing member unaffected by code rotation",
        );
        // Renames propagate to members through their heartbeat — wait for it.
        let mut renamed = false;
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if cs.lock().unwrap().as_ref().unwrap().network_name == "sandbox-net-v2" {
                renamed = true;
                break;
            }
        }
        check(renamed, "network rename propagated to member via heartbeat");

        // Re-save alice's session — charlie's join overwrote the client slot.
        sync::apply_update(&cs, &http, &store_path).await.unwrap();

        // ---- 9d. saved host state in connect.json ----
        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
        check(
            store["host"]["manifest"]["version"].as_u64().unwrap() >= 10,
            "published manifest persisted in connect.json",
        );
        check(store["host"]["code"] == "newcode", "rotated code persisted");

        // ---- 10. persistence / resume ----
        println!("== persistence ==");
        let cs2: sync::SharedClient = Arc::new(Mutex::new(None));
        let hs2: sync::SharedHost = Arc::new(Mutex::new(None));
        // can't re-host on same port — client resume only
        sync::resume(&hs2, &cs2, &store_path, &sync_root);
        {
            let g = cs2.lock().unwrap();
            check(g.is_some(), "client session restored from connect.json");
            if let Some(c) = g.as_ref() {
                check(c.applied_version > 0, "applied version persisted");
            }
        }

        // ---- 11. synced pack → loaders ----
        println!("== synced pack -> loader ==");
        let packs = sync::list_synced_packs(&inst_root);
        check(packs.len() == 1, "synced pack listed");
        check(
            packs[0].mc_version == "1.21.1" && packs[0].loader == "fabric",
            "synced pack carries MC/loader meta",
        );
        let mr_client = modrinth::ModrinthClient::new();
        let vanilla = launchers::LauncherInfo {
            kind: "vanilla".into(),
            name: "MC".into(),
            path: None,
            data_dir: None,
        };
        // vanilla load needs minecraft_dir() — real one. Point load via a temp
        // trick: run it against real .minecraft? No — instead test the prism path
        // which takes data_dir, and manually verify vanilla logic via pack_exists.
        let prism = launchers::LauncherInfo {
            kind: "prism".into(),
            name: "Prism".into(),
            path: None,
            data_dir: Some(prism_dir.to_string_lossy().into_owned()),
        };
        match sync::load_synced_pack(&mr_client, &inst_root, &packs[0].network_id, &prism).await {
            Ok(l) => {
                println!("{l}");
                let inst = prism_dir.join("instances").join(&packs[0].name.to_lowercase().replace(' ', "-"));
                check(inst.join("mmc-pack.json").is_file(), "prism instance written");
                check(inst.join(".minecraft/mods/alpha.jar").is_file(), "mods copied into prism instance");
            }
            Err(e) => {
                check(false, &format!("load_synced_pack prism failed: {e}"));
            }
        }
        check(
            launchers::pack_instance_exists(&prism, &packs[0].name),
            "overwrite detection sees the new instance",
        );
        let _ = vanilla; // vanilla path needs a real .minecraft — covered by packtest
    });

    // ---- 12. host offline ----
    println!("== host offline ==");
    sync::host_stop(&hs, &store_path);
    rt.block_on(async {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let r = sync::check_pending(&cs, &http).await;
        check(r.is_err(), "check fails when host is down");
        check(
            !cs.lock().unwrap().as_ref().unwrap().host_online,
            "host marked offline, network remembered",
        );
        check(
            cs.lock().unwrap().as_ref().unwrap().network_name == "sandbox-net-v2",
            "renamed network remembered while offline",
        );
    });
    {
        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
        check(store["host"].is_null(), "host_stop cleared saved host state");
    }

    // ---- 13. core app features (deterministic) ----
    rt.block_on(async {
    println!("== core: scan ==");
    let scan = scanner::scan_folder_progress(&host_mods2.to_string_lossy(), |_, _| {}).unwrap();
    check(scan.mods.len() == 6, &format!("scan_folder found {} jars", scan.mods.len()));
    check(
        scan.mods.iter().any(|m| m.mod_id.as_deref() == Some("alpha")),
        "jar metadata parsed (mod id)",
    );
    check(
        scan.mods.iter().all(|m| !m.sha1.is_empty() && !m.sha512.is_empty()),
        "all jars hashed (sha1+sha512)",
    );
    let scan2 = scanner::scan_paths(
        vec![
            host_mods2.join("alpha.jar"),
            host_mods2.join("not-a-jar.txt"),
            host_mods2.join("alpha.jar"), // dup — must dedup
        ],
        |_, _| {},
    )
    .unwrap();
    check(scan2.mods.len() == 1, "scan_paths filters non-jars + dedups");

    println!("== core: compat ==");
    let sel: std::collections::HashSet<String> =
        scan.mods.iter().map(|m| m.path.clone()).collect();
    let rep = compat::analyze(&scan.mods, &std::collections::HashMap::new(), &sel);
    check(
        rep.suggested_loader.as_deref() == Some("fabric"),
        &format!("suggested loader: {:?}", rep.suggested_loader),
    );
    check(
        rep.suggested_mc_version.as_deref() == Some("1.21.1"),
        &format!("suggested MC: {:?}", rep.suggested_mc_version),
    );
    check(rep.conflicts.is_empty(), "no conflicts in uniform set");

    println!("== core: build ==");
    let out_dir = base.join("built");
    std::fs::create_dir_all(&out_dir).unwrap();
    let cfg = pack::PackConfig {
        name: "Sandbox Pack".into(),
        version: "1.0".into(),
        summary: "test".into(),
        mc_version: "1.21.1".into(),
        loader: "fabric".into(),
        loader_version: "0.15.11".into(),
        selected_paths: sel.iter().cloned().collect(),
    };
    let b = pack::build_zip(&scan.mods, &cfg, out_dir.to_str().unwrap()).unwrap();
    {
        let f = std::fs::File::open(&b.output_path).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        check(z.len() == 6, &format!("zip has {} jar entries", z.len()));
        check(
            z.by_name("mods/alpha.jar").is_ok(),
            "zip entries are mods/<file>.jar",
        );
    }
    let b = pack::build_modlist(&scan.mods, &std::collections::HashMap::new(), &cfg, out_dir.to_str().unwrap()).unwrap();
    let ml = std::fs::read_to_string(&b.output_path).unwrap();
    check(ml.contains("alpha") && ml.contains("Sandbox Pack"), "modlist.txt written");
    let mr_client = modrinth::ModrinthClient::new();
    let b = pack::build_mrpack(&mr_client, &scan.mods, &cfg, out_dir.to_str().unwrap())
        .await
        .unwrap();
    let (mrpack_path, resolved_n) = (b.output_path, b.resolved_count);
    {
        let f = std::fs::File::open(&mrpack_path).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        let mut idx = String::new();
        use std::io::Read;
        z.by_name("modrinth.index.json").unwrap().read_to_string(&mut idx).unwrap();
        let idx: serde_json::Value = serde_json::from_str(&idx).unwrap();
        check(idx["name"] == "Sandbox Pack", "mrpack index has pack name");
        check(idx["dependencies"]["minecraft"] == "1.21.1", "mrpack index has MC dep");
        let bundled = (0..z.len()).filter(|i| z.by_index(*i).unwrap().name().starts_with("overrides/mods/")).count();
        println!("  (resolved online: {resolved_n}, bundled as overrides: {bundled})");
        check(bundled > 0 || resolved_n > 0, "mrpack carries all selected mods");
    }
    let info = launchers::pack_file_info(&mrpack_path).unwrap();
    check(
        info["name"] == "Sandbox Pack" && info["loader"] == "fabric" && info["mcVersion"] == "1.21.1",
        "pack_file_info reads mrpack metadata",
    );
    let info = launchers::pack_file_info(out_dir.join("sandbox-pack.zip").to_str().unwrap()).unwrap();
    check(info["name"] == "sandbox-pack", "pack_file_info: zip falls back to filename");

    println!("== core: loader installs (temp dirs) ==");
    let prism3 = base.join("prism-data-3");
    match launchers::install_mrpack_prism(&mrpack_path, prism3.to_str().unwrap()).await {
        Ok(l) => {
            println!("{l}");
            let inst = prism3.join("instances/sandbox-pack");
            check(inst.join("mmc-pack.json").is_file(), "mrpack → prism: mmc-pack.json");
            check(
                inst.join(".minecraft/mods/alpha.jar").is_file(),
                "mrpack → prism: override mods staged",
            );
        }
        Err(e) => check(false, &format!("mrpack → prism failed: {e}")),
    }
    let mc2 = base.join(".minecraft-2");
    let inst2 = base.join("instances-2");
    std::fs::create_dir_all(&mc2).unwrap();
    match launchers::install_mrpack_vanilla(&mr_client, &mrpack_path, &mc2, &inst2).await {
        Ok(l) => {
            println!("{l}");
            check(
                inst2.join("sandbox-pack").is_dir(),
                "mrpack → vanilla: instance dir staged",
            );
            check(
                inst2.join("sandbox-pack/mods/alpha.jar").is_file(),
                "mrpack → vanilla: override mods staged",
            );
            check(
                mc2.join("launcher_profiles.json").is_file(),
                "mrpack → vanilla: profile written",
            );
        }
        Err(e) => check(false, &format!("mrpack → vanilla failed: {e}")),
    }
    let zip_path = out_dir.join("sandbox-pack.zip");
    match launchers::install_zip_vanilla(&mr_client, zip_path.to_str().unwrap(), &mc2, &inst2, &cfg).await {
        Ok(l) => {
            println!("{l}");
            check(
                inst2.join("sandbox-pack/mods/alpha.jar").is_file(),
                "zip → vanilla: jars extracted",
            );
        }
        Err(e) => check(false, &format!("zip → vanilla failed: {e}")),
    }
    match launchers::install_zip_prism(zip_path.to_str().unwrap(), prism3.to_str().unwrap(), &cfg) {
        Ok(l) => {
            println!("{l}");
            check(
                prism3.join("instances/sandbox-pack/.minecraft/mods/newpack.jar").is_file(),
                "zip → prism: jars staged",
            );
        }
        Err(e) => check(false, &format!("zip → prism failed: {e}")),
    }
    // load_pack_file dispatch → prism
    let prism_l = launchers::LauncherInfo {
        kind: "prism".into(),
        name: "Prism".into(),
        path: None,
        data_dir: Some(prism3.to_string_lossy().into_owned()),
    };
    match launchers::load_pack_file(&mr_client, &prism_l, &mrpack_path, &cfg).await {
        Ok(_) => check(true, "load_pack_file dispatches mrpack → prism"),
        Err(e) => check(false, &format!("load_pack_file failed: {e}")),
    }
    // deploy_prism from a scanned selection
    let prism4 = base.join("prism-data-4");
    match launchers::deploy_prism(&mr_client, &scan.mods, &cfg, None, Some(prism4.to_str().unwrap())).await {
        Ok(l) => {
            println!("{l}");
            check(
                prism4.join("instances/sandbox-pack/.minecraft/mods/alpha.jar").is_file(),
                "deploy_prism: selection staged into instance",
            );
        }
        Err(e) => check(false, &format!("deploy_prism failed: {e}")),
    }
    check(
        launchers::pack_instance_exists(&prism_l, "Sandbox Pack"),
        "pack_instance_exists true after install",
    );

    println!("== modrinth api (needs network — soft checks) ==");
    match mr_client.game_versions().await {
        Ok(v) => check(!v.is_empty(), "game_versions returns list"),
        Err(e) => println!("  [SKIP] modrinth unreachable: {e}"),
    }
    match mr_client.loader_versions("fabric", "1.21.1").await {
        Ok(v) => check(!v.is_empty(), "loader_versions returns list"),
        Err(e) => println!("  [SKIP] modrinth unreachable: {e}"),
    }
    match mr_client.resolve_hashes(&[scan.mods[0].sha1.clone()]).await {
        Ok(_) => check(true, "resolve_hashes round-trips"),
        Err(e) => println!("  [SKIP] modrinth unreachable: {e}"),
    }

    println!("== discovery (soft) ==");
    sync::found_networks();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let found = sync::found_networks();
    if found.is_empty() {
        println!("  [SKIP] broadcast didn't loop back on this machine");
    } else {
        println!("  found {} network(s)", found.len());
    }
    });

    // ---- 14. app updates (LAN, standalone from Connect) ----
    rt.block_on(async {
        println!("== app updates ==");
        let alice = base.join("alice"); // this process's server seeds from here
        appupdate::init(&alice);
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let pk64 = kp.pk.to_base64();
        let payload = vec![b'X'; 40_000]; // fake installer bytes
        let fname = "Modpack-Manager_1.0.0_x64-setup.exe";
        let bundle_a = base.join("bundle-a");
        let man_a = make_bundle(&bundle_a, "1.0.0", fname, &payload, &kp, &[]);
        let sig_a = std::fs::read_to_string(bundle_a.join("latest.json.minisig")).unwrap();
        let m_a: appupdate::UpdateManifest = serde_json::from_str(&man_a).unwrap();

        // --- signature trust ---
        check(
            appupdate::verify_manifest_sig(man_a.as_bytes(), &sig_a, &pk64).is_ok(),
            "signed manifest verifies",
        );
        let forged = man_a.replace("\"1.0.0\"", "\"9.9.9\"");
        check(
            appupdate::verify_manifest_sig(forged.as_bytes(), &sig_a, &pk64).is_err(),
            "forged manifest version rejected (sig binds version)",
        );
        let kp2 = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        check(
            appupdate::verify_manifest_sig(man_a.as_bytes(), &sig_a, &kp2.pk.to_base64()).is_err(),
            "wrong-key signature rejected",
        );

        // --- manifest gate ---
        let cur = env!("CARGO_PKG_VERSION");
        check(
            appupdate::gate_manifest(&m_a, cur, None).is_ok(),
            "newer version passes gate",
        );
        check(
            appupdate::gate_manifest(&m_a, "1.0.0", None).is_err(),
            "equal version refused",
        );
        check(
            appupdate::gate_manifest(&m_a, "9.9.9", None).is_err(),
            "downgrade refused",
        );
        check(
            appupdate::gate_manifest(&m_a, cur, Some("1.0.0")).is_err(),
            "skipped version refused",
        );
        let mut m_beta = m_a.clone();
        m_beta.channel = "beta".into();
        check(
            appupdate::gate_manifest(&m_beta, cur, None).is_err(),
            "wrong channel refused",
        );
        let mut m_arch = m_a.clone();
        m_arch.arch = "aarch64".into();
        check(
            appupdate::gate_manifest(&m_arch, cur, None).is_err(),
            "wrong arch refused",
        );
        let mut m_fn = m_a.clone();
        m_fn.file = "../evil.exe".into();
        check(
            appupdate::gate_manifest(&m_fn, cur, None).is_err(),
            "traversal filename refused",
        );
        let mut m_app = m_a.clone();
        m_app.app = "com.other.app".into();
        check(
            appupdate::gate_manifest(&m_app, cur, None).is_err(),
            "foreign app id refused",
        );

        // --- seed pick (strict verify before anything goes live) ---
        let bad_bundle = base.join("bundle-bad");
        std::fs::create_dir_all(&bad_bundle).unwrap();
        std::fs::write(bad_bundle.join("latest.json"), &man_a).unwrap();
        std::fs::write(bad_bundle.join("latest.json.minisig"), &sig_a).unwrap();
        std::fs::write(bad_bundle.join(fname), b"CORRUPTED PAYLOAD").unwrap();
        check(
            appupdate::pick_update_bundle_pk(&hs, &alice, bad_bundle.to_str().unwrap(), &pk64)
                .is_err(),
            "payload hash mismatch rejected at pick",
        );
        check(
            !appupdate::seed_dir(&alice).exists(),
            "rejected bundle leaves no seed behind",
        );

        let r = appupdate::pick_update_bundle_pk(&hs, &alice, bundle_a.to_str().unwrap(), &pk64);
        check(r.is_ok(), &format!("signed bundle seeded: {:?}", r.err()));
        check(
            appupdate::seed_bundle_version(&alice).as_deref() == Some("1.0.0"),
            "seed advertises bundle version (not running version)",
        );
        check(
            !std::fs::read_dir(appupdate::seed_dir(&alice))
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".tmp")),
            "copy-then-activate leaves no .tmp orphans",
        );

        // --- served over the LAN HTTP ---
        std::thread::sleep(std::time::Duration::from_millis(300));
        let man_net = http
            .get(format!("http://{addr}/app/latest.json"))
            .send().await.unwrap().bytes().await.unwrap();
        let sig_net = http
            .get(format!("http://{addr}/app/latest.json.minisig"))
            .send().await.unwrap().text().await.unwrap();
        check(
            appupdate::verify_manifest_sig(&man_net, &sig_net, &pk64).is_ok(),
            "served manifest+sig verify",
        );
        let mnet: serde_json::Value = serde_json::from_slice(&man_net).unwrap();
        check(mnet["version"] == "1.0.0", "/app/latest.json serves the bundle");
        let r = http
            .get(format!("http://{addr}/app/file?name={fname}"))
            .send().await.unwrap();
        check(r.status().as_u16() == 200, "/app/file serves payload");
        let body = r.bytes().await.unwrap();
        check(body.as_ref() == payload.as_slice(), "payload bytes intact");
        let r = http
            .get(format!("http://{addr}/app/file?name={fname}"))
            .header("Range", "bytes=100-")
            .send().await.unwrap();
        let st206 = r.status().as_u16();
        let tail = r.bytes().await.unwrap();
        check(st206 == 206 && tail.as_ref() == &payload[100..], "Range resume → 206 + tail bytes");
        let r = http
            .get(format!("http://{addr}/app/file?name={fname}"))
            .header("Range", "bytes=99999999-")
            .send().await.unwrap();
        check(r.status().as_u16() == 416, "bad range → 416");
        let r = http
            .get(format!("http://{addr}/app/file?name=../evil.exe"))
            .send().await.unwrap();
        check(r.status().is_client_error(), "/app/file traversal rejected");
        // Concurrency: manifest fetch while a payload download is open.
        let h2 = http.clone();
        let u = format!("http://{addr}/app/file?name={fname}");
        let dl = tokio::spawn(async move { h2.get(u).send().await.unwrap().bytes().await });
        let r2 = http
            .get(format!("http://{addr}/app/latest.json"))
            .send().await.unwrap();
        check(r2.status().is_success(), "server not starved by a parallel download");
        let _ = dl.await;

        // --- hostile seeder: valid manifest, swapped payload ---
        let seed = appupdate::seed_dir(&alice);
        std::fs::write(seed.join(fname), b"CORRUPTED").unwrap();
        let bob = base.join("bob");
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &bob, &hs, |_, _| {}, &pk64).await;
        check(r.is_err(), "swapped payload rejected at sha256 gate");
        let parts_left = appupdate::dl_dir(&bob)
            .read_dir()
            .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".part")))
            .unwrap_or(false);
        check(!parts_left, ".part cleaned after reject");

        // --- failover past a dead seeder ---
        std::fs::write(seed.join(fname), &payload).unwrap(); // restore
        appupdate::add_manual_seeder("127.0.0.1:9"); // dead
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &bob, &hs, |_, _| {}, &pk64).await;
        check(r.is_ok(), &format!("download succeeds past dead seeder: {:?}", r.err()));
        let st = appupdate::app_update_status_pk(&http, &bob, &pk64).await;
        check(st.state == "downloaded" && st.version == "1.0.0", "status → downloaded");

        // --- viral reseed: bob now carries the bundle ---
        check(
            appupdate::seed_bundle_version(&bob).as_deref() == Some("1.0.0"),
            "ALICE→BOB: downloader reseeds the bundle",
        );
        let carol = base.join("carol");
        let r = appupdate::pick_update_bundle_pk(
            &hs, &carol,
            appupdate::seed_dir(&bob).to_str().unwrap(), &pk64,
        );
        check(r.is_ok(), "reseeded bundle validates for the next peer");

        // --- newer offer purges a stale download ---
        let payload2 = vec![b'Y'; 5_000];
        let bundle_b = base.join("bundle-b");
        make_bundle(&bundle_b, "2.0.0", "Modpack-Manager_2.0.0_x64-setup.exe", &payload2, &kp, &[]);
        appupdate::pick_update_bundle_pk(&hs, &alice, bundle_b.to_str().unwrap(), &pk64).unwrap();
        appupdate::add_manual_seeder(&addr); // dead seeder may have purged the live one
        let st = appupdate::app_update_status_pk(&http, &bob, &pk64).await;
        check(st.state == "offer" && st.version == "2.0.0",
            &format!("newer offer displaces stale download (got {})", st.state));
        let bj: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(bob.join("appupdate.json")).unwrap(),
        ).unwrap();
        check(bj["downloaded"].is_null(), "stale download entry purged");

        // --- waiting state: known version, no seeders ---
        appupdate::clear_seeders(); // drop live manual seeders for offline tests
        let dave = base.join("dave");
        std::fs::create_dir_all(&dave).unwrap();
        std::fs::write(dave.join("appupdate.json"), r#"{"knownVersion":"9.9.9"}"#).unwrap();
        let st = appupdate::app_update_status_pk(&http, &dave, &pk64).await;
        check(st.state == "waiting" && st.version == "9.9.9",
            &format!("offline seeders → waiting state (got {})", st.state));

        // --- skip persistence (no seeder: known-version path) ---
        appupdate::skip_version(&dave, "9.9.9");
        let st = appupdate::app_update_status_pk(&http, &dave, &pk64).await;
        check(st.state == "skipped" && st.version == "9.9.9", "skip persists → skipped state");
        appupdate::unskip_version(&dave);
        let st = appupdate::app_update_status_pk(&http, &dave, &pk64).await;
        check(st.state == "waiting", "un-skip returns to waiting");

        // --- skip against a live announce ---
        let dave2 = base.join("dave2");
        appupdate::skip_version(&dave2, "2.0.0");
        appupdate::add_manual_seeder(&addr);
        let st = appupdate::app_update_status_pk(&http, &dave2, &pk64).await;
        check(st.state == "skipped" && st.version == "2.0.0",
            &format!("skipped announce never reaches offer (got {})", st.state));
        appupdate::clear_seeders();

        // --- prepare_install: prev-exe escape hatch ---
        appupdate::add_manual_seeder(&addr); // seeders were cleared for offline tests
        let r = appupdate::download_app_update_pk(&http, &carol, &hs, |_, _| {}, &pk64).await;
        check(r.is_ok(), "carol downloads v2.0.0");
        let p = appupdate::prepare_install(&carol).unwrap();
        check(p.is_file(), "prepare_install returns the verified installer");
        check(
            appupdate::dl_dir(&carol).join("prev").read_dir().map(|d| d.count() > 0).unwrap_or(false),
            "prev exe backed up (escape hatch)",
        );

        // --- launch purge rules ---
        let eve = base.join("eve");
        let eve_dl = appupdate::dl_dir(&eve);
        std::fs::create_dir_all(&eve_dl).unwrap();
        std::fs::write(eve_dl.join("old.part"), b"x").unwrap();
        std::fs::write(eve_dl.join("orphan.tmp"), b"x").unwrap();
        let old = filetime::FileTime::from_unix_time(1_000_000, 0);
        filetime::set_file_mtime(eve_dl.join("old.part"), old).unwrap();
        filetime::set_file_mtime(eve_dl.join("orphan.tmp"), old).unwrap();
        // downloaded <= running purges; seed < running purges; seed >= running kept
        std::fs::write(eve.join("appupdate.json"),
            r#"{"downloaded":{"version":"0.0.1","path":"C:/nope","sha256":"x"},"knownVersion":"0.0.1"}"#).unwrap();
        let eve_seed = appupdate::seed_dir(&eve);
        std::fs::create_dir_all(&eve_seed).unwrap();
        std::fs::write(eve_seed.join("latest.json"),
            r#"{"app":"com.modpackmanager.app","version":"0.0.1","file":"x","sha256":"x","size":1}"#).unwrap();
        appupdate::launch_purge(&eve);
        check(!eve_dl.join("old.part").exists() && !eve_dl.join("orphan.tmp").exists(),
            "stale .part/.tmp swept at launch");
        check(appupdate::seed_bundle_version(&eve).is_none(),
            "superseded seed (< running) purged");
        let ej: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(eve.join("appupdate.json")).unwrap(),
        ).unwrap();
        check(ej["downloaded"].is_null() && ej["knownVersion"].is_null(),
            "installed/superseded download entries purged");

        // seed == running version must be KEPT (viral chain after self-update)
        let frank = base.join("frank");
        let frank_seed = appupdate::seed_dir(&frank);
        std::fs::create_dir_all(&frank_seed).unwrap();
        std::fs::write(frank_seed.join("latest.json"), format!(
            r#"{{"app":"com.modpackmanager.app","version":"{}","file":"x","sha256":"x","size":1}}"#,
            env!("CARGO_PKG_VERSION"))).unwrap();
        appupdate::launch_purge(&frank);
        check(
            frank_seed.join("latest.json").exists(),
            "seed at running version survives purge (still needed by peers)",
        );

        // --- corrupt state falls back to defaults ---
        appupdate::clear_seeders(); // live seeder would turn idle into offer
        let grace = base.join("grace");
        std::fs::create_dir_all(&grace).unwrap();
        std::fs::write(grace.join("appupdate.json"), "not json {{{").unwrap();
        let st = appupdate::app_update_status_pk(&http, &grace, &pk64).await;
        check(st.state == "idle", &format!("corrupt appupdate.json → safe idle (got {})", st.state));

        // --- manifest size cap: >64KB manifest must fail fetch ---
        let huge = base.join("bundle-huge");
        std::fs::create_dir_all(&huge).unwrap();
        std::fs::write(huge.join(fname), &payload).unwrap();
        let big_man = format!("{}{}", &man_a[..man_a.len()-1], &format!(",\"pad\":\"{}\"}}", "z".repeat(70_000)));
        let big_sig = minisign::sign(Some(&kp.pk), &kp.sk,
            std::io::Cursor::new(big_man.as_bytes()), None, None).unwrap().to_string();
        std::fs::write(huge.join("latest.json"), &big_man).unwrap();
        std::fs::write(huge.join("latest.json.minisig"), &big_sig).unwrap();
        appupdate::pick_update_bundle_pk(&hs, &alice, huge.to_str().unwrap(), &pk64).unwrap();
        appupdate::add_manual_seeder(&addr);
        let henry = base.join("henry");
        let r = appupdate::download_app_update_pk(&http, &henry, &hs, |_, _| {}, &pk64).await;
        check(r.is_err(), "oversized manifest rejected by fetch cap");
        // restore the good v2.0.0 seed for the next check
        appupdate::pick_update_bundle_pk(&hs, &alice, bundle_b.to_str().unwrap(), &pk64).unwrap();

        // --- running exe alone never advertises (strict publish rule) ---
        let iris = base.join("iris");
        check(
            appupdate::seed_bundle_version(&iris).is_none(),
            "no seed dir → nothing advertised (build ≠ publish)",
        );
    });

    // ---- cleanup ----
    println!("== cleanup ==");
    drop(cs);
    drop(hs);
    if let Err(e) = std::fs::remove_dir_all(&base) {
        println!("  [WARN] cleanup failed: {e}");
    } else {
        check(!base.exists(), "sandbox dir removed — nothing left behind");
    }

    let fails = unsafe { FAILURES };
    println!("\n{} failure(s)", fails);
    std::process::exit(if fails > 0 { 1 } else { 0 });
}

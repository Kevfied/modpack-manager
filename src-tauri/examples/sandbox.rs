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

fn sha1_bytes(b: &[u8]) -> String {
    use sha1::Digest;
    let mut h = sha1::Sha1::new();
    h.update(b);
    hex::encode(h.finalize())
}

/// A throttled byte source — one fixed delay per read so a 6-file apply
/// is measurably parallel (sequential ~3s vs parallel ~1s).
struct SlowReader {
    data: Arc<Vec<u8>>,
    pos: usize,
    delay: std::time::Duration,
}
impl std::io::Read for SlowReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let n = buf.len().min(2048).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        if self.pos < self.data.len() {
            std::thread::sleep(self.delay);
        }
        Ok(n)
    }
}

/// A second, independent "host" — a real tiny_http server on :47777 that
/// serves the whole Connect protocol with per-read delays and a toggleable
/// Range implementation. Proves parallel downloads, mid-transfer
/// cancel+resume, and the old-host 200 fallback — none of which the
/// instant-loopback SharedHost can exercise.
struct FakeHost {
    addr: String,
    range_hits: Arc<std::sync::atomic::AtomicUsize>,
    leave_hits: Arc<std::sync::atomic::AtomicUsize>,
    honor_range: Arc<std::sync::atomic::AtomicBool>,
}
fn spawn_fake_host(files: &[(String, Vec<u8>)]) -> FakeHost {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let manifest = sync::Manifest {
        network_id: "fake-net".into(),
        pack_name: "Fake Pack".into(),
        version: 1,
        note: String::new(),
        mc_version: "1.21.1".into(),
        loader: "fabric".into(),
        loader_version: "0.15.11".into(),
        files: files
            .iter()
            .map(|(n, b)| sync::ManifestFile {
                path: n.clone(),
                sha1: sha1_bytes(b),
                size: b.len() as u64,
            })
            .collect(),
    };
    let manifest_json = serde_json::to_string(&manifest).unwrap();
    let srv = Arc::new(tiny_http::Server::http("127.0.0.1:47777").expect("fake host bind"));
    let range_hits = Arc::new(AtomicUsize::new(0));
    let leave_hits = Arc::new(AtomicUsize::new(0));
    let honor_range = Arc::new(AtomicBool::new(true));
    {
        let srv = srv.clone();
        let (rh, lh, hr) = (range_hits.clone(), leave_hits.clone(), honor_range.clone());
        let files: Arc<Vec<(String, Arc<Vec<u8>>)>> = Arc::new(
            files
                .iter()
                .map(|(n, b)| (n.clone(), Arc::new(b.clone())))
                .collect(),
        );
        std::thread::spawn(move || {
            while let Ok(req) = srv.recv() {
                let (rh, lh, hr) = (rh.clone(), lh.clone(), hr.clone());
                let mj = manifest_json.clone();
                let files = files.clone();
                // Per-request thread — parallel clients must be served
                // concurrently, just like the real server.
                std::thread::spawn(move || {
                    let url = req.url().to_string();
                    let path = url.split('?').next().unwrap_or("");
                    let method = req.method().as_str().to_string();
                    match (method.as_str(), path) {
                        ("GET", "/info") => {
                            let _ = req.respond(tiny_http::Response::from_string(
                                r#"{"networkId":"fake-net","name":"fake-net","packName":"Fake Pack","machine":"fake","latestVersion":1,"members":0,"requiresCode":false,"appVersion":"0.0.0"}"#,
                            ));
                        }
                        ("POST", "/join") => {
                            let _ = req.respond(tiny_http::Response::from_string(
                                r#"{"ok":true,"memberId":4242}"#,
                            ));
                        }
                        ("POST", "/heartbeat") => {
                            let _ = req.respond(tiny_http::Response::from_string(
                                r#"{"members":[],"latestVersion":1}"#,
                            ));
                        }
                        ("POST", "/leave") => {
                            lh.fetch_add(1, Ordering::Relaxed);
                            let _ = req.respond(tiny_http::Response::from_string(r#"{"ok":true}"#));
                        }
                        ("GET", "/manifest") => {
                            let _ = req.respond(tiny_http::Response::from_string(mj));
                        }
                        ("GET", "/file") => {
                            let name = url.rsplit("path=").next().unwrap_or("");
                            let Some((_, data)) = files.iter().find(|(n, _)| n == name) else {
                                let _ = req.respond(tiny_http::Response::empty(404));
                                return;
                            };
                            let mut start = 0usize;
                            for hdr in req.headers() {
                                if hdr.field.equiv("Range") {
                                    rh.fetch_add(1, Ordering::Relaxed);
                                    if let Some(s) = hdr
                                        .value
                                        .as_str()
                                        .strip_prefix("bytes=")
                                        .and_then(|x| x.split('-').next())
                                        .and_then(|x| x.parse::<usize>().ok())
                                    {
                                        start = s.min(data.len());
                                    }
                                }
                            }
                            let ranged = hr.load(Ordering::Relaxed) && start > 0;
                            let body: Arc<Vec<u8>> = if ranged {
                                Arc::new(data[start..].to_vec())
                            } else {
                                data.clone()
                            };
                            let mut headers = vec![tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"application/octet-stream"[..],
                            )
                            .unwrap()];
                            if ranged {
                                headers.push(
                                    tiny_http::Header::from_bytes(
                                        &b"Content-Range"[..],
                                        format!(
                                            "bytes {}-{}/{}",
                                            start,
                                            data.len() - 1,
                                            data.len()
                                        )
                                        .as_bytes(),
                                    )
                                    .unwrap(),
                                );
                            }
                            let resp = tiny_http::Response::new(
                                tiny_http::StatusCode(if ranged { 206 } else { 200 }),
                                headers,
                                SlowReader {
                                    data: body.clone(),
                                    pos: 0,
                                    delay: std::time::Duration::from_millis(10),
                                },
                                Some(body.len()),
                                None,
                            );
                            let _ = req.respond(resp);
                        }
                        _ => {
                            let _ = req.respond(tiny_http::Response::empty(404));
                        }
                    }
                });
            }
        });
    }
    FakeHost {
        addr: "127.0.0.1:47777".into(),
        range_hits,
        leave_hits,
        honor_range,
    }
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

        // ---- 9e. open network: no join code asked or needed ----
        println!("== open network ==");
        sync::host_update(&hs, &store_path, "sandbox-net-v2", "").unwrap();
        let info: serde_json::Value = http
            .get(format!("http://{addr}/info"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        check(
            info["requiresCode"] == false,
            "open network reports requiresCode=false",
        );
        let cs5: sync::SharedClient = Arc::new(Mutex::new(None));
        let open_join =
            sync::join_network(&cs5, &store_path, &sync_root, &addr, "", "dave").await;
        check(open_join.is_ok(), "open network joins with empty code");
        sync::host_update(&hs, &store_path, "sandbox-net-v2", "newcode").unwrap();
        let info: serde_json::Value = http
            .get(format!("http://{addr}/info"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        check(
            info["requiresCode"] == true,
            "locked network reports requiresCode=true",
        );
        // dave joined during the open window — member-id auth still works
        // after the code is re-enabled.
        check(
            sync::check_pending(&cs5, &http).await.is_ok(),
            "member joined during open window survives code re-enable",
        );

        // ---- 9f. member-id merge + id 0 guard ----
        println!("== member ids ==");
        let r = http
            .post(format!("http://{addr}/join"))
            .json(&serde_json::json!({"code": "newcode", "memberId": 7777, "name": "eve"}))
            .send()
            .await
            .unwrap();
        check(r.status().is_success(), "eve joins as id 7777");
        // same id rejoins with a new name → MERGE, not a second row
        let r = http
            .post(format!("http://{addr}/join"))
            .json(&serde_json::json!({"code": "newcode", "memberId": 7777, "name": "eve-renamed"}))
            .send()
            .await
            .unwrap();
        check(r.status().is_success(), "id 7777 rejoin accepted (merge)");
        let v: serde_json::Value = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", "42")
            .json(&serde_json::json!({"memberId": 42, "name": "bob", "appliedVersion": 0}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let members = v["members"].as_array().unwrap();
        let eve_rows = members.iter().filter(|m| m["id"] == 7777).count();
        check(
            eve_rows == 1 && members.iter().any(|m| m["id"] == 7777 && m["name"] == "eve-renamed"),
            "rejoin merged — one row, new name, no duplicate",
        );
        // memberId 0 is reserved for the host row — server must assign one
        let r = http
            .post(format!("http://{addr}/join"))
            .json(&serde_json::json!({"code": "newcode", "memberId": 0, "name": "zero"}))
            .send()
            .await
            .unwrap();
        let v: serde_json::Value = r.json().await.unwrap();
        check(
            v["memberId"].as_u64().unwrap_or(0) != 0,
            "memberId 0 rejected — server assigned a real id",
        );
        // ghost member: joins, never heartbeats — asserted offline-but-listed
        // later via roster persistence.
        let r = http
            .post(format!("http://{addr}/join"))
            .json(&serde_json::json!({"code": "newcode", "memberId": 5555, "name": "ghost"}))
            .send()
            .await
            .unwrap();
        check(r.status().is_success(), "ghost member registered (id 5555)");

        // ---- 9g. rejoin preserves applied_version ----
        println!("== rejoin merge ==");
        let alice_id = cs.lock().unwrap().as_ref().unwrap().member_id;
        let alice_applied = cs.lock().unwrap().as_ref().unwrap().applied_version;
        let r = http
            .post(format!("http://{addr}/join"))
            .json(&serde_json::json!({"code": "newcode", "memberId": alice_id, "name": "alice"}))
            .send()
            .await
            .unwrap();
        check(r.status().is_success(), "alice rejoins with her saved id");
        let v: serde_json::Value = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", "42")
            .json(&serde_json::json!({"memberId": 42, "name": "bob", "appliedVersion": 0}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let a = v["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == alice_id)
            .unwrap();
        check(
            a["appliedVersion"].as_u64().unwrap_or(999) == alice_applied,
            &format!(
                "rejoin kept applied_version v{alice_applied} (got {})",
                a["appliedVersion"]
            ),
        );
        check(a["upToDate"] == true, "rejoined alice not marked behind");

        // ---- 9h. targeted publish dies with leave/forget; forget ≠ ban ----
        println!("== target cleanup ==");
        let r = http
            .post(format!("http://{addr}/join"))
            .json(&serde_json::json!({"code": "newcode", "memberId": 8888, "name": "eve2"}))
            .send()
            .await
            .unwrap();
        check(r.status().is_success(), "eve2 joins as 8888");
        let tgt = sync::host_publish(&hs, &store_path, "secret", Some(8888), false).unwrap();
        check(tgt.contains("only"), "targeted publish to 8888");
        let tm: serde_json::Value = http
            .get(format!("http://{addr}/manifest"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", "8888")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let targeted_v = tm["version"].as_u64().unwrap();
        let global_v = {
            let g = hs.lock().unwrap();
            g.as_ref().unwrap().manifest.version
        };
        check(targeted_v != global_v, "eve2 sees her targeted manifest");
        // leave → row AND target gone → she sees the global manifest again
        let _ = http
            .post(format!("http://{addr}/leave"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", "8888")
            .send()
            .await
            .unwrap();
        let tm: serde_json::Value = http
            .get(format!("http://{addr}/manifest"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", "8888")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        check(
            tm["version"].as_u64().unwrap() == global_v,
            "leave clears the targeted manifest (global served)",
        );
        let v: serde_json::Value = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", "42")
            .json(&serde_json::json!({"memberId": 42, "name": "bob", "appliedVersion": 0}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        check(
            v["members"].as_array().unwrap().iter().all(|m| m["id"] != 8888),
            "leave removed eve2's roster row",
        );
        // forget: same cleanup, no ban — the id can rejoin immediately
        let r = http
            .post(format!("http://{addr}/join"))
            .json(&serde_json::json!({"code": "newcode", "memberId": 8888, "name": "eve2"}))
            .send()
            .await
            .unwrap();
        check(r.status().is_success(), "left member rejoins freely");
        sync::host_forget(&hs, &store_path, 8888).unwrap();
        let r = http
            .post(format!("http://{addr}/join"))
            .json(&serde_json::json!({"code": "newcode", "memberId": 8888, "name": "eve2"}))
            .send()
            .await
            .unwrap();
        check(
            r.status().is_success(),
            "forgot member is NOT banned — instant rejoin ok",
        );
        // clean up eve2's row for the roster count later
        let _ = http
            .post(format!("http://{addr}/leave"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", "8888")
            .send()
            .await
            .unwrap();
        // 7777's stale row out too (eve-renamed was a merge test)
        let _ = http
            .post(format!("http://{addr}/leave"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", "7777")
            .send()
            .await
            .unwrap();

        // ---- 9i. switching networks tells the old host (fake 2nd host) ----
        println!("== network switch ==");
        let fake_files: Vec<(String, Vec<u8>)> = (0..6)
            .map(|i| {
                let mut b = vec![0u8; 96 * 1024];
                for (j, x) in b.iter_mut().enumerate() {
                    *x = ((i * 31 + j * 7) % 251) as u8;
                }
                (format!("fake{i}.jar"), b)
            })
            .collect();
        let fake = spawn_fake_host(&fake_files);
        std::thread::sleep(std::time::Duration::from_millis(150));
        let fake_members_before = fake.leave_hits.load(std::sync::atomic::Ordering::Relaxed);
        let r = sync::join_network(&cs, &store_path, &sync_root, &fake.addr, "", "alice").await;
        check(r.is_ok(), &format!("alice joins fake network: {:?}", r.err()));
        let v: serde_json::Value = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", "42")
            .json(&serde_json::json!({"memberId": 42, "name": "bob", "appliedVersion": 0}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        check(
            v["members"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["id"] != alice_id),
            "old host dropped alice's row on network switch",
        );
        // …and back — the fake host must get its own /leave too
        let r = sync::join_network(&cs, &store_path, &sync_root, &addr, "newcode", "alice").await;
        check(r.is_ok(), &format!("alice rejoins real network: {:?}", r.err()));
        check(
            fake.leave_hits.load(std::sync::atomic::Ordering::Relaxed) > fake_members_before,
            "fake host received /leave when alice switched back",
        );
        let v: serde_json::Value = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", "42")
            .json(&serde_json::json!({"memberId": 42, "name": "bob", "appliedVersion": 0}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let alice_rows = v["members"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["name"] == "alice")
            .count();
        check(alice_rows == 1, "rejoin left exactly ONE alice row");

        // ---- 9j. kicked member learns it on the NEXT status check ----
        println!("== kicked probe ==");
        let charlie_id = cs4.lock().unwrap().as_ref().unwrap().member_id;
        sync::host_kick(&hs, &store_path, charlie_id).unwrap();
        let e = sync::check_pending(&cs4, &http).await;
        check(e.is_err(), "kicked member's manifest check errors");
        check(
            cs4.lock().unwrap().as_ref().unwrap().kicked,
            "first 403 probed the heartbeat — kicked flag set immediately",
        );
        // rejoin clears the kick server-side — raw /join, because
        // join_network would read the shared store and inherit whichever
        // member id was last saved there (a sandbox-only artifact).
        let r = http
            .post(format!("http://{addr}/join"))
            .json(&serde_json::json!({"code": "newcode", "memberId": charlie_id, "name": "charlie"}))
            .send()
            .await
            .unwrap();
        check(r.status().is_success(), "kicked member rejoins with the code");
        let v: serde_json::Value = http
            .post(format!("http://{addr}/heartbeat"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", charlie_id.to_string())
            .json(&serde_json::json!({"memberId": charlie_id, "name": "charlie", "appliedVersion": 0}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        check(v["kicked"].is_null(), "rejoined charlie heartbeats normally");

        // ---- 9k. parallel apply against the throttled fake host ----
        // Each fake-network client gets its OWN sync root — sharing one
        // would make every later apply find the files already in place.
        println!("== parallel apply ==");
        let sr6 = sync_root.join("c6");
        let cs6: sync::SharedClient = Arc::new(Mutex::new(None));
        sync::join_network(&cs6, &store_path, &sr6, &fake.addr, "", "p-join")
            .await
            .unwrap();
        let t0 = std::time::Instant::now();
        sync::apply_update(&cs6, &http, &store_path).await.unwrap();
        let elapsed = t0.elapsed();
        // 6 × 96KB at 10ms/2KB ≈ 470ms/file → sequential ~2.8s, 4-way ~1s.
        check(
            elapsed.as_millis() < 2200,
            &format!("parallel apply of 6 throttled files took {}ms (<2.2s)", elapsed.as_millis()),
        );
        let mut all_ok = true;
        for (n, b) in &fake_files {
            let p = sr6.join("fake-net").join("mods").join(n);
            if !p.is_file() || std::fs::read(&p).ok().as_deref() != Some(b.as_slice()) {
                all_ok = false;
            }
        }
        check(all_ok, "all 6 parallel files byte-identical");

        // ---- 9l. cancel mid-apply → .part survives → resume completes ----
        println!("== cancel + resume ==");
        let sr7 = sync_root.join("c7");
        let cs7: sync::SharedClient = Arc::new(Mutex::new(None));
        sync::join_network(&cs7, &store_path, &sr7, &fake.addr, "", "c-join")
            .await
            .unwrap();
        let cs7b = cs7.clone();
        let httpb = http.clone();
        let storeb = store_path.clone();
        let h7 = tokio::spawn(async move { sync::apply_update(&cs7b, &httpb, &storeb).await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        sync::cancel_apply();
        let r = h7.await.unwrap();
        let msg = r.unwrap_or_else(|e| e.to_string());
        check(
            msg.contains("cancelled"),
            &format!("mid-apply cancel → '{msg}'"),
        );
        let mods7 = sr7.join("fake-net").join("mods");
        let parts: Vec<_> = std::fs::read_dir(&mods7)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "part").unwrap_or(false))
            .collect();
        check(
            !parts.is_empty(),
            "cancel left .part file(s) for resume",
        );
        let range_before = fake.range_hits.load(std::sync::atomic::Ordering::Relaxed);
        let r = sync::apply_update(&cs7, &http, &store_path).await;
        check(r.is_ok(), &format!("re-apply after cancel: {:?}", r.err()));
        check(
            fake.range_hits.load(std::sync::atomic::Ordering::Relaxed) > range_before,
            "resume sent Range header(s) for the surviving .part(s)",
        );
        let mut all_ok = true;
        for (n, b) in &fake_files {
            let p = mods7.join(n);
            if !p.is_file() || std::fs::read(&p).ok().as_deref() != Some(b.as_slice()) {
                all_ok = false;
            }
        }
        check(all_ok, "resumed files byte-identical after cancel");

        // ---- 9m. explicit resume offset + old-host 200 fallback ----
        println!("== range fallback ==");
        let sr8 = sync_root.join("c8");
        let cs8: sync::SharedClient = Arc::new(Mutex::new(None));
        sync::join_network(&cs8, &store_path, &sr8, &fake.addr, "", "r-join")
            .await
            .unwrap();
        let mods8 = sr8.join("fake-net").join("mods");
        std::fs::create_dir_all(&mods8).unwrap();
        // a .part with the correct first 4KB — a Range request should resume it
        std::fs::write(mods8.join("fake0.part"), &fake_files[0].1[..4096]).unwrap();
        let range_before = fake.range_hits.load(std::sync::atomic::Ordering::Relaxed);
        sync::apply_update(&cs8, &http, &store_path).await.unwrap();
        check(
            fake.range_hits.load(std::sync::atomic::Ordering::Relaxed) > range_before,
            "valid .part resumed via Range (206)",
        );
        let mut all_ok = true;
        for (n, b) in &fake_files {
            let p = mods8.join(n);
            if !p.is_file() || std::fs::read(&p).ok().as_deref() != Some(b.as_slice()) {
                all_ok = false;
            }
        }
        check(all_ok, "resumed prefix produced correct bytes");
        // old host ignores Range → 200 + full body → engine restarts the file
        fake.honor_range
            .store(false, std::sync::atomic::Ordering::Relaxed);
        for (n, _) in &fake_files {
            let _ = std::fs::remove_file(mods8.join(n));
        }
        std::fs::write(mods8.join("fake0.part"), b"GARBAGEPREFIX").unwrap();
        sync::apply_update(&cs8, &http, &store_path).await.unwrap();
        check(
            std::fs::read(mods8.join("fake0.jar")).ok().as_deref()
                == Some(fake_files[0].1.as_slice()),
            "200-on-Range restart: garbage .part did NOT corrupt the file",
        );
        fake.honor_range
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // ---- 9n. Range on the REAL host + host-online threshold + memo ----
        println!("== host range / flapping / memo ==");
        let alpha = std::fs::read(host_mods.join("alpha.jar")).unwrap();
        let r = http
            .get(format!("http://{addr}/file?path=alpha.jar"))
            .header("X-Join-Code", "newcode")
            .header("X-Member-Id", alice_id)
            .header("Range", "bytes=10-")
            .send()
            .await
            .unwrap();
        check(r.status().as_u16() == 206, "real host honours Range → 206");
        let body = r.bytes().await.unwrap();
        check(
            body.as_ref() == &alpha[10..],
            "206 body is the requested tail",
        );
        // dead host: two failed checks keep host_online, the third drops it
        let dead: sync::SharedClient = Arc::new(Mutex::new(Some(sync::ClientState {
            network_id: "dead-net".into(),
            network_name: "dead".into(),
            host_addr: "127.0.0.1:1".into(),
            code: String::new(),
            member_id: 9,
            display_name: "x".into(),
            applied_version: 0,
            declined: vec![],
            synced_dir: base.join("dead"),
            host_online: true,
            consec_failures: 0,
            applying: false,
            kicked: false,
            member_views: vec![],
            latest_version: 0,
            latest_note: String::new(),
            hash_memo: std::collections::HashMap::new(),
            hb_token: 1,
        })));
        let _ = sync::check_pending(&dead, &http).await;
        let _ = sync::check_pending(&dead, &http).await;
        check(
            dead.lock().unwrap().as_ref().unwrap().host_online,
            "2 failed checks — host still 'online' (no flap)",
        );
        let _ = sync::check_pending(&dead, &http).await;
        check(
            !dead.lock().unwrap().as_ref().unwrap().host_online,
            "3rd consecutive miss marks host offline",
        );
        // hash memo: same size+mtime → skip re-hash; changed size → detect.
        // The host's live folder may have been swapped in §6b — publish
        // from whatever h.folder currently points at.
        let cur_folder = hs.lock().unwrap().as_ref().unwrap().folder.clone();
        make_jar(&cur_folder.join("epsilon.jar"), "epsilon", "1.0");
        sync::host_publish(&hs, &store_path, "memo test", None, false).unwrap();
        let (_, dl0, _) = sync::check_pending(&cs, &http).await.unwrap().unwrap();
        check(dl0 == 1, &format!("new file pending ({dl0} to fetch)"));
        let alpha_local = sync_root
            .join(cs.lock().unwrap().as_ref().unwrap().network_id.clone())
            .join("mods/alpha.jar");
        let orig_mtime = std::fs::metadata(&alpha_local)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // tamper alpha with same length, then restore its mtime — a
        // re-hash would catch it, a memo hit must not
        let mut tampered = std::fs::read(&alpha_local).unwrap();
        tampered[0] ^= 0xFF;
        std::fs::write(&alpha_local, &tampered).unwrap();
        let ps = format!(
            "(Get-Item '{}').LastWriteTimeUtc = [DateTimeOffset]::FromUnixTimeSeconds({}).UtcDateTime",
            alpha_local.display(),
            orig_mtime
        );
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .output();
        let (_, dl1, _) = sync::check_pending(&cs, &http).await.unwrap().unwrap();
        check(
            dl1 == 1,
            &format!("size+mtime unchanged → memo skipped re-hash ({dl1} to fetch)"),
        );
        // now break the size — memo miss → real hash → mismatch detected
        tampered.push(0xEE);
        std::fs::write(&alpha_local, &tampered).unwrap();
        let (_, dl2, _) = sync::check_pending(&cs, &http).await.unwrap().unwrap();
        check(
            dl2 >= 2,
            &format!("size change → memo miss → tamper detected ({dl2} to fetch)"),
        );
        // apply repairs everything the memo let slide
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        check(
            sync::check_pending(&cs, &http).await.unwrap().is_none(),
            "apply repaired memo-masked tamper — nothing pending",
        );
        // epsilon was only a memo-test fixture — remove it so later
        // sections see the original 6-jar folder again.
        std::fs::remove_file(&cur_folder.join("epsilon.jar")).unwrap();
        sync::host_publish(&hs, &store_path, "drop epsilon", None, false).unwrap();
        sync::apply_update(&cs, &http, &store_path).await.unwrap();
        check(
            !sync_root
                .join(cs.lock().unwrap().as_ref().unwrap().network_id.clone())
                .join("mods/epsilon.jar")
                .exists(),
            "member mirror-deleted epsilon after its removal",
        );

        // 9n-ii · non-zip .jar: scanner keeps content hashes on zip-parse
        // failure — before the fix it published sha1 "" so a member could
        // never verify the download (permanent "sha1 mismatch").
        {
            let broken = cur_folder.join("broken-mod.jar");
            let mut rng = 0x9E3779B97F4A7C15u64;
            let mut content = vec![0u8; 50_000];
            for b in &mut content {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                *b = rng as u8;
            }
            std::fs::write(&broken, &content).unwrap();
            sync::host_publish(&hs, &store_path, "broken jar", None, false).unwrap();
            let has_real_sha1 = hs
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .manifest
                .files
                .iter()
                .find(|f| f.path == "broken-mod.jar")
                .map(|f| f.sha1.len() == 40)
                .unwrap_or(false);
            check(has_real_sha1, "non-zip jar enters manifest with a real sha1");
            sync::apply_update(&cs, &http, &store_path).await.unwrap();
            let local_broken = sync_root
                .join(cs.lock().unwrap().as_ref().unwrap().network_id.clone())
                .join("mods/broken-mod.jar");
            check(
                std::fs::read(&local_broken)
                    .map(|b| b == content)
                    .unwrap_or(false),
                "member downloads non-zip jar byte-identical",
            );
            std::fs::remove_file(&broken).unwrap();
            sync::host_publish(&hs, &store_path, "drop broken", None, false).unwrap();
            sync::apply_update(&cs, &http, &store_path).await.unwrap();
        }

        // ---- 9o. discovery TTL ----
        println!("== discovery ttl ==");
        sync::found_insert_for_test(sync::FoundNetwork {
            network_id: "stale-net".into(),
            name: "stale".into(),
            pack_name: "x".into(),
            machine: "x".into(),
            addr: "10.9.9.9:47676".into(),
            latest_version: 1,
            members: 1,
            app_version: "0".into(),
            requires_code: Some(true),
            last_seen: 1, // ancient — must be pruned on read
        });
        check(
            sync::found_networks()
                .iter()
                .all(|f| f.network_id != "stale-net"),
            "stale found entry pruned by TTL",
        );
        // our own announce should have landed in the found list by now
        let mut found_ours = false;
        for _ in 0..24 {
            if let Some(f) = sync::found_networks()
                .iter()
                .find(|f| f.name == "sandbox-net-v2")
            {
                check(
                    f.requires_code == Some(true),
                    "announce carried requiresCode=true",
                );
                found_ours = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        check(found_ours, "our announce is discoverable on the LAN");
        // alice is now on a fresh id from the network-switch rejoin —
        // re-save her session so resume restores her, not a stale slot.
        sync::apply_update(&cs, &http, &store_path).await.unwrap();

        // ---- 10. persistence / resume ----
        println!("== persistence ==");
        // The roster must be IN connect.json — a host restart restoring
        // an empty roster is exactly how offline members vanished.
        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
        let saved_members = store["host"]["members"].as_array().unwrap();
        check(
            saved_members.len() >= 4,
            &format!(
                "roster persisted to connect.json ({} members)",
                saved_members.len()
            ),
        );
        check(
            saved_members.iter().any(|m| m["id"] == 5555),
            "offline ghost row is part of the persisted roster",
        );
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
        {
            // THE offline-member fix: a resumed host restores the whole
            // roster — offline members reappear instead of vanishing.
            let g = hs2.lock().unwrap();
            let h = g.as_ref().expect("host resume should restore HostState");
            check(
                h.members.len() >= 4,
                &format!("resumed host restored {} member rows", h.members.len()),
            );
            check(
                h.members.contains_key(&5555),
                "ghost member back in the roster after host restart",
            );
        }
        // The resumed host spawned its own announcer (correct production
        // behavior — a restarted host SHOULD announce). In-process it
        // would keep our found entry alive forever — stop it.
        sync::host_stop(&hs2, &store_path);

        // ---- 11. synced pack → loaders ----
        println!("== synced pack -> loader ==");
        let packs = sync::list_synced_packs(&inst_root);
        check(!packs.is_empty(), "synced packs listed");
        // Several networks may have synced dirs now (fake-net too) —
        // keep only the real sandbox pack for the loader assertions.
        let packs: Vec<_> = packs
            .into_iter()
            .filter(|p| p.name == "Sandbox Pack")
            .collect();
        check(packs.len() == 1, "sandbox pack synced dir present");
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
        // Stale jars in the managed instance get mirror-deleted on reload.
        let inst = prism_dir
            .join("instances")
            .join(&packs[0].name.to_lowercase().replace(' ', "-"));
        let stale = inst.join(".minecraft/mods/stale-old.jar");
        std::fs::write(&stale, b"stale").unwrap();
        let notajar = inst.join(".minecraft/mods/note.txt");
        std::fs::write(&notajar, b"keep me").unwrap();
        let l2 = sync::load_synced_pack(&mr_client, &inst_root, &packs[0].network_id, &prism)
            .await
            .unwrap();
        println!("{l2}");
        check(!stale.exists(), "stale jar mirror-deleted on reinstall");
        check(notajar.exists(), "non-jar file in mods dir untouched");
        check(
            inst.join(".minecraft/mods/alpha.jar").is_file(),
            "pack jar still present after mirror",
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
        // host_online drops only after OFFLINE_MISSES consecutive misses
        for _ in 0..3 {
            let _ = sync::check_pending(&cs, &http).await;
        }
        check(
            !cs.lock().unwrap().as_ref().unwrap().host_online,
            "host marked offline after 3 misses, network remembered",
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

    // ---- 12b. leaving clears the synced mirror from disk ----
    println!("== leave cleans synced dir ==");
    let leftover = { cs.lock().unwrap().as_ref().unwrap().synced_dir.clone() };
    check(leftover.join("pack.json").is_file(), "synced dir exists pre-leave");
    rt.block_on(sync::leave_network(&cs, &http, &store_path));
    check(
        !leftover.exists(),
        "leave_network removed the synced mirror dir",
    );
    check(cs.lock().unwrap().is_none(), "client session cleared on leave");

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
        // UDP demux: ONE socket on 47677 must route MMPKAPP datagrams to the
        // seeder map even while the discovery listener owns the port —
        // two separate listeners starve each other on Windows.
        {
            let ann = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
            ann.set_broadcast(true).unwrap();
            let msg = serde_json::json!({
                "magic": "MMPKAPP", "bundleVersion": "99.0.0",
                "runningVersion": "0.0.1", "port": 47999u32,
                "machine": "sbxfake"
            }).to_string();
            for _ in 0..3 {
                ann.send_to(msg.as_bytes(), "255.255.255.255:47677").unwrap();
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
            let mut heard = false;
            for _ in 0..30 {
                if appupdate::found_seeders().iter().any(|s| s.machine == "sbxfake"
                    && s.bundle_version == "99.0.0" && s.addr.ends_with(":47999")) {
                    heard = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            check(heard, "MMPKAPP demuxed into seeder map over real UDP");
        }
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

    // ---- 15. real TTL: our entry must die after hosting stopped ----
    println!("== discovery ttl (real) ==");
    {
        let mut gone = false;
        // The Connect host stopped in §12 — its found entry ages out at
        // FOUND_TTL. Poll until it disappears (≤30s of slack).
        for _ in 0..60 {
            if sync::found_networks()
                .iter()
                .all(|f| f.name != "sandbox-net-v2")
            {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        check(gone, "stopped host's entry expired from the found list");
    }

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

//! Updater stress suite — every hostile/weird input the LAN update channel
//! can plausibly see, run in one process over 127.0.0.1.
//! Everything lives in %TEMP%/mm-updstress-<pid> which is deleted at the end.
//!
//! Usage: cargo run --example updstress

use app_lib::{appupdate, sync};
use std::path::Path;
use std::sync::{Arc, Mutex};

static mut FAILURES: u32 = 0;

fn check(cond: bool, msg: &str) {
    println!("  [{}] {}", if cond { "OK" } else { "FAIL" }, msg);
    if !cond {
        unsafe { FAILURES += 1 };
    }
}

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
        "notes": "stress bundle",
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

fn seed_payload(seed: &Path, m: &appupdate::UpdateManifest, bytes: &[u8]) {
    std::fs::write(appupdate::seed_dir(seed).join(&m.file), bytes).unwrap();
}

fn main() {
    let base = std::env::temp_dir().join(format!("mm-updstress-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let hs: sync::SharedHost = Arc::new(Mutex::new(None));
    let http = reqwest::Client::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let addr = format!("127.0.0.1:{}", sync::HTTP_PORT);
    let cur = env!("CARGO_PKG_VERSION");

    // The process-wide app dir the HTTP server seeds from ("alice").
    let alice = base.join("alice");
    appupdate::init(&alice);

    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let pk64 = kp.pk.to_base64();
    let kp_evil = minisign::KeyPair::generate_unencrypted_keypair().unwrap();

    rt.block_on(async {
        // ================================================================
        println!("== signatures & trust ==");
        let payload = vec![b'X'; 40_000];
        let fname = "Modpack-Manager_1.0.0_x64-setup.exe";
        let bundle_a = base.join("bundle-a");
        let man_a = make_bundle(&bundle_a, "1.0.0", fname, &payload, &kp, &[]);
        let sig_a = std::fs::read_to_string(bundle_a.join("latest.json.minisig")).unwrap();
        let m_a: appupdate::UpdateManifest = serde_json::from_str(&man_a).unwrap();

        check(
            appupdate::verify_manifest_sig(man_a.as_bytes(), &sig_a, &pk64).is_ok(),
            "signed manifest verifies",
        );
        check(
            appupdate::verify_manifest_sig(
                man_a.replace("\"1.0.0\"", "\"9.9.9\"").as_bytes(),
                &sig_a,
                &pk64,
            )
            .is_err(),
            "forged version rejected (sig binds every byte)",
        );
        // Signature lifted from a different manifest must not fit this one.
        let bundle_sig2 = base.join("bundle-sig2");
        let man_b = make_bundle(&bundle_sig2, "2.0.0", "other.exe", b"Y", &kp, &[]);
        let _ = man_b;
        let sig_b = std::fs::read_to_string(bundle_sig2.join("latest.json.minisig")).unwrap();
        check(
            appupdate::verify_manifest_sig(man_a.as_bytes(), &sig_b, &pk64).is_err(),
            "lifted signature from other manifest rejected",
        );
        check(
            appupdate::verify_manifest_sig(man_a.as_bytes(), &sig_a, &kp_evil.pk.to_base64())
                .is_err(),
            "wrong pubkey rejected",
        );
        check(
            appupdate::verify_manifest_sig(man_a.as_bytes(), "", &pk64).is_err(),
            "empty signature rejected",
        );
        check(
            appupdate::verify_manifest_sig(man_a.as_bytes(), "garbage!!!", &pk64).is_err(),
            "garbage signature rejected",
        );
        // A perfectly signed manifest from the ATTACKER's key must fail too.
        let bundle_evil = base.join("bundle-evil");
        let man_evil = make_bundle(&bundle_evil, "9.9.9", fname, &payload, &kp_evil, &[]);
        let sig_evil =
            std::fs::read_to_string(bundle_evil.join("latest.json.minisig")).unwrap();
        check(
            appupdate::verify_manifest_sig(man_evil.as_bytes(), &sig_evil, &pk64).is_err(),
            "attacker-signed manifest rejected",
        );

        // ================================================================
        println!("== manifest gate edge cases ==");
        check(appupdate::gate_manifest(&m_a, cur, None).is_ok(), "newer passes gate");
        check(appupdate::gate_manifest(&m_a, "1.0.0", None).is_err(), "equal refused");
        check(appupdate::gate_manifest(&m_a, "9.9.9", None).is_err(), "downgrade refused");
        check(
            appupdate::gate_manifest(&m_a, cur, Some("1.0.0")).is_err(),
            "skipped refused",
        );
        let mut m = m_a.clone();
        m.channel = "beta".into();
        check(appupdate::gate_manifest(&m, cur, None).is_err(), "wrong channel refused");
        let mut m = m_a.clone();
        m.arch = "aarch64".into();
        check(appupdate::gate_manifest(&m, cur, None).is_err(), "wrong arch refused");
        let mut m = m_a.clone();
        m.app = "com.other.app".into();
        check(appupdate::gate_manifest(&m, cur, None).is_err(), "foreign app refused");
        for bad in ["../evil.exe", "..\\evil.exe", "a/b.exe", "a\\b.exe", "x..y.exe", "C:\\x.exe", ""] {
            let mut m = m_a.clone();
            m.file = bad.into();
            check(
                appupdate::gate_manifest(&m, cur, None).is_err(),
                &format!("unsafe filename refused: {bad:?}"),
            );
        }
        let mut m = m_a.clone();
        m.channel = String::new();
        m.arch = String::new();
        check(
            appupdate::gate_manifest(&m, cur, None).is_ok(),
            "empty channel/arch tolerated (defaults)",
        );
        let mut m = m_a.clone();
        m.version = "1.0.0-beta.1".into();
        let _ = appupdate::gate_manifest(&m, cur, None); // must not panic
        check(true, "prerelease version parses without panic");
        let mut m = m_a.clone();
        m.version = "banana".into();
        check(
            appupdate::gate_manifest(&m, cur, None).is_err(),
            "non-semver version refused",
        );

        // ================================================================
        println!("== pick_update_bundle edges ==");
        let bob = base.join("bob");
        check(
            appupdate::pick_update_bundle_pk(&hs, &alice, "C:\\does\\not\\exist", &pk64).is_err(),
            "nonexistent dir refused",
        );
        let a_file = base.join("a-file");
        std::fs::write(&a_file, b"x").unwrap();
        check(
            appupdate::pick_update_bundle_pk(&hs, &alice, a_file.to_str().unwrap(), &pk64)
                .is_err(),
            "file-as-dir refused",
        );
        let empty_b = base.join("bundle-empty");
        std::fs::create_dir_all(&empty_b).unwrap();
        check(
            appupdate::pick_update_bundle_pk(&hs, &alice, empty_b.to_str().unwrap(), &pk64)
                .is_err(),
            "missing manifest refused",
        );
        let no_sig = base.join("bundle-nosig");
        std::fs::create_dir_all(&no_sig).unwrap();
        std::fs::write(no_sig.join("latest.json"), &man_a).unwrap();
        std::fs::write(no_sig.join(fname), &payload).unwrap();
        check(
            appupdate::pick_update_bundle_pk(&hs, &alice, no_sig.to_str().unwrap(), &pk64)
                .is_err(),
            "missing .minisig refused",
        );
        let no_pay = base.join("bundle-nopay");
        std::fs::create_dir_all(&no_pay).unwrap();
        std::fs::write(no_pay.join("latest.json"), &man_a).unwrap();
        std::fs::write(no_pay.join("latest.json.minisig"), &sig_a).unwrap();
        check(
            appupdate::pick_update_bundle_pk(&hs, &alice, no_pay.to_str().unwrap(), &pk64)
                .is_err(),
            "missing payload refused",
        );
        check(
            !appupdate::seed_dir(&alice).exists(),
            "all refused picks left zero residue",
        );

        // ================================================================
        println!("== seed + HTTP protocol abuse ==");
        appupdate::pick_update_bundle_pk(&hs, &alice, bundle_a.to_str().unwrap(), &pk64)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        for (name, want) in [
            ("", 404u16),
            ("nonexistent.exe", 404),
            ("latest.json", 404), // only m.file is servable
            ("../x", 404),
            ("%2e%2e%2f%2e%2e%2fevil.exe", 404),
            ("%252e%252e%252f", 404),
            ("latest.json%00.exe", 404),
            ("%00", 404),
        ] {
            let r = http
                .get(format!("http://{addr}/app/file?name={name}"))
                .send()
                .await
                .unwrap();
            check(
                r.status().as_u16() == want,
                &format!("name={name:?} → {want} (got {})", r.status().as_u16()),
            );
        }
        let r = http
            .get(format!("http://{addr}/app/file"))
            .send()
            .await
            .unwrap();
        check(r.status().as_u16() == 404, "no name param → 404");
        let r = http
            .get(format!("http://{addr}/app/file?name={fname}&name=evil"))
            .send()
            .await
            .unwrap();
        check(r.status().as_u16() == 200, "duplicate name param harmless");
        // Range edges
        for (range, want) in [
            ("bytes=0-", 200u16),
            ("bytes=100-", 206),
            ("bytes=39999-", 206),
            ("bytes=40000-", 416),
            ("bytes=99999999-", 416),
            ("bananas", 200),
            ("bytes=abc-", 200),
        ] {
            let r = http
                .get(format!("http://{addr}/app/file?name={fname}"))
                .header("Range", range)
                .send()
                .await
                .unwrap();
            let st = r.status().as_u16();
            let body = r.bytes().await.unwrap();
            let ok = st == want
                && match want {
                    200 => body.len() == payload.len(),
                    206 => {
                        let from: usize = range
                            .trim_start_matches("bytes=")
                            .trim_end_matches('-')
                            .parse()
                            .unwrap_or(0);
                        body.as_ref() == &payload[from..]
                    }
                    _ => true,
                };
            check(ok, &format!("Range {range:?} → {want}"));
        }
        // POST must not crash the handler (methods are ignored by design).
        let r = http
            .post(format!("http://{addr}/app/file?name={fname}"))
            .send()
            .await
            .unwrap();
        check(r.status().as_u16() == 200, "POST /app/file handled without crash");
        // Unknown /app route.
        let r = http
            .get(format!("http://{addr}/app/evil"))
            .send()
            .await
            .unwrap();
        check(r.status().as_u16() == 404, "unknown /app route → 404");
        // Rapid sequential hammering.
        let mut all_ok = true;
        for _ in 0..30 {
            let r = http
                .get(format!("http://{addr}/app/latest.json"))
                .send()
                .await
                .unwrap();
            all_ok &= r.status().is_success();
        }
        check(all_ok, "30 rapid manifest fetches all 200");

        // ================================================================
        println!("== download integrity ==");
        // Swapped payload under a VALID manifest.
        seed_payload(&alice, &m_a, b"CORRUPTED");
        appupdate::clear_seeders();
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &bob, &hs, |_, _| {}, &pk64).await;
        check(r.is_err(), "swapped payload rejected at sha256 gate");
        seed_payload(&alice, &m_a, &payload);

        // Truncated seeder payload → incomplete download.
        seed_payload(&alice, &m_a, &payload[..20_000]);
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &bob, &hs, |_, _| {}, &pk64).await;
        check(r.is_err(), "truncated seeder payload rejected");
        // Bigger-than-manifest payload → size mismatch.
        let mut bigger = payload.clone();
        bigger.extend_from_slice(&[0u8; 4096]);
        seed_payload(&alice, &m_a, &bigger);
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &bob, &hs, |_, _| {}, &pk64).await;
        check(r.is_err(), "oversized seeder payload rejected (size lie)");
        seed_payload(&alice, &m_a, &payload);

        // .part injection: correct prefix → resumes to a hash-correct file.
        let part = appupdate::dl_dir(&bob).join(format!("{fname}.part"));
        std::fs::create_dir_all(appupdate::dl_dir(&bob)).unwrap();
        std::fs::write(&part, &payload[..25_000]).unwrap();
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &bob, &hs, |_, _| {}, &pk64).await;
        check(r.is_ok(), "injected .part resumes to valid download");
        // .part with corrupted prefix → total hash must still catch it.
        let bad_dl = base.join("bad-dl");
        std::fs::create_dir_all(appupdate::dl_dir(&bad_dl)).unwrap();
        let mut bad_part = payload.clone();
        bad_part[0] ^= 0xFF;
        std::fs::write(
            appupdate::dl_dir(&bad_dl).join(format!("{fname}.part")),
            &bad_part[..25_000],
        )
        .unwrap();
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &bad_dl, &hs, |_, _| {}, &pk64).await;
        check(r.is_err(), "corrupted .part prefix caught by total hash");
        // .part larger than the file → reset, clean download.
        let big_dl = base.join("big-dl");
        std::fs::create_dir_all(appupdate::dl_dir(&big_dl)).unwrap();
        std::fs::write(
            appupdate::dl_dir(&big_dl).join(format!("{fname}.part")),
            &bigger,
        )
        .unwrap();
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &big_dl, &hs, |_, _| {}, &pk64).await;
        check(r.is_ok(), "oversized .part reset → clean download");

        // Empty payload edge.
        let bundle_zero = base.join("bundle-zero");
        make_bundle(&bundle_zero, "1.1.0", "empty.exe", b"", &kp, &[]);
        let zdl = base.join("zdl");
        appupdate::pick_update_bundle_pk(&hs, &alice, bundle_zero.to_str().unwrap(), &pk64)
            .unwrap();
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &zdl, &hs, |_, _| {}, &pk64).await;
        check(r.is_ok(), "empty payload handled without panic");
        // restore normal seed
        appupdate::pick_update_bundle_pk(&hs, &alice, bundle_a.to_str().unwrap(), &pk64)
            .unwrap();

        // Served-manifest wins over announced version (double-gate): the
        // manual seeder announces 999.0.0 but serves 1.0.0 — the offer must
        // carry the REAL served version.
        let mike = base.join("mike");
        appupdate::clear_seeders();
        appupdate::add_manual_seeder(&addr);
        let st = appupdate::app_update_status_pk(&http, &mike, &pk64).await;
        check(
            st.state == "offer" && st.version == "1.0.0",
            &format!("served manifest wins over announce (got {} {})", st.state, st.version),
        );

        // ================================================================
        println!("== concurrency stress ==");
        // 8 parallel payload fetches + manifest fetch.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let h2 = http.clone();
            let u = format!("http://{addr}/app/file?name={fname}");
            handles.push(tokio::spawn(async move {
                h2.get(u).send().await.unwrap().bytes().await.unwrap()
            }));
        }
        let r = http
            .get(format!("http://{addr}/app/latest.json"))
            .send()
            .await
            .unwrap();
        check(r.status().is_success(), "manifest fetch OK during 8 parallel downloads");
        let mut all = true;
        for h in handles {
            let b = h.await.unwrap();
            all &= b.as_ref() == payload.as_slice();
        }
        check(all, "8 parallel payload fetches byte-perfect");

        // Duplicate download while one is running → rejected.
        let big_payload = vec![b'Z'; 60 * 1024 * 1024]; // 60MB
        let bundle_big = base.join("bundle-big");
        make_bundle(&bundle_big, "1.2.0", "big.exe", &big_payload, &kp, &[]);
        appupdate::pick_update_bundle_pk(&hs, &alice, bundle_big.to_str().unwrap(), &pk64)
            .unwrap();
        appupdate::add_manual_seeder(&addr);
        let in_progress = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = in_progress.clone();
        let hs2 = hs.clone();
        let http2 = http.clone();
        let bob2 = bob.clone();
        let pk = pk64.clone();
        let dl1 = tokio::spawn(async move {
            appupdate::download_app_update_pk(
                &http2,
                &bob2,
                &hs2,
                move |d, _| {
                    if d > 0 {
                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                },
                &pk,
            )
            .await
        });
        // Wait for first byte, then try a second download and a cancel.
        for _ in 0..500 {
            if in_progress.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        check(
            in_progress.load(std::sync::atomic::Ordering::Relaxed),
            "60MB download started streaming",
        );
        let r = appupdate::download_app_update_pk(&http, &bob, &hs, |_, _| {}, &pk64).await;
        check(r.is_err(), "concurrent download refused (already in progress)");
        appupdate::cancel_download();
        let r1 = dl1.await.unwrap();
        check(r1.is_err(), "cancel mid-download aborts");
        // Cancel left a resumeable .part.
        let leftover = appupdate::dl_dir(&bob)
            .read_dir()
            .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".part")))
            .unwrap_or(false);
        check(leftover, "cancelled download leaves resumeable .part");
        appupdate::cancel_download(); // no-op on idle — must not panic
        check(true, "cancel with nothing running is a no-op");
        // Resume the cancelled download to completion.
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &bob, &hs, |_, _| {}, &pk64).await;
        check(r.is_ok(), "cancelled download resumes to verified completion");

        // ================================================================
        println!("== state machine stress ==");
        // waiting → offer transition when a seeder appears.
        let dave = base.join("dave");
        std::fs::create_dir_all(&dave).unwrap();
        std::fs::write(dave.join("appupdate.json"), r#"{"knownVersion":"5.0.0"}"#).unwrap();
        appupdate::clear_seeders();
        let st = appupdate::app_update_status_pk(&http, &dave, &pk64).await;
        check(st.state == "waiting", "all seeders gone → waiting");
        appupdate::add_manual_seeder(&addr);
        let st = appupdate::app_update_status_pk(&http, &dave, &pk64).await;
        check(st.state == "offer", "seeder reappears → waiting becomes offer");

        // Skip is per-version: a fresh client that skipped 1.2.0 must not be
        // offered it, and a later 3.0.0 bypasses the skip.
        let nina = base.join("nina");
        appupdate::skip_version(&nina, "1.2.0");
        appupdate::add_manual_seeder(&addr);
        let st = appupdate::app_update_status_pk(&http, &nina, &pk64).await;
        check(
            st.state == "skipped" && st.version == "1.2.0",
            &format!("skip hides its version (got {})", st.state),
        );
        appupdate::skip_version(&dave, "1.2.0");
        let bundle_c = base.join("bundle-c");
        make_bundle(&bundle_c, "3.0.0", "v3.exe", &payload, &kp, &[]);
        appupdate::pick_update_bundle_pk(&hs, &alice, bundle_c.to_str().unwrap(), &pk64)
            .unwrap();
        appupdate::add_manual_seeder(&addr);
        let st = appupdate::app_update_status_pk(&http, &dave, &pk64).await;
        check(
            st.state == "offer" && st.version == "3.0.0",
            "newer version bypasses the skip",
        );

        // Downloaded entry whose file vanished → falls through gracefully.
        let eve = base.join("eve");
        std::fs::create_dir_all(&eve).unwrap();
        std::fs::write(
            eve.join("appupdate.json"),
            r#"{"downloaded":{"version":"4.0.0","path":"C:\\gone\\x.exe","sha256":"x"}}"#,
        )
        .unwrap();
        appupdate::clear_seeders();
        let st = appupdate::app_update_status_pk(&http, &eve, &pk64).await;
        check(st.state == "idle", "phantom downloaded entry → idle, not downloaded");
        // Entry pointing at a directory must also be ignored.
        std::fs::create_dir_all(eve.join("a-dir")).unwrap();
        std::fs::write(
            eve.join("appupdate.json"),
            format!(
                r#"{{"downloaded":{{"version":"4.0.0","path":"{}","sha256":"x"}}}}"#,
                eve.join("a-dir").to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let st = appupdate::app_update_status_pk(&http, &eve, &pk64).await;
        check(st.state == "idle", "downloaded path that's a dir → ignored");
        // Unknown extra fields tolerated.
        std::fs::write(
            eve.join("appupdate.json"),
            r#"{"futureField":123,"knownVersion":null}"#,
        )
        .unwrap();
        let st = appupdate::app_update_status_pk(&http, &eve, &pk64).await;
        check(st.state == "idle", "unknown JSON fields tolerated");

        // ================================================================
        println!("== persistence & purge ==");
        let frank = base.join("frank");
        let fdl = appupdate::dl_dir(&frank);
        std::fs::create_dir_all(&fdl).unwrap();
        // Fresh .part must be KEPT (it's a resume target), old swept.
        std::fs::write(fdl.join("fresh.part"), b"x").unwrap();
        std::fs::write(fdl.join("old.part"), b"x").unwrap();
        std::fs::write(fdl.join("old.tmp"), b"x").unwrap();
        let ancient = filetime::FileTime::from_unix_time(1_000_000, 0);
        filetime::set_file_mtime(fdl.join("old.part"), ancient).unwrap();
        filetime::set_file_mtime(fdl.join("old.tmp"), ancient).unwrap();
        appupdate::launch_purge(&frank);
        check(fdl.join("fresh.part").exists(), "fresh .part KEPT (resume target)");
        check(
            !fdl.join("old.part").exists() && !fdl.join("old.tmp").exists(),
            "stale .part/.tmp swept",
        );

        // Seed at running version survives; strictly-older dies.
        let gus = base.join("gus");
        let gs = appupdate::seed_dir(&gus);
        std::fs::create_dir_all(&gs).unwrap();
        std::fs::write(
            gs.join("latest.json"),
            format!(
                r#"{{"app":"com.modpackmanager.app","version":"{cur}","file":"x","sha256":"x","size":1}}"#
            ),
        )
        .unwrap();
        appupdate::launch_purge(&gus);
        check(gs.join("latest.json").exists(), "seed == running survives purge");
        std::fs::write(
            gs.join("latest.json"),
            r#"{"app":"com.modpackmanager.app","version":"0.0.1","file":"x","sha256":"x","size":1}"#,
        )
        .unwrap();
        appupdate::launch_purge(&gus);
        check(!gs.join("latest.json").exists(), "seed < running purged");

        // Skip persists across a "restart" (fresh read of the same dir).
        appupdate::skip_version(&frank, "7.7.7");
        appupdate::clear_seeders();
        let st = appupdate::app_update_status_pk(&http, &frank, &pk64).await;
        check(
            st.state == "skipped" && st.version == "7.7.7",
            "skip survives reload (skipped state without seeder)",
        );

        // Manifest fetch cap boundary: exactly 64KB must pass the cap.
        let padded = format!(
            "{}{}",
            &man_a[..man_a.len() - 1],
            format!(",\"pad\":\"{}\"}}", "z".repeat(65536 - man_a.len() - 9))
        );
        assert_eq!(padded.len(), 65536, "pad math must hit exactly 64KB");
        let p_sig = minisign::sign(
            Some(&kp.pk),
            &kp.sk,
            std::io::Cursor::new(padded.as_bytes()),
            None,
            None,
        )
        .unwrap()
        .to_string();
        let bpad = base.join("bundle-pad");
        std::fs::create_dir_all(&bpad).unwrap();
        std::fs::write(bpad.join(fname), &payload).unwrap();
        std::fs::write(bpad.join("latest.json"), &padded).unwrap();
        std::fs::write(bpad.join("latest.json.minisig"), &p_sig).unwrap();
        appupdate::pick_update_bundle_pk(&hs, &alice, bpad.to_str().unwrap(), &pk64).unwrap();
        appupdate::add_manual_seeder(&addr);
        let hdl = base.join("hdl");
        let r = appupdate::download_app_update_pk(&http, &hdl, &hs, |_, _| {}, &pk64).await;
        check(r.is_ok(), "manifest at exactly 64KB passes the cap");
        // One byte over must fail.
        let over = format!("{}{}", &padded[..padded.len() - 2], "zz\"}");
        let o_sig = minisign::sign(
            Some(&kp.pk),
            &kp.sk,
            std::io::Cursor::new(over.as_bytes()),
            None,
            None,
        )
        .unwrap()
        .to_string();
        let bover = base.join("bundle-over");
        std::fs::create_dir_all(&bover).unwrap();
        std::fs::write(bover.join(fname), &payload).unwrap();
        std::fs::write(bover.join("latest.json"), &over).unwrap();
        std::fs::write(bover.join("latest.json.minisig"), &o_sig).unwrap();
        appupdate::pick_update_bundle_pk(&hs, &alice, bover.to_str().unwrap(), &pk64).unwrap();
        appupdate::add_manual_seeder(&addr);
        let r = appupdate::download_app_update_pk(&http, &hdl, &hs, |_, _| {}, &pk64).await;
        check(r.is_err(), "manifest at 64KB+1 rejected by cap");
        // restore good seed
        appupdate::pick_update_bundle_pk(&hs, &alice, bundle_a.to_str().unwrap(), &pk64)
            .unwrap();

        // ================================================================
        println!("== install & misc ==");
        // install_app_update writes update.cmd + launches the detached helper.
        // The helper (cmd.exe) always spawns; a bogus payload can't reach this
        // point because the manifest hash binds the file at download time.
        let r = appupdate::install_app_update(&hdl);
        check(r.is_ok(), "install_app_update launches helper");
        let cmd_path = hdl.join("updates-dl").join("update.cmd");
        let cmd_body = std::fs::read_to_string(&cmd_path).unwrap_or_default();
        check(
            cmd_body.contains("/S") && cmd_body.contains("start"),
            "update.cmd runs installer silently then relaunches",
        );
        // prepare_install is idempotent.
        let p1 = appupdate::prepare_install(&hdl);
        let p2 = appupdate::prepare_install(&hdl);
        check(p1.is_ok() && p2.is_ok(), "prepare_install idempotent");
        // Corrupting the staged payload fails the install-time hash check.
        let pp = p1.as_ref().unwrap().clone();
        let good = std::fs::read(&pp).unwrap();
        std::fs::write(&pp, b"corrupt").unwrap();
        check(
            appupdate::install_app_update(&hdl).is_err(),
            "corrupt staged payload rejected at install",
        );
        std::fs::write(&pp, &good).unwrap();
        // ensure_server double-call → still serving, no crash.
        check(sync::ensure_server(&hs) && sync::server_running(), "ensure_server idempotent");
        // Progress callback reported sane values during the big download.
        let prog = Arc::new(Mutex::new(Vec::new()));
        let prog2 = prog.clone();
        appupdate::pick_update_bundle_pk(&hs, &alice, bundle_big.to_str().unwrap(), &pk64)
            .unwrap();
        appupdate::add_manual_seeder(&addr);
        let idl = base.join("idl");
        let r = appupdate::download_app_update_pk(
            &http,
            &idl,
            &hs,
            move |d, t| prog2.lock().unwrap().push((d, t)),
            &pk64,
        )
        .await;
        check(r.is_ok(), "60MB payload downloads through full verify path");
        let ticks = prog.lock().unwrap();
        let mono = ticks.windows(2).all(|w| w[1].0 >= w[0].0);
        let fin = ticks.last().map(|(d, t)| *d == *t && *t == 60 * 1024 * 1024).unwrap_or(false);
        check(mono && fin, "progress monotonic, final == size");

        // TTL: discovered seeders expire (~12s) instead of lingering forever.
        appupdate::clear_seeders();
        appupdate::add_manual_seeder("127.0.0.1:9999");
        check(!appupdate::found_seeders().is_empty(), "manual seeder registered");
        std::thread::sleep(std::time::Duration::from_secs(13));
        check(
            appupdate::found_seeders().is_empty(),
            "seeder entries expire after TTL",
        );

        // UDP announce loopback (soft — depends on OS broadcast handling).
        appupdate::start_update_announcer(hs.clone(), alice.clone());
        std::thread::sleep(std::time::Duration::from_secs(4));
        let found = appupdate::found_seeders();
        if found.is_empty() {
            println!("  [SKIP] MMPKAPP broadcast didn't loop back");
        } else {
            println!("  MMPKAPP announce loopback: {} seeder(s)", found.len());
        }
    });

    // ---- cleanup ----
    println!("== cleanup ==");
    drop(hs);
    if let Err(e) = std::fs::remove_dir_all(&base) {
        println!("  [WARN] cleanup failed: {e}");
    } else {
        check(!base.exists(), "stress dir removed — nothing left behind");
    }

    let fails = unsafe { FAILURES };
    println!("\n{} failure(s)", fails);
    std::process::exit(if fails > 0 { 1 } else { 0 });
}

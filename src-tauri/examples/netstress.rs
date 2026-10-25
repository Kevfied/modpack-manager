//! Real multi-process Connect stress test — one OS process hosts, two
//! member processes join + download concurrently, then a third joins
//! late. Not in-process simulation: actual process boundaries, real
//! TCP, real heartbeat tasks.
//!
//! Modes:
//!   netstress                 orchestrates the whole scenario
//!   netstress host <dir>      host child: start → publish → report roster
//!   netstress member <name> <addr> <code> <dir>   member child: join → sync → wait
//!
//! The orchestrator asserts file-byte equality, roster presence, live
//! update propagation (v1 → v2 while members watch), heartbeat liveness
//! DURING concurrent transfers, and clean leaves. Everything lives in
//! %TEMP%/mm-netstress-<pid> and is deleted at the end.

use app_lib::sync;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zip::write::SimpleFileOptions;

static mut FAILURES: u32 = 0;
fn check(cond: bool, msg: &str) {
    println!("  [{}] {}", if cond { "OK" } else { "FAIL" }, msg);
    if !cond {
        unsafe { FAILURES += 1 };
    }
}

fn make_jar(path: &Path, id: &str, body_kb: usize) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let f = std::fs::File::create(path).unwrap();
    let mut z = zip::ZipWriter::new(f);
    let opt = SimpleFileOptions::default();
    z.start_file("fabric.mod.json", opt).unwrap();
    z.write_all(
        format!(r#"{{"id":"{id}","name":"{id}","version":"1.0","depends":{{"minecraft":"1.21.1"}}}}"#)
            .as_bytes(),
    )
    .unwrap();
    z.start_file("data.bin", opt).unwrap();
    let mut chunk = vec![0u8; 8192];
    for (i, b) in chunk.iter_mut().enumerate() {
        *b = ((i * 31 + id.len() * 7) % 251) as u8;
    }
    for _ in 0..(body_kb / 8) {
        z.write_all(&chunk).unwrap();
    }
    z.finish().unwrap();
}

fn sha1_hex(b: &[u8]) -> String {
    use sha1::Digest;
    let mut h = sha1::Sha1::new();
    h.update(b);
    hex::encode(h.finalize())
}

/// The temp dir layout each child gets (store + synced root).
struct ChildEnv {
    store: PathBuf,
    sync_root: PathBuf,
    log_dir: PathBuf,
}
fn env_for(base: &Path, name: &str) -> ChildEnv {
    let d = base.join(name);
    ChildEnv {
        store: d.join("connect.json"),
        sync_root: d.join("instances").join("_synced"),
        log_dir: d.join("logs"),
    }
}

// ============================================================ host child

fn run_host(dir: &str) {
    let e = env_for(Path::new(dir), "host");
    std::fs::create_dir_all(&e.log_dir).unwrap();
    app_lib::applog::init(&e.log_dir);
    let mods = Path::new(dir).join("host-mods");
    let hs: sync::SharedHost = Arc::new(Mutex::new(None));
    sync::host_start(
        &hs,
        &e.store,
        "netstress",
        "s3cret",
        mods.to_str().unwrap(),
        "Net Pack",
        "1.21.1",
        "fabric",
        "0.15.11",
    )
    .unwrap();
    sync::host_publish(&hs, &e.store, "v1", None, false, false, "", "", "", "", "").unwrap();
    println!("READY");
    std::io::stdout().flush().unwrap();

    // stdin commands: PUB <file> — add file + republish · QUIT — stop.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let s = std::io::stdin();
        for l in BufReader::new(s).lines() {
            if tx.send(l.unwrap_or_default()).is_err() {
                return;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        // The roster print every 3s lets the orchestrator watch members
        // come and go in real time.
        {
            let g = hs.lock().unwrap();
            if let Some(h) = g.as_ref() {
                let roster: Vec<String> = h
                    .members
                    .values()
                    .map(|m| format!("{}({})", m.name, m.id))
                    .collect();
                println!("MEMBERS {} {}", roster.len(), roster.join(","));
                std::io::stdout().flush().unwrap();
            }
        }
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(cmd) => {
                let cmd = cmd.trim();
                if cmd == "QUIT" {
                    break;
                }
                if let Some(fname) = cmd.strip_prefix("PUB ") {
                    let mut body = vec![0u8; 96 * 1024];
                    let mut st = fname.len() as u32;
                    for b in body.iter_mut() {
                        st = st.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        *b = (st >> 24) as u8;
                    }
                    make_jar(&mods.join(fname), fname.trim_end_matches(".jar"), 8);
                    let _ = std::fs::write(&mods.join(fname), &body);
                    let note = format!("add {fname}");
                    match sync::host_publish(&hs, &e.store, &note, None, false, false, "", "", "", "", "") {
                        Ok(l) => println!("PUBOK {l}"),
                        Err(err) => println!("PUBFAIL {err}"),
                    }
                    std::io::stdout().flush().unwrap();
                }
            }
            Err(_) => {
                if Instant::now() > deadline {
                    break;
                }
            }
        }
    }
    sync::host_stop(&hs, &e.store);
    println!("BYE");
    std::io::stdout().flush().unwrap();
}

// ============================================================ member child

fn run_member(name: &str, addr: &str, code: &str, dir: &str) {
    let e = env_for(Path::new(dir), name);
    std::fs::create_dir_all(&e.log_dir).unwrap();
    app_lib::applog::init(&e.log_dir);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let http = reqwest::Client::new();
        let cs: sync::SharedClient = Arc::new(Mutex::new(None));
        match sync::join_network(&cs, &e.store, &e.sync_root, addr, code, name).await {
            Ok(l) => println!("JOINED {l}"),
            Err(err) => {
                println!("JOINFAIL {err}");
                std::io::stdout().flush().unwrap();
                std::process::exit(2);
            }
        }
        std::io::stdout().flush().unwrap();

        // stdin commands: SYNC — check+apply if pending · LEAVE · QUIT.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let s = std::io::stdin();
            for l in BufReader::new(s).lines() {
                if tx.send(l.unwrap_or_default()).is_err() {
                    return;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(240);
        let applied_report = |cs: &sync::SharedClient, http: &reqwest::Client| {
            let cs = cs.clone();
            let http = http.clone();
            let store = e.store.clone();
            async move {
                match sync::check_pending(&cs, &http).await {
                    Ok(Some((m, dl, del))) => {
                        println!("PENDING v{} dl={} del={}", m.version, dl, del);
                        std::io::stdout().flush().unwrap();
                        match sync::apply_update_progress(
                            &cs,
                            &http,
                            &store,
                            |p| {
                                if p.phase == "downloading" {
                                    println!("PROG {}/{} files {}/{}B",
                                        p.done_files, p.total_files,
                                        p.done_bytes, p.total_bytes);
                                    std::io::stdout().flush().unwrap();
                                }
                            },
                        )
                        .await
                        {
                            Ok(l) => println!("APPLIED {}", l.lines().last().unwrap_or("")),
                            Err(err) => println!("APPLYFAIL {err}"),
                        }
                    }
                    Ok(None) => println!("UPTODATE"),
                    Err(err) => println!("PENDFAIL {err}"),
                }
                std::io::stdout().flush().unwrap();
            }
        };
        // Initial sync immediately.
        applied_report(&cs, &http).await;
        // Then auto-sync every 3s — a real member polls on its own.
        loop {
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(cmd) => {
                    let cmd = cmd.trim();
                    if cmd == "QUIT" {
                        break;
                    }
                    if cmd == "LEAVE" {
                        sync::leave_network(&cs, &http, &e.store).await;
                        println!("LEFT");
                        std::io::stdout().flush().unwrap();
                        break;
                    }
                    if cmd == "SYNC" {
                        applied_report(&cs, &http).await;
                    }
                }
                Err(_) => {
                    if Instant::now() > deadline {
                        println!("TIMEOUT");
                        std::io::stdout().flush().unwrap();
                        break;
                    }
                    // idle tick — poll like the app does; a fresh publish
                    // triggers a real apply so propagation is observable.
                    if let Ok(Some(_)) = sync::check_pending(&cs, &http).await {
                        applied_report(&cs, &http).await;
                    }
                }
            }
        }
    });
    println!("BYE");
    std::io::stdout().flush().unwrap();
}

// ============================================================ orchestrator

struct Proc {
    child: Child,
    stdin: ChildStdin,
    lines: std::sync::mpsc::Receiver<String>,
}
fn spawn(role: &str, args: &[String]) -> Proc {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new(exe);
    cmd.arg(role).args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().expect("spawn child");
    let stdin = child.stdin.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    std::thread::spawn(move || {
        let mut s = String::new();
        loop {
            s.clear();
            match reader.read_line(&mut s) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if tx.send(s.trim_end().to_string()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    Proc { child, stdin, lines: rx }
}
impl Proc {
    fn send(&mut self, s: &str) {
        let _ = writeln!(self.stdin, "{s}");
        let _ = self.stdin.flush();
    }
    /// Collect lines until `pred` matches or the timeout hits.
    fn wait_for(&self, pred: impl Fn(&str) -> bool, timeout: Duration) -> Vec<String> {
        let mut got = vec![];
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            match self.lines.recv_timeout(Duration::from_millis(300)) {
                Ok(l) => {
                    let hit = pred(&l);
                    got.push(l);
                    if hit {
                        return got;
                    }
                }
                Err(_) => {}
            }
        }
        got
    }
}

fn dir_hashes(dir: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        sha1_hex(&std::fs::read(e.path()).unwrap_or_default()),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("host") => {
            run_host(&args[1]);
            return;
        }
        Some("member") => {
            run_member(&args[1], &args[2], &args[3], &args[4]);
            return;
        }
        _ => {}
    }

    let base = std::env::temp_dir().join(format!("mm-netstress-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let host_mods = base.join("host-mods");
    std::fs::create_dir_all(&host_mods).unwrap();
    for i in 0..8 {
        make_jar(&host_mods.join(format!("mod{i}.jar")), &format!("mod{i}"), 512);
    }
    let addr = format!("127.0.0.1:{}", sync::HTTP_PORT);

    println!("== spawn host ==");
    let mut host = spawn("host", &[base.to_string_lossy().into_owned()]);
    let ready = host.wait_for(|l| l == "READY", Duration::from_secs(15));
    check(
        ready.iter().any(|l| l == "READY"),
        "host child started + published v1",
    );

    println!("== two members join + sync concurrently ==");
    let mk = |name: &str| -> Vec<String> {
        vec![
            name.to_string(),
            addr.clone(),
            "s3cret".into(),
            base.to_string_lossy().into_owned(),
        ]
    };
    let mut alice = spawn("member", &mk("alice"));
    let mut bob = spawn("member", &mk("bob"));
    let ja = alice.wait_for(|l| l.starts_with("JOIN"), Duration::from_secs(15));
    let jb = bob.wait_for(|l| l.starts_with("JOIN"), Duration::from_secs(15));
    check(
        ja.iter().any(|l| l.starts_with("JOINED")),
        &format!("alice joined ({})", ja.last().map(String::as_str).unwrap_or("?")),
    );
    check(
        jb.iter().any(|l| l.starts_with("JOINED")),
        &format!("bob joined ({})", jb.last().map(String::as_str).unwrap_or("?")),
    );

    // While both download — hit the host's /heartbeat and time it. This
    // is THE starvation regression: file streams once held the host
    // mutex and heartbeats queued behind them.
    println!("== heartbeat liveness during concurrent downloads ==");
    let http = reqwest::Client::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut worst_ms = 0u128;
    for _ in 0..6 {
        let t = Instant::now();
        let r = rt.block_on(async {
            http.post(format!("http://{addr}/heartbeat"))
                .header("X-Join-Code", "s3cret")
                .header("X-Member-Id", "999999")
                .json(&serde_json::json!({"memberId":999999,"name":"probe","appliedVersion":0}))
                .timeout(Duration::from_secs(5))
                .send()
                .await
        });
        if let Ok(res) = r {
            let _ = rt.block_on(res.bytes());
        }
        worst_ms = worst_ms.max(t.elapsed().as_millis());
        std::thread::sleep(Duration::from_millis(400));
    }
    check(
        worst_ms < 3000,
        &format!("heartbeat stayed live during transfers ({worst_ms}ms worst)"),
    );

    let aa = alice.wait_for(|l| l.starts_with("APPLIED"), Duration::from_secs(60));
    let ba = bob.wait_for(|l| l.starts_with("APPLIED"), Duration::from_secs(60));
    check(
        aa.iter().any(|l| l.starts_with("APPLIED")),
        "alice applied v1",
    );
    check(
        ba.iter().any(|l| l.starts_with("APPLIED")),
        "bob applied v1 (concurrent with alice)",
    );

    // Byte equality — both members must hold exactly the host's files.
    let want = dir_hashes(&host_mods);
    for name in ["alice", "bob"] {
        let got = dir_hashes(&base.join(name).join("instances/_synced"));
        // synced_root/<networkId>/mods — find whichever dir was created
        let net_root = base.join(name).join("instances/_synced");
        let mut found = vec![];
        if let Ok(rd) = std::fs::read_dir(&net_root) {
            for e in rd.flatten() {
                let mdir = e.path().join("mods");
                if mdir.is_dir() {
                    found = dir_hashes(&mdir);
                }
            }
        }
        check(
            found == want,
            &format!("{name}'s synced mods are byte-identical to host's"),
        );
        let _ = got;
    }

    // Roster — the host should now list both members (prints every 3s).
    println!("== roster ==");
    let roster = host.wait_for(
        |l| l.starts_with("MEMBERS") && l.contains("alice") && l.contains("bob"),
        Duration::from_secs(15),
    );
    check(
        roster
            .last()
            .map(|l| l.contains("alice") && l.contains("bob"))
            .unwrap_or(false),
        &format!("host roster shows alice + bob ({})", roster.last().map(String::as_str).unwrap_or("?")),
    );

    // Live update — host publishes v2 while members watch.
    println!("== live v2 publish ==");
    host.send("PUB extra.jar");
    let pub_ok = host.wait_for(|l| l.starts_with("PUB"), Duration::from_secs(15));
    check(
        pub_ok.iter().any(|l| l.starts_with("PUBOK")),
        "host published v2 mid-session",
    );
    let a2 = alice.wait_for(|l| l.starts_with("APPLIED"), Duration::from_secs(45));
    let b2 = bob.wait_for(|l| l.starts_with("APPLIED"), Duration::from_secs(45));
    check(
        a2.iter().any(|l| l.starts_with("APPLIED")),
        &format!("alice auto-applied v2 [{}]", a2.last().map(String::as_str).unwrap_or("silent")),
    );
    check(
        b2.iter().any(|l| l.starts_with("APPLIED")),
        &format!("bob auto-applied v2 [{}]", b2.last().map(String::as_str).unwrap_or("silent")),
    );
    let want2 = dir_hashes(&host_mods);
    {
        let net_root = base.join("alice").join("instances/_synced");
        let mut found = vec![];
        if let Ok(rd) = std::fs::read_dir(&net_root) {
            for e in rd.flatten() {
                let mdir = e.path().join("mods");
                if mdir.is_dir() {
                    found = dir_hashes(&mdir);
                }
            }
        }
        check(found == want2, "alice v2 bytes identical (incl. extra.jar)");
    }

    // Third member joins late — must get v2 wholesale.
    println!("== late joiner ==");
    let mut carol = spawn("member", &mk("carol"));
    let ca = carol.wait_for(|l| l.starts_with("APPLIED"), Duration::from_secs(60));
    check(
        ca.iter().any(|l| l.starts_with("APPLIED")),
        &format!("carol joined late and applied v2 directly [{}]", ca.last().map(String::as_str).unwrap_or("silent")),
    );

    // Graceful leaves — roster must shrink (host prints MEMBERS every 3s).
    println!("== clean leaves ==");
    alice.send("LEAVE");
    let left = alice.wait_for(|l| l == "LEFT", Duration::from_secs(10));
    check(left.iter().any(|l| l == "LEFT"), "alice left cleanly");
    let shrunk = host.wait_for(
        |l| l.starts_with("MEMBERS") && !l.contains("alice("),
        Duration::from_secs(15),
    );
    check(
        shrunk
            .last()
            .map(|l| !l.contains("alice("))
            .unwrap_or(false),
        "alice's row removed after /leave — no ghost",
    );

    // Wrong-code member must be rejected at the protocol level.
    println!("== wrong code ==");
    let mut mallory = spawn(
        "member",
        &[
            "mallory".to_string(),
            addr.clone(),
            "wrongcode".into(),
            base.to_string_lossy().into_owned(),
        ],
    );
    let mj = mallory.wait_for(|l| l.starts_with("JOIN"), Duration::from_secs(15));
    check(
        mj.iter().any(|l| l.starts_with("JOINFAIL")),
        "bad code rejected",
    );
    let _ = mallory.child.kill();

    // Shut everyone down.
    for p in [&mut bob, &mut carol] {
        p.send("QUIT");
    }
    host.send("QUIT");
    let _ = host.wait_for(|l| l == "BYE", Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(500));
    for p in [&mut alice, &mut bob, &mut carol, &mut mallory, &mut host] {
        let _ = p.child.kill();
        let _ = p.child.wait();
    }

    if let Err(e) = std::fs::remove_dir_all(&base) {
        println!("  [WARN] cleanup: {e}");
    } else {
        check(!base.exists(), "netstress dir removed — nothing left behind");
    }
    let fails = unsafe { FAILURES };
    println!("\n{} failure(s)", fails);
    std::process::exit(if fails > 0 { 1 } else { 0 });
}

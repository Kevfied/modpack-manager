import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import { listen } from "@tauri-apps/api/event";
import {
  ackAppUpdate,
  appHidden,
  appUpdateStatus,
  cancelAppUpdate,
  compatReport,
  connectStatus,
  downloadAppUpdate,
  installAppUpdate,
  openLogsDir,
  openReleaseNotes,
  resolveMods,
  scanFiles,
  scanFolder,
  skipAppVersion,
  tailLog,
  unskipAppVersion,
} from "./lib/api";
import { disable, enable, isEnabled } from "@tauri-apps/plugin-autostart";
import type {
  AppUpdateStatus,
  CompatReport,
  ModInfo,
  PackSettings,
  ResolvedMod,
  ScanResult,
} from "./lib/api";
import ModsView from "./views/ModsView";
import BuildView from "./views/BuildView";
import LoaderView from "./views/LoaderView";
import ConnectView from "./views/ConnectView";

type View = "loader" | "mods" | "build" | "connect";

export default function App() {
  const [view, setView] = useState<View>("loader");
  const [folder, setFolder] = useState<string | null>(null);
  const [mods, setMods] = useState<ModInfo[]>([]);
  const [resolved, setResolved] = useState<Record<string, ResolvedMod>>({});
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [compat, setCompat] = useState<CompatReport | null>(null);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("");
  // Titlebar gear: startup toggle + log access.
  const [gearOpen, setGearOpen] = useState(false);
  const [autostartOn, setAutostartOn] = useState(false);
  useEffect(() => {
    isEnabled()
      .then(setAutostartOn)
      .catch(() => {});
  }, []);
  const [settings, setSettings] = useState<PackSettings>({
    name: "My Modpack",
    version: "1.0.0",
    summary: "",
    mcVersion: "",
    loader: "fabric",
    loaderVersion: "",
  });

  // Auto-fill target from the compat suggestion once a scan resolves.
  useEffect(() => {
    if (!compat) return;
    setSettings((s) => ({
      ...s,
      mcVersion: s.mcVersion || compat.suggestedMcVersion || "",
      loader: compat.suggestedLoader || s.loader,
    }));
  }, [compat]);

  // Trailing progress events can land after a scan fully finished —
  // without the gate the statusbar sticks on "hashing n/n…".
  const scanning = useRef(false);
  useEffect(() => {
    const un = listen<{ done: number; total: number }>("scan-progress", (e) => {
      if (scanning.current) {
        setStatus(`hashing ${e.payload.done}/${e.payload.total}…`);
      }
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // ---- app update (LAN) ----
  const [upd, setUpd] = useState<AppUpdateStatus | null>(null);
  const [appVer, setAppVer] = useState("");
  // "Later" hides the banner for this state+version until it changes.
  const [updHidden, setUpdHidden] = useState("");
  // Optimistic hide for the post-update welcome — ack clears it in the
  // store, but don't make the user wait a poll cycle for it to vanish.
  const [welcomeHidden, setWelcomeHidden] = useState("");
  const [updConfirm, setUpdConfirm] = useState(false);
  const [updHosting, setUpdHosting] = useState(false);
  const [updErr, setUpdErr] = useState("");

  // While a download request is in flight, ignore stale "offer" poll
  // results — an in-flight poll from before the click would otherwise
  // snap the bar back to "Update now" until the next tick.
  const dlRequested = useRef("");
  useEffect(() => {
    let alive = true;
    const poll = async () => {
      try {
        const s = await appUpdateStatus();
        if (!alive) return;
        if (
          dlRequested.current &&
          s.state === "offer" &&
          s.version === dlRequested.current
        ) {
          return; // stale pre-click result — keep the downloading view
        }
        if (s.state !== "offer") dlRequested.current = "";
        setUpd(s);
      } catch {
        /* backend not ready */
      }
    };
    poll();
    getVersion().then(setAppVer).catch(() => {});
    const t = setInterval(() => {
      if (!appHidden.current) poll();
    }, 5000);
    const un = listen("appupdate-progress", poll);
    // A seeder announce (new or version-bumped) re-checks instantly —
    // the offer shows within a second of a peer publishing.
    const un2 = listen("appupdate-offer", poll);
    return () => {
      alive = false;
      clearInterval(t);
      un.then((f) => f());
      un2.then((f) => f());
    };
  }, []);

  const updKey = upd ? `${upd.state}:${upd.version}` : "";
  const updVisible = !!upd && upd.state !== "idle" && updHidden !== updKey;

  // Poll-gated actions: hide instantly so the click feels immediate; the
  // next poll brings the bar back in its new state (offer→skipped etc.).
  // On failure, un-hide so the current state stays visible.
  const updAction = (fn: () => Promise<unknown>) => {
    setUpdHidden(updKey);
    fn().catch(() => setUpdHidden(""));
  };

  const onUpdateNow = async () => {
    setUpdErr("");
    if (upd) {
      dlRequested.current = upd.version;
      setUpd({ ...upd, state: "downloading", progress: 0 });
    }
    try {
      await downloadAppUpdate();
    } catch (e) {
      dlRequested.current = "";
      setUpdErr(String(e));
    }
  };
  const onInstall = async () => {
    setUpdErr("");
    try {
      setUpdHosting((await connectStatus()).mode === "hosting");
      setUpdConfirm(true);
    } catch {
      setUpdConfirm(true);
    }
  };

  // Toggles don't await the report — last-issued must win, not
  // last-resolved, or the banner can describe an older selection.
  const compatSeq = useRef(0);
  const refreshCompat = useCallback(
    async (allMods: ModInfo[], res: Record<string, ResolvedMod>, sel: Set<string>) => {
      const seq = ++compatSeq.current;
      if (sel.size === 0) { setCompat(null); return; }
      const report = await compatReport(allMods, res, [...sel]);
      if (seq === compatSeq.current) setCompat(report);
    },
    []
  );

  // Merge a scan's results into the working set (folders + picked files
  // coexist — dedup by path, new jars get selected), then resolve on Modrinth.
  const mergeScan = useCallback(
    async (result: ScanResult) => {
      const byPath = new Map(mods.map((m) => [m.path, m]));
      for (const m of result.mods) byPath.set(m.path, m);
      const merged = [...byPath.values()].sort((a, b) =>
        a.filename.localeCompare(b.filename)
      );
      const sel = new Set(selected);
      for (const m of result.mods) sel.add(m.path);
      setMods(merged);
      setSelected(sel);
      setStatus(`Found ${result.mods.length} jars — resolving on Modrinth…`);
      const res = await resolveMods(merged);
      const mergedRes = { ...resolved, ...res };
      setResolved(mergedRes);
      await refreshCompat(merged, mergedRes, sel);
      setStatus("");
    },
    [mods, selected, resolved, refreshCompat]
  );

  const onScan = useCallback(
    async (path: string) => {
      setBusy(true);
      scanning.current = true;
      setStatus("Scanning…");
      try {
        setFolder(path);
        await mergeScan(await scanFolder(path));
      } catch (e) {
        setStatus(`Error: ${e}`);
      } finally {
        scanning.current = false;
        setBusy(false);
      }
    },
    [mergeScan]
  );

  const onScanFiles = useCallback(
    async (paths: string[]) => {
      setBusy(true);
      scanning.current = true;
      setStatus("Scanning…");
      try {
        await mergeScan(await scanFiles(paths));
      } catch (e) {
        setStatus(`Error: ${e}`);
      } finally {
        scanning.current = false;
        setBusy(false);
      }
    },
    [mergeScan]
  );

  const onRemove = useCallback(
    (path: string) => {
      const next = mods.filter((m) => m.path !== path);
      const sel = new Set(selected);
      sel.delete(path);
      const res = { ...resolved };
      const removed = mods.find((m) => m.path === path);
      if (removed) delete res[removed.sha1];
      setMods(next);
      setSelected(sel);
      setResolved(res);
      refreshCompat(next, res, sel);
    },
    [mods, selected, resolved, refreshCompat]
  );

  const onClear = useCallback(() => {
    setMods([]);
    setSelected(new Set());
    setResolved({});
    setCompat(null);
    setFolder(null);
    setStatus("");
  }, []);

  const onToggle = useCallback(
    (path: string, on: boolean) => {
      const next = new Set(selected);
      if (on) next.add(path); else next.delete(path);
      setSelected(next);
      refreshCompat(mods, resolved, next);
    },
    [selected, mods, resolved, refreshCompat]
  );

  const onSetAll = useCallback(
    (on: boolean) => {
      const next = on ? new Set(mods.map((m) => m.path)) : new Set<string>();
      setSelected(next);
      refreshCompat(mods, resolved, next);
    },
    [mods, resolved, refreshCompat]
  );

  const selectedMods = mods.filter((m) => selected.has(m.path));
  const win = getCurrentWindow();

  return (
    <>
      <header className="titlebar" data-tauri-drag-region>
        <svg className="app-icon" viewBox="0 0 24 24" aria-hidden="true">
          <path
            d="M12 2.5 21 7.5v9l-9 5-9-5v-9l9-5zM3 7.5l9 5m0 0 9-5m-9 5v9"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.6"
            strokeLinejoin="round"
          />
        </svg>
        <h1>modpack-manager</h1>
        <nav className="nav-tabs">
          <button
            className={view === "loader" ? "active" : ""}
            onClick={() => setView("loader")}
          >
            Loader
          </button>
          <button className={view === "mods" ? "active" : ""} onClick={() => setView("mods")}>
            Mods{mods.length > 0 ? ` (${selected.size}/${mods.length})` : ""}
          </button>
          <button
            className={view === "build" ? "active" : ""}
            onClick={() => setView("build")}
            disabled={selectedMods.length === 0}
          >
            Build
          </button>
          <button
            className={view === "connect" ? "active" : ""}
            onClick={() => setView("connect")}
          >
            Connect
          </button>
        </nav>
        <div className="spacer" style={{ flex: 1 }} />
        <div className="gear-wrap">
          <button
            className="gear-btn"
            title="Settings"
            onClick={() => setGearOpen((v) => !v)}
          >
            &#9881;
          </button>
          {gearOpen && (
            <>
              <div className="gear-veil" onClick={() => setGearOpen(false)} />
              <div className="gear-menu">
                <label>
                  <input
                    type="checkbox"
                    checked={autostartOn}
                    onChange={async (e) => {
                      const on = e.target.checked;
                      try {
                        if (on) await enable();
                        else await disable();
                        setAutostartOn(on);
                      } catch {
                        /* registry unavailable */
                      }
                    }}
                  />
                  Launch on Windows startup
                </label>
                <button
                  onClick={() => {
                    setGearOpen(false);
                    openLogsDir();
                  }}
                >
                  Open logs folder
                </button>
                <button
                  onClick={async () => {
                    setGearOpen(false);
                    try {
                      const lines = await tailLog(200);
                      await navigator.clipboard.writeText(lines.join("\n"));
                      setStatus("✓ last 200 log lines copied");
                    } catch (e) {
                      setStatus(`x logs: ${e}`);
                    }
                  }}
                >
                  Copy logs to clipboard
                </button>
              </div>
            </>
          )}
        </div>
        <div className="win-btns">
          <button onClick={() => win.minimize()} title="Minimize">&#x2014;</button>
          <button onClick={() => win.toggleMaximize()} title="Maximize">&#x25A1;</button>
          <button className="close" onClick={() => win.close()} title="Close">&#x2715;</button>
        </div>
      </header>

      {upd?.justUpdated && welcomeHidden !== upd.justUpdated && (
        <div className="upd-bar upd-good">
          <span>
            ✓ updated to <b>v{upd.justUpdated}</b>
            {upd.updatedNotes && (
              <span className="dim upd-notes">{upd.updatedNotes}</span>
            )}
          </span>
          <button onClick={() => openReleaseNotes(upd.justUpdated)}>
            Learn more
          </button>
          <button
            onClick={() => {
              setWelcomeHidden(upd.justUpdated);
              ackAppUpdate().catch(() => setWelcomeHidden(""));
            }}
          >
            Dismiss
          </button>
        </div>
      )}

      {updVisible && upd && (
        <div className="upd-bar">
          {upd.state === "offer" && (
            <>
              <span>
                ▲ update <b>v{upd.version}</b> available
                {upd.size > 0 && <> ({(upd.size / 1048576).toFixed(1)} MB)</>}
                {upd.source && <span className="dim"> from {upd.source}</span>}
                {upd.notes && <span className="dim upd-notes">{upd.notes}</span>}
              </span>
              <button onClick={onUpdateNow}>Update now</button>
              <button onClick={() => openReleaseNotes(upd.version)}>
                Read more
              </button>
              <button onClick={() => setUpdHidden(updKey)}>Later</button>
              <button onClick={() => updAction(() => skipAppVersion(upd.version))}>
                Skip this version
              </button>
            </>
          )}
          {upd.state === "downloading" && (
            <>
              <span>
                downloading v{upd.version}… {(upd.progress * 100).toFixed(0)}%
              </span>
              <div className="upd-prog">
                <div style={{ width: `${upd.progress * 100}%` }} />
              </div>
              <button onClick={() => updAction(() => cancelAppUpdate())}>Cancel</button>
              <button onClick={() => setUpdHidden(updKey)}>Hide</button>
            </>
          )}
          {upd.state === "downloaded" && (
            <>
              <span>
                update <b>v{upd.version}</b> verified — ready to install
                {upd.notes && <span className="dim upd-notes">{upd.notes}</span>}
              </span>
              <button onClick={onInstall}>Restart &amp; install</button>
              <button onClick={() => openReleaseNotes(upd.version)}>
                Read more
              </button>
              <button onClick={() => setUpdHidden(updKey)}>Later</button>
            </>
          )}
          {upd.state === "waiting" && (
            <>
              <span>
                update <b>v{upd.version}</b> exists — waiting for a seeder…
              </span>
              <button onClick={() => updAction(() => skipAppVersion(upd.version))}>Skip</button>
              <button onClick={() => setUpdHidden(updKey)}>Hide</button>
            </>
          )}
          {upd.state === "skipped" && (
            <>
              <span className="dim">update v{upd.version} skipped</span>
              <button onClick={() => updAction(() => unskipAppVersion())}>Reconsider</button>
              <button onClick={() => setUpdHidden(updKey)}>Hide</button>
            </>
          )}
          {updErr && <span className="upd-err">{updErr}</span>}
        </div>
      )}

      {updConfirm && upd && (
        <div className="upd-overlay" onClick={() => setUpdConfirm(false)}>
          <div className="upd-card" onClick={(e) => e.stopPropagation()}>
            <h3>Install v{upd.version}?</h3>
            <p className="dim">
              The app will close and the installer will run — takes a few
              seconds, then v{upd.version} opens.
            </p>
            {updHosting && (
              <p className="upd-warn">
                You're hosting a Connect network — members will disconnect
                until you're back.
              </p>
            )}
            <p className="dim">
              If Windows flags the installer: "More info" → "Run anyway".
              Your networks, packs and settings are preserved.
            </p>
            <div className="upd-actions">
              <button
                onClick={() => {
                  setUpdConfirm(false);
                  installAppUpdate().catch((e) => setUpdErr(String(e)));
                }}
              >
                Restart &amp; install
              </button>
              <button onClick={() => setUpdConfirm(false)}>Cancel</button>
            </div>
          </div>
        </div>
      )}

      {view === "mods" && (
        <ModsView
          mods={mods}
          resolved={resolved}
          selected={selected}
          folder={folder}
          busy={busy}
          compat={compat}
          onScan={onScan}
          onScanFiles={onScanFiles}
          onClear={onClear}
          onRemove={onRemove}
          onToggle={onToggle}
          onSetAll={onSetAll}
        />
      )}
      {view === "build" && (
        <BuildView
          mods={selectedMods}
          resolved={resolved}
          compat={compat}
          folder={folder}
          settings={settings}
          setSettings={setSettings}
        />
      )}
      {view === "loader" && (
        <LoaderView
          mods={selectedMods}
          compat={compat}
          settings={settings}
          setSettings={setSettings}
        />
      )}
      {view === "connect" && <ConnectView settings={settings} />}

      <footer className="statusbar">
        <span>{status || "ready"}</span>
        {folder && <span className="dim mono">{folder}</span>}
        <span style={{ flex: 1 }} />
        {appVer && <span className="dim mono">v{appVer}</span>}
      </footer>
    </>
  );
}

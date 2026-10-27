import { useEffect, useState } from "react";
import {
  appHidden,
  detectLaunchers,
  listSyncedPacks,
  loadIntoLauncher,
  loadPackFile,
  loadSyncedPack,
  packExists,
  packFileInfo,
  suggestPackMeta,
} from "../lib/api";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import type {
  CompatReport,
  DetectedLoaders,
  LauncherInfo,
  ModInfo,
  PackConfig,
  PackFileInfo,
  PackSettings,
  SyncedPackInfo,
} from "../lib/api";

interface Props {
  mods: ModInfo[];
  compat: CompatReport | null;
  settings: PackSettings;
  setSettings: React.Dispatch<React.SetStateAction<PackSettings>>;
}

const LOADER_ICON: Record<string, string> = {
  prism: "◆",
  vanilla: "■",
  tlauncher: "▲",
};

export default function LoaderView(props: Props) {
  const { settings: cfg } = props;
  const [detected, setDetected] = useState<DetectedLoaders | null>(null);
  const [packFile, setPackFile] = useState<string | null>(null);
  const [packInfo, setPackInfo] = useState<PackFileInfo | null>(null);
  const [synced, setSynced] = useState<SyncedPackInfo[]>([]);
  const [log, setLog] = useState<string[]>([]);
  const [busy, setBusy] = useState<string | null>(null);

  const pushLog = (line: string) =>
    setLog((l) => [...l, ...line.split("\n").filter(Boolean)]);

  // Live lists — a Connect apply mid-session adds the synced pack here,
  // a newly installed launcher appears too, without revisiting the tab.
  useEffect(() => {
    const load = (first: boolean) => {
      detectLaunchers()
        .then(setDetected)
        .catch((e) => {
          if (first) pushLog(`x detection failed: ${e}`);
        });
      listSyncedPacks().then(setSynced).catch(() => {});
    };
    load(true);
    const t = window.setInterval(() => {
      if (!appHidden.current) load(false);
    }, 4000);
    return () => window.clearInterval(t);
  }, []);

  const makeConfig = (): PackConfig => ({
    ...cfg,
    selectedPaths: props.mods.map((m) => m.path),
  });

  // Ask before clobbering an existing pack/instance of the same name.
  const confirmOverwrite = async (l: LauncherInfo, name: string): Promise<boolean> => {
    if (!(await packExists(l, name))) return true;
    return await confirm(
      `A pack named "${name}" already exists in ${l.name}.\nOverwrite it?`,
      { title: "Pack exists", kind: "warning", okLabel: "Overwrite", cancelLabel: "Cancel" }
    );
  };

  const launchSelection = async (l: LauncherInfo) => {
    if (!(await confirmOverwrite(l, cfg.name))) return;
    setBusy(`sel-${l.kind}`);
    pushLog(`> ${l.name}: staging ${props.mods.length} mod(s)…`);
    try {
      pushLog(await loadIntoLauncher(l, makeConfig()));
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(null);
    }
  };

  const pickPackFile = async () => {
    const file = await open({
      filters: [{ name: "Modpack", extensions: ["mrpack", "zip"] }],
      title: "Pick a modpack file",
    });
    if (typeof file !== "string") return;
    setPackFile(file);
    setPackInfo(null);
    try {
      setPackInfo(await packFileInfo(file));
    } catch {
      /* info is optional — ignore */
    }
  };

  // A bare folder of .jar files works too — same deploy path the Connect
  // "apply" uses, with loader/MC guessed from the jars themselves.
  const pickPackFolder = async () => {
    const dir = await open({ directory: true, title: "Pick a folder of mods" });
    if (typeof dir !== "string") return;
    setPackFile(dir);
    setPackInfo(null);
    try {
      const m = await suggestPackMeta(dir);
      setPackInfo({
        name: m.packName,
        mcVersion: m.mcVersion,
        loader: m.loader,
        loaderVersion: m.loaderVersion,
      });
    } catch {
      /* info is optional — ignore */
    }
  };

  const launchFile = async (l: LauncherInfo) => {
    if (!packFile) return;
    const name = packInfo?.name || cfg.name;
    if (!(await confirmOverwrite(l, name))) return;
    setBusy(`file-${l.kind}`);
    pushLog(`> ${l.name}: loading ${packFile.split(/[\\/]/).pop()}…`);
    try {
      pushLog(
        await loadPackFile(l, packFile, {
          ...makeConfig(),
          name,
          mcVersion: packInfo?.mcVersion || cfg.mcVersion,
          loader: packInfo?.loader || cfg.loader,
          loaderVersion: packInfo?.loaderVersion || cfg.loaderVersion,
        })
      );
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(null);
    }
  };

  const loadSynced = async (l: LauncherInfo, p: SyncedPackInfo) => {
    if (!(await confirmOverwrite(l, p.name))) return;
    setBusy(`sync-${l.kind}-${p.networkId}`);
    pushLog(`> ${l.name}: loading synced pack "${p.name}" v${p.version}…`);
    try {
      pushLog(await loadSyncedPack(p.networkId, l));
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(null);
    }
  };

  const launcherCards = (onClick: (l: LauncherInfo) => void, prefix: string) =>
    detected?.launchers.map((l) => (
      <div className="launcher-card" key={prefix + l.kind}>
        <div style={{ fontSize: "1.4rem" }}>{LOADER_ICON[l.kind] ?? "●"}</div>
        <div className="grow">
          <div className="name">{l.name}</div>
          <div className="sub mono">{l.path ?? l.dataDir ?? ""}</div>
        </div>
        <button
          className="primary"
          disabled={busy !== null}
          onClick={() => onClick(l)}
        >
          {busy === `${prefix}-${l.kind}` ? "Working…" : "Load"}
        </button>
      </div>
    ));

  return (
    <div className="view">
      {props.mods.length > 0 && (
        <div className="panel">
          <h3>From your selection — {props.mods.length} mod(s)</h3>
          <div className="dim" style={{ marginBottom: 10 }}>
            {cfg.name} · MC {cfg.mcVersion || "?"} · {cfg.loader}
            {cfg.loaderVersion ? ` ${cfg.loaderVersion}` : ""} — edit in Build tab
          </div>
          {launcherCards(launchSelection, "sel")}
        </div>
      )}

      <div className="panel">
        <h3>From a pack file or mods folder</h3>
        <div className="toolbar">
          <button onClick={pickPackFile}>Choose .mrpack / .zip…</button>
          <button onClick={pickPackFolder}>Choose mods folder…</button>
          {packFile && <span className="mono dim">{packFile}</span>}
        </div>
        {packInfo && (
          <div className="dim" style={{ marginBottom: 10 }}>
            {packInfo.name}
            {packInfo.mcVersion ? ` · MC ${packInfo.mcVersion}` : ""}
            {packInfo.loader ? ` · ${packInfo.loader} ${packInfo.loaderVersion ?? ""}` : ""}
          </div>
        )}
        {packFile &&
          (detected?.launchers.length ? (
            launcherCards(launchFile, "file")
          ) : (
            <div className="dim">No launchers detected.</div>
          ))}
        {!packFile && (
          <div className="dim">
            Give it a pack file — or just a folder of .jar mods — and pick
            where to load it.
          </div>
        )}
      </div>

      {synced.length > 0 && (
        <div className="panel">
          <h3>Synced packs (from Connect)</h3>
          {synced.map((p) => (
            <div key={p.networkId} style={{ marginBottom: 10 }}>
              <div className="dim" style={{ marginBottom: 6 }}>
                <b style={{ color: "var(--text)" }}>{p.name}</b> · v{p.version} ·{" "}
                {p.fileCount} mod(s) · MC {p.mcVersion || "?"} · {p.loader}
                {p.loaderVersion ? ` ${p.loaderVersion}` : ""}
              </div>
              {detected?.launchers.map((l) => (
                <button
                  key={p.networkId + l.kind}
                  style={{ marginRight: 8 }}
                  disabled={busy !== null}
                  onClick={() => loadSynced(l, p)}
                >
                  {busy === `sync-${l.kind}-${p.networkId}` ? "Working…" : `Load → ${l.name}`}
                </button>
              ))}
            </div>
          ))}
        </div>
      )}

      {props.mods.length === 0 && !packFile && synced.length === 0 && (
        <div className="banner info">
          Nothing to load yet — scan a folder in Mods, or pick a pack file above.
        </div>
      )}

      {log.length > 0 && (
        <div className="log-box">
          {log.map((l, i) => (
            <div key={i} className={l.startsWith("✓") ? "ok" : l.startsWith("x") || l.startsWith("!") ? "err" : ""}>
              {l}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  buildModlist,
  buildMrpack,
  buildZip,
  checkUpdates,
  findMissingDeps,
  getGameVersions,
  getLoaderVersions,
} from "../lib/api";
import type {
  BuildResult,
  CompatReport,
  DependencyWarning,
  ModInfo,
  PackConfig,
  PackSettings,
  ResolvedMod,
  UpdateInfo,
} from "../lib/api";

interface Props {
  mods: ModInfo[];
  resolved: Record<string, ResolvedMod>;
  compat: CompatReport | null;
  folder: string | null;
  settings: PackSettings;
  setSettings: (s: PackSettings) => void;
}

const LOADERS = ["fabric", "forge", "neoforge", "quilt"];

export default function BuildView(props: Props) {
  const { settings: cfg, setSettings: setCfg } = props;
  const { name, version, summary, mcVersion, loader, loaderVersion } = cfg;
  const set = (patch: Partial<PackSettings>) => setCfg({ ...cfg, ...patch });
  const [mcVersions, setMcVersions] = useState<string[]>([]);
  const [loaderVersions, setLoaderVersions] = useState<string[]>([]);
  const [deps, setDeps] = useState<DependencyWarning[]>([]);
  const [updates, setUpdates] = useState<UpdateInfo[] | null>(null);
  const [log, setLog] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);

  const pushLog = (line: string) => setLog((l) => [...l, line]);

  useEffect(() => {
    getGameVersions().then(setMcVersions).catch(() => {});
    findMissingDeps(props.mods, props.resolved, props.mods.map((m) => m.path))
      .then(setDeps)
      .catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (!mcVersion || !loader) return;
    getLoaderVersions(loader, mcVersion)
      .then((vs) => {
        setLoaderVersions(vs);
        if (vs.length && !vs.includes(loaderVersion)) set({ loaderVersion: vs[0] });
      })
      .catch(() => setLoaderVersions([]));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loader, mcVersion]);

  const makeConfig = (): PackConfig => ({
    ...cfg,
    selectedPaths: props.mods.map((m) => m.path),
  });

  const pickOutDir = async (): Promise<string | null> => {
    const dir = await open({ directory: true, title: "Where to save the pack" });
    return typeof dir === "string" ? dir : null;
  };

  const run = async (what: string, fn: (outDir: string) => Promise<BuildResult>) => {
    const outDir = await pickOutDir();
    if (!outDir) return;
    setBusy(true);
    pushLog(`> ${what}…`);
    try {
      const r = await fn(outDir);
      pushLog(
        `✓ ${r.outputPath} — ${r.resolvedCount} Modrinth ref(s), ${r.overrideCount} bundled jar(s)`
      );
    } catch (e) {
      pushLog(`✗ ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const checkForUpdates = async () => {
    setBusy(true);
    pushLog(`> Checking for ${loader} ${mcVersion} builds of your mods…`);
    try {
      const u = await checkUpdates(loader, mcVersion);
      setUpdates(u);
      pushLog(`✓ ${u.length} mod(s) have a ${loader} build for ${mcVersion}`);
    } catch (e) {
      pushLog(`✗ update check failed: ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const unresolved = props.mods.filter((m) => !props.resolved[m.sha1]?.projectId);

  return (
    <div className="view">
      <div className="panel">
        <h3>Pack</h3>
        <div className="form-grid">
          <label>Name</label>
          <input type="text" value={name} onChange={(e) => set({ name: e.target.value })} />
          <label>Version</label>
          <input type="text" value={version} onChange={(e) => set({ version: e.target.value })} />
          <label>Summary</label>
          <input
            type="text"
            value={summary}
            onChange={(e) => set({ summary: e.target.value })}
            placeholder="Optional description"
          />
          <label>MC version</label>
          <select value={mcVersion} onChange={(e) => set({ mcVersion: e.target.value })}>
            <option value="">— pick —</option>
            {props.compat?.commonGameVersions.map((v) => (
              <option key={v} value={v}>{v} (common)</option>
            ))}
            {mcVersions
              .filter((v) => !props.compat?.commonGameVersions.includes(v))
              .map((v) => (
                <option key={v} value={v}>{v}</option>
              ))}
          </select>
          <label>Loader</label>
          <select value={loader} onChange={(e) => set({ loader: e.target.value })}>
            {LOADERS.map((l) => (
              <option key={l} value={l}>
                {l}{props.compat?.commonLoaders.includes(l) ? " (common)" : ""}
              </option>
            ))}
          </select>
          <label>Loader version</label>
          <select value={loaderVersion} onChange={(e) => set({ loaderVersion: e.target.value })}>
            {loaderVersions.length === 0 && <option value="">—</option>}
            {loaderVersions.map((v) => (
              <option key={v} value={v}>{v}</option>
            ))}
          </select>
        </div>
      </div>

      {deps.length > 0 && (
        <div className="banner warn">
          <b>Possibly missing dependencies:</b>
          {deps.map((d, i) => (
            <div key={i}>• {d.modName} requires {d.requiredTitle ?? d.requiredProject}</div>
          ))}
        </div>
      )}
      {unresolved.length > 0 && (
        <div className="banner info">
          {unresolved.length} jar(s) not on Modrinth will be bundled inside the pack as files
          ({unresolved.map((m) => m.filename).join(", ")}).
        </div>
      )}

      <div className="panel">
        <h3>Export</h3>
        <div className="toolbar">
          <button
            className="primary"
            disabled={busy || !mcVersion || !loaderVersion}
            onClick={() => run("Building .mrpack", (d) => buildMrpack(makeConfig(), d))}
          >
            .mrpack
          </button>
          <button
            disabled={busy || !mcVersion}
            onClick={() => run("Building .zip", (d) => buildZip(makeConfig(), d))}
          >
            .zip (mods folder)
          </button>
          <button
            disabled={busy}
            onClick={() => run("Writing modlist", (d) => buildModlist(makeConfig(), props.resolved, d))}
          >
            modlist.txt
          </button>
          <div className="spacer" />
          <button disabled={busy || !mcVersion || !loader} onClick={checkForUpdates}>
            Check updates for target
          </button>
        </div>
        {!mcVersion && <div className="dim">Pick an MC version to enable export.</div>}
        {updates && (
          <div style={{ marginTop: 10 }}>
            {updates.length === 0 ? (
              <div className="dim">No matching builds found on Modrinth for this target.</div>
            ) : (
              updates.map((u) => (
                <div key={u.filename} className="dim" style={{ fontSize: "0.85rem" }}>
                  {u.projectTitle ?? u.filename}: {u.newVersion}
                </div>
              ))
            )}
          </div>
        )}
        {log.length > 0 && (
          <div className="log-box" style={{ marginTop: 12 }}>
            {log.map((l, i) => (
              <div key={i} className={l.startsWith("✓") ? "ok" : l.startsWith("✗") ? "err" : ""}>
                {l}
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

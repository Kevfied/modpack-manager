import { useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import type { CompatReport, ModInfo, ResolvedMod } from "../lib/api";

interface Props {
  mods: ModInfo[];
  resolved: Record<string, ResolvedMod>;
  selected: Set<string>;
  folder: string | null;
  busy: boolean;
  compat: CompatReport | null;
  onScan: (path: string) => void;
  onScanFiles: (paths: string[]) => void;
  onClear: () => void;
  onRemove: (path: string) => void;
  onToggle: (path: string, on: boolean) => void;
  onSetAll: (on: boolean) => void;
}

function fmtSize(n: number): string {
  if (n > 1 << 20) return `${(n / (1 << 20)).toFixed(1)} MB`;
  if (n > 1 << 10) return `${(n / (1 << 10)).toFixed(0)} KB`;
  return `${n} B`;
}

export default function ModsView(props: Props) {
  const [filter, setFilter] = useState("");

  const pickFolder = async () => {
    const dir = await open({ directory: true, title: "Add a folder of mods" });
    if (typeof dir === "string") props.onScan(dir);
  };

  const pickFiles = async () => {
    const files = await open({
      multiple: true,
      title: "Pick mod jars",
      filters: [{ name: "Mod jars", extensions: ["jar"] }],
    });
    if (Array.isArray(files) && files.length) props.onScanFiles(files);
    else if (typeof files === "string") props.onScanFiles([files]);
  };

  const filtered = useMemo(() => {
    const q = filter.toLowerCase();
    if (!q) return props.mods;
    return props.mods.filter((m) => {
      const r = props.resolved[m.sha1];
      return (
        m.filename.toLowerCase().includes(q) ||
        (m.name ?? "").toLowerCase().includes(q) ||
        (m.modId ?? "").toLowerCase().includes(q) ||
        (r?.projectTitle ?? "").toLowerCase().includes(q)
      );
    });
  }, [props.mods, props.resolved, filter]);

  const matchCount = props.mods.filter((m) => props.resolved[m.sha1]?.projectId).length;

  if (props.mods.length === 0 && !props.busy) {
    return (
      <div className="view">
        <div className="empty-state">
          <div className="big">Add mods to get started</div>
          <div>
            Add a whole folder (scanned recursively) or pick specific .jar
            files — you can mix both.
          </div>
          <div>
            <button className="primary" onClick={pickFolder}>Add folder…</button>
            {" "}
            <button onClick={pickFiles}>Add mods…</button>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="view">
      <div className="toolbar">
        <button className="primary" onClick={pickFolder} disabled={props.busy}>
          Add folder…
        </button>
        <button onClick={pickFiles} disabled={props.busy}>
          Add mods…
        </button>
        <button onClick={props.onClear} disabled={props.busy}>
          Clear
        </button>
        {props.folder && <span className="dim mono">{props.folder}</span>}
        <div className="spacer" />
        <input
          type="text"
          placeholder="Filter…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          style={{ width: 220 }}
        />
        <button onClick={() => props.onSetAll(true)}>All</button>
        <button onClick={() => props.onSetAll(false)}>None</button>
      </div>

      {props.compat && props.compat.conflicts.length > 0 && (
        <div className="banner warn">
          {props.compat.conflicts.map((c, i) => <div key={i}>! {c}</div>)}
        </div>
      )}
      {props.compat?.suggestedLoader && (
        <div className="banner info">
          Suggested target: <b>{props.compat.suggestedLoader}</b> for Minecraft{" "}
          <b>{props.compat.suggestedMcVersion ?? "?"}</b>
          {props.compat.commonGameVersions.length > 0 &&
            ` — common versions: ${props.compat.commonGameVersions.slice(0, 8).join(", ")}`}
          {matchCount < props.mods.length &&
            ` · ${props.mods.length - matchCount} jar(s) not on Modrinth (will bundle as raw files)`}
        </div>
      )}

      <table>
        <thead>
          <tr>
            <th style={{ width: 30 }}></th>
            <th>Mod</th>
            <th>File</th>
            <th style={{ width: 80 }}>Size</th>
            <th style={{ width: 110 }}>Loader</th>
            <th style={{ width: 140 }}>MC versions</th>
            <th style={{ width: 90 }}>Modrinth</th>
            <th style={{ width: 30 }}></th>
          </tr>
        </thead>
        <tbody>
          {filtered.map((m) => {
            const r = props.resolved[m.sha1];
            const name = r?.projectTitle ?? m.name ?? m.modId ?? "—";
            const loaders = r?.loaders?.length ? r.loaders : m.loaders;
            const gvs = r?.gameVersions?.length ? r.gameVersions : m.gameVersions;
            return (
              <tr key={m.path}>
                <td>
                  <input
                    type="checkbox"
                    checked={props.selected.has(m.path)}
                    onChange={(e) => props.onToggle(m.path, e.target.checked)}
                  />
                </td>
                <td>
                  {name}
                  {m.version && <span className="dim"> {r?.versionNumber ?? m.version}</span>}
                </td>
                <td className="dim mono">{m.filename}</td>
                <td className="dim">{fmtSize(m.size)}</td>
                <td>{loaders.length ? loaders.join(", ") : <span className="dim">?</span>}</td>
                <td className="dim">
                  {gvs.length ? gvs.slice(0, 4).join(", ") + (gvs.length > 4 ? "…" : "") : "?"}
                </td>
                <td>
                  {m.parseError ? (
                    <span className="badge err">parse err</span>
                  ) : r?.projectId ? (
                    <span className="badge ok">matched</span>
                  ) : r ? (
                    <span className="badge miss">not found</span>
                  ) : (
                    <span className="badge neutral">…</span>
                  )}
                </td>
                <td>
                  <button
                    className="row-del"
                    title="Remove from list"
                    onClick={() => props.onRemove(m.path)}
                  >
                    ×
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

import { useCallback, useEffect, useRef, useState } from "react";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import {
  appHidden,
  applyUpdate,
  cancelApply,
  connectNetworkInfo,
  connectStatus,
  declineUpdate,
  detectLaunchers,
  hostKick,
  hostPublish,
  hostSetFolder,
  hostStart,
  hostStop,
  hostUpdate,
  joinNetwork,
  leaveNetwork,
  listSyncedPacks,
  loadHostPack,
  loadSyncedPack,
  packExists,
  scanNetworks,
  suggestPackMeta,
  tailLog,
  uiLog,
} from "../lib/api";
import type {
  ApplyProgress,
  ConnectStatus,
  FoundNetwork,
  LauncherInfo,
  PackSettings,
  SyncedPackInfo,
} from "../lib/api";

interface Props {
  settings: PackSettings;
}

const savedName = () => localStorage.getItem("mm-display-name") ?? "";
const ts = (t: number) => new Date(t * 1000).toLocaleString();

/** "3m ago" for stale found-network entries. */
const ago = (t: number) => {
  const s = Math.max(0, Math.floor(Date.now() / 1000) - t);
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
};

export default function ConnectView(_props: Props) {
  const [status, setStatus] = useState<ConnectStatus | null>(null);
  const [found, setFound] = useState<FoundNetwork[]>([]);
  const [log, setLog] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  // host form
  const [netName, setNetName] = useState("");
  const [netCode, setNetCode] = useState("");
  const [netReqCode, setNetReqCode] = useState(true);
  const [netFolder, setNetFolder] = useState("");
  // Publish dialog — pack meta is confirmed HERE, at publish time, not at
  // host setup: a mods folder isn't a pack until you publish it.
  const [pubOpen, setPubOpen] = useState(false);
  const [pubStale, setPubStale] = useState(false);
  const [pubMeta, setPubMeta] = useState({
    name: "",
    ver: "",
    mc: "",
    loader: "",
    lv: "",
  });
  const [note, setNote] = useState("");
  const [pubTarget, setPubTarget] = useState<string | "all">("all");
  // edit-network form (hosting view)
  const [editing, setEditing] = useState(false);
  const [editName, setEditName] = useState("");
  const [editCode, setEditCode] = useState("");
  const [editReqCode, setEditReqCode] = useState(true);
  const [editMeta, setEditMeta] = useState({ name: "", ver: "", mc: "", loader: "", lv: "" });
  // join form
  const [joinAddr, setJoinAddr] = useState("");
  const [joinCode, setJoinCode] = useState("");
  // null = unknown (manual addr / old host) → show the code field;
  // false = announced open → hide it entirely.
  const [joinNeedsCode, setJoinNeedsCode] = useState<boolean | null>(null);
  const [displayName, setDisplayName] = useState(savedName());
  // post-apply: launcher picker + synced pack meta
  const [launchers, setLaunchers] = useState<LauncherInfo[]>([]);
  const [syncedPack, setSyncedPack] = useState<SyncedPackInfo | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  // The "?" guide — member-only, teaches the joined flow step by step.
  const [guideOpen, setGuideOpen] = useState(false);
  // Which detected launcher already holds the pack — powers the
  // persistent "installed ✓" state and the guide's last step.
  const [installedIn, setInstalledIn] = useState<string | null>(null);
  // live download progress while apply_update streams files
  const [applyProg, setApplyProg] = useState<ApplyProgress | null>(null);
  // Byte samples for the download speed/ETA readout.
  const speedSamples = useRef<{ t: number; b: number }[]>([]);
  const timer = useRef<number | null>(null);
  // Mirror of `busy` for the interval callback — while apply/publish runs,
  // connectStatus polls would re-hash every synced file (diff_manifest)
  // and compete with the download.
  const busyRef = useRef(false);
  useEffect(() => {
    busyRef.current = busy;
  }, [busy]);

  const pushLog = (line: string) =>
    setLog((l) => [...l, ...line.split("\n").filter(Boolean)]);

  // Only the freshest refresh may write state — a poll already in flight
  // when the user joins/leaves/applies would otherwise land after the
  // action's own refresh and snap the view back for a whole interval.
  const refreshSeq = useRef(0);
  const refresh = useCallback(async () => {
    const seq = ++refreshSeq.current;
    try {
      const s = await connectStatus();
      // Reported once per kick — log even if this response is stale.
      if (s.kicked) {
        pushLog("x you were kicked from the network — rejoin with the code");
      }
      if (seq !== refreshSeq.current) return;
      setStatus(s);
    } catch (e) {
      if (seq === refreshSeq.current) pushLog(`x status: ${e}`);
    }
    try {
      const f = await scanNetworks();
      if (seq === refreshSeq.current) setFound(f);
    } catch {
      /* discovery listener may be off */
    }
  }, []);

  useEffect(() => {
    refresh();
    timer.current = window.setInterval(() => {
      if (!busyRef.current && !appHidden.current) refresh();
    }, 4000);
    // A network announce (new network, version bump, member change)
    // refreshes instantly — found networks and pending updates appear
    // the moment the host publishes, not on the next poll.
    const un = listen("networks-found", refresh);
    const un2 = listen<ApplyProgress>("connect-apply-progress", (e) =>
      setApplyProg(e.payload)
    );
    return () => {
      if (timer.current) window.clearInterval(timer.current);
      un.then((f) => f());
      un2.then((f) => f());
    };
  }, [refresh]);

  // Once a pack is applied, fetch the launcher list + pack meta so the
  // member can load it straight into their launcher from this tab —
  // and keep them fresh so a newly installed launcher shows up live.
  const applied = status?.appliedVersion ?? 0;
  const joined = status?.mode === "joined";
  const hosting = status?.mode === "hosting";
  const nid = status?.networkId ?? null;
  useEffect(() => {
    if (!(joined || hosting) || applied === 0) return;
    const load = () => {
      (async () => {
        // Pack name for the installed-✓ probe: hosts use their own pack
        // name, members resolve it from the synced copy's pack.json.
        const name = hosting
          ? status?.packName ?? "Pack"
          : await listSyncedPacks().then((packs) => {
              const p = packs.find((p) => p.networkId === nid) ?? null;
              setSyncedPack(p);
              return p?.name || status?.networkName || "Synced Pack";
            });
        const d = await detectLaunchers();
        setLaunchers(d.launchers);
        // Which launcher (if any) already holds this pack — drives the
        // persistent "installed ✓" state + the guide's done step.
        const hits = await Promise.all(
          d.launchers.map(async (l) => ((await packExists(l, name)) ? l.name : null))
        );
        setInstalledIn(hits.find((x) => x) ?? null);
      })().catch((e) => pushLog(`x launcher detection: ${e}`));
    };
    load();
    const t = window.setInterval(() => {
      if (!appHidden.current) load();
    }, 5000);
    return () => window.clearInterval(t);
  }, [joined, hosting, applied, nid]);

  const loadSyncedInto = async (l: LauncherInfo) => {
    const name = hosting
      ? status?.packName ?? "Pack"
      : syncedPack?.name || status?.networkName || "Synced Pack";
    if (!hosting && !nid) return;
    if (
      (await packExists(l, name)) &&
      !(await confirm(
        `A pack named "${name}" already exists in ${l.name}.\nOverwrite it?`,
        { title: "Pack exists", kind: "warning", okLabel: "Overwrite", cancelLabel: "Cancel" }
      ))
    ) {
      return;
    }
    setBusy(true);
    pushLog(`> ${l.name}: installing "${name}" v${applied}…`);
    try {
      // Hosts deploy their live folder; members deploy the synced copy —
      // the backend funnels both through the same code.
      pushLog(hosting ? await loadHostPack(l) : await loadSyncedPack(nid!, l));
      setInstalledIn(l.name); // instant "go play" — poll confirms later
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const pickFolder = async () => {
    const dir = await open({ directory: true, title: "Pick the mods folder to share" });
    if (typeof dir !== "string") return;
    setNetFolder(dir);
  };

  const doHost = async () => {
    setBusy(true);
    try {
      // Meta args left blank — the backend auto-detects them from the jars,
      // and the publish dialog is where they get confirmed anyway.
      pushLog(
        await hostStart(
          netName || "My Network",
          netReqCode ? netCode : "",
          netFolder,
          "",
          "",
          "",
          ""
        )
      );
      await refresh();
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

  // The publish dialog prefills from the last confirmed meta and opens
  // instantly. When meta is incomplete (fresh/resumed host) or the folder
  // moved underneath it (metaFolder ≠ folder), a jar scan re-suggests in
  // the background and fills only fields that are still empty.
  const openPublish = () => {
    uiLog("ui", `publish click (mode=${status?.mode} folder=${status?.folder ?? "null"})`);
    if (!status) return;
    const stale = !!status.metaFolder && !!status.folder && status.metaFolder !== status.folder;
    const meta = {
      name: status.packName || "",
      // A folder swap invalidates the old pack's version string too.
      ver: stale ? "" : status.packVersion || "",
      mc: status.mcVersion || "",
      loader: status.loader || "",
      lv: status.loaderVersion || "",
    };
    const incomplete = !meta.name.trim() || !meta.mc.trim() || !meta.loader;
    setPubMeta(meta);
    setPubStale(stale);
    setPubOpen(true);
    if (status.folder && (stale || incomplete || !status.metaFolder)) {
      const folder = status.folder;
      // `unconfirmed` = meta never saved for this folder (old connect.json):
      // every value is a default, so the scan replaces — not just fills.
      const unconfirmed = !status.metaFolder;
      suggestPackMeta(folder)
        .then((s) =>
          setPubMeta((m) => ({
            name: unconfirmed || !m.name ? (s.packName || m.name) : m.name,
            ver: m.ver,
            mc: unconfirmed || !m.mc ? (s.mcVersion || m.mc) : m.mc,
            loader: unconfirmed || !m.loader ? (s.loader || m.loader) : m.loader,
            lv: unconfirmed || !m.lv ? (s.loaderVersion || m.lv) : m.lv,
          }))
        )
        .catch(() => {});
    }
  };

  const confirmPublish = async () => {
    const { name, mc, loader } = pubMeta;
    uiLog("ui", `publish confirm (name=${name} mc=${mc} loader=${loader})`);
    if (!name.trim() || !mc.trim() || !loader.trim()) {
      pushLog("x pack name, MC version and loader are required to publish");
      return;
    }
    setPubOpen(false);
    await doPublish(false, false);
  };

  const doPublish = async (force = false, allowEmpty = false) => {
    setBusy(true);
    try {
      pushLog(
        await hostPublish(
          note,
          pubTarget === "all" ? null : pubTarget,
          allowEmpty,
          force,
          {
            packName: pubMeta.name,
            packVersion: pubMeta.ver,
            mcVersion: pubMeta.mc,
            loader: pubMeta.loader,
            loaderVersion: pubMeta.lv,
          }
        )
      );
      setNote("");
      setPubTarget("all");
      await refresh();
    } catch (e) {
      const msg = String(e);
      if (msg.includes("UNCHANGED")) {
        setBusy(false);
        uiLog("ui", "UNCHANGED — showing republish confirm");
        const ok = await confirm(
          "Nothing changed since the last publish — same files, same hashes.\n\nRepublish anyway? Members can re-apply to repair a broken copy.",
          { title: "Republish unchanged", okLabel: "Republish", cancelLabel: "Cancel" }
        );
        if (ok) await doPublish(true, allowEmpty);
        return;
      }
      if (msg.includes("EMPTY_PACK")) {
        setBusy(false);
        const ok = await confirm(
          "The mods folder is empty — publishing would remove every mod on every member.\n\nPublish an empty pack anyway?",
          { title: "Empty pack", okLabel: "Publish anyway", cancelLabel: "Cancel" }
        );
        if (ok) await doPublish(force, true);
        return;
      }
      pushLog(`x ${msg}`);
    } finally {
      setBusy(false);
    }
  };

  const doChangeFolder = async () => {
    const dir = await open({
      directory: true,
      title: "Pick the new mods folder",
    });
    if (typeof dir !== "string") return;
    setBusy(true);
    try {
      const r = await hostSetFolder(dir);
      pushLog(`✓ folder changed — ${r.fileCount} file(s) found`);
      if (r.samePct < 50) {
        pushLog(
          `! only ${r.samePct}% of files match the current pack — the next publish will replace most of it`
        );
      }
      await refresh();
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const doJoin = async (addr: string) => {
    if (!displayName.trim()) {
      pushLog("x set a display name first");
      return;
    }
    localStorage.setItem("mm-display-name", displayName.trim());
    setBusy(true);
    try {
      // Open networks never ask for a code. `null` means the announce
      // didn't say (old host) or it's a manual IP — ask /info to be sure.
      let code = joinCode;
      let needs = joinNeedsCode;
      if (needs === null) {
        try {
          const info = await connectNetworkInfo(addr);
          needs = info.requiresCode !== false;
        } catch {
          needs = true; // unreachable → let the join error surface it
        }
      }
      if (needs === false) code = "";
      pushLog(await joinNetwork(addr, code, displayName.trim()));
      setJoinAddr("");
      setJoinCode("");
      await refresh();
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const doSaveEdit = async () => {
    setBusy(true);
    try {
      pushLog(
        await hostUpdate(
          editName,
          editReqCode ? editCode : "",
          editMeta.name,
          editMeta.ver,
          editMeta.mc,
          editMeta.loader,
          editMeta.lv
        )
      );
      setEditing(false);
      await refresh();
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const doKick = async (id: string, name: string) => {
    if (
      !(await confirm(`Kick ${name} from the network?\nThey can rejoin with the join code.`, {
        title: "Kick member",
        okLabel: "Kick",
        cancelLabel: "Cancel",
      }))
    ) {
      return;
    }
    setBusy(true);
    try {
      pushLog(await hostKick(id));
      await refresh();
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const doCancelApply = async () => {
    try {
      await cancelApply();
      pushLog("! cancel requested — finishing current chunk");
    } catch (e) {
      pushLog(`x ${e}`);
    }
  };

  const doCopyLogs = async () => {
    try {
      const lines = await tailLog(200);
      await navigator.clipboard.writeText(lines.join("\n"));
      pushLog("✓ last 200 log lines copied to clipboard");
    } catch (e) {
      pushLog(`x logs: ${e}`);
    }
  };

  const doApply = async () => {
    setBusy(true);
    setApplyProg(null);
    speedSamples.current = [];
    try {
      pushLog(await applyUpdate());
      await refresh();
      // The apply finished — the launcher picker is the expected next
      // step, so open it instead of making them scroll to find it.
      setPickerOpen(true);
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setApplyProg(null);
      setBusy(false);
    }
  };

  // Download speed + ETA from the byte-progress samples.
  const speedEta = (() => {
    if (!applyProg || applyProg.phase !== "downloading") return "";
    const now = Date.now();
    const samples = speedSamples.current;
    if (samples.length === 0 || now - samples[samples.length - 1].t > 250) {
      samples.push({ t: now, b: applyProg.doneBytes });
      if (samples.length > 12) samples.shift();
    }
    if (samples.length < 2) return "";
    const dt = (now - samples[0].t) / 1000;
    if (dt <= 0) return "";
    const bps = (applyProg.doneBytes - samples[0].b) / dt;
    if (bps <= 0) return "";
    const rem = applyProg.totalBytes - applyProg.doneBytes;
    const eta = rem > 0 ? Math.ceil(rem / bps) : 0;
    const mbps = (bps / 1048576).toFixed(1);
    return ` · ${mbps} MB/s` + (eta > 0 ? ` · ~${eta}s left` : "");
  })();

  const doDecline = async (version: number) => {
    setBusy(true);
    try {
      await declineUpdate(version);
      pushLog(`✓ declined v${version} — it won't show again`);
      await refresh();
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const membersTable = (canKick: boolean) =>
    status && status.members.length > 0 && (
      <table>
        <thead>
          <tr>
            <th>Member</th>
            <th style={{ width: 80 }}>Status</th>
            <th style={{ width: 120 }}>Version</th>
            {canKick && <th style={{ width: 30 }}></th>}
          </tr>
        </thead>
        <tbody>
          {status.members.map((m) => (
            <tr key={m.id}>
              <td>
                {m.host && (
                  <span className="owner" title="Network owner">♛ </span>
                )}
                {m.name}
              </td>
              <td>
                <span className={`badge ${m.online ? "ok" : "miss"}`}>
                  {m.online ? "online" : "offline"}
                </span>
              </td>
              <td>
                {m.upToDate ? (
                  <span className="badge ok">v{m.appliedVersion} ✓</span>
                ) : (
                  <span className="badge miss">v{m.appliedVersion} behind</span>
                )}
                {m.targeted && (
                  <span className="badge" title="An update was published only for this member">
                    targeted
                  </span>
                )}
                {m.appVersion && (
                  <span className="badge" title="App version this member is running">
                    app v{m.appVersion}
                  </span>
                )}
              </td>
              {canKick && (
                <td>
                  {!m.host && (
                    <button
                      className="row-del"
                      title={`Kick ${m.name} — blocked until they rejoin with the code`}
                      onClick={() => doKick(m.id, m.name)}
                    >
                      ×
                    </button>
                  )}
                </td>
              )}
            </tr>
          ))}
        </tbody>
      </table>
    );

  // ---- hosting view ----
  if (status?.mode === "hosting") {
    return (
      <div className="view">
        <div className="panel">
          <h3>
            Hosting: {status.networkName}{" "}
            <span className="dim">
              — {status.latestVersion === 0 ? "nothing published yet" : `latest: v${status.latestVersion}`}
            </span>
          </h3>
          <div className="dim" style={{ marginBottom: 10 }}>
            folder: <span className="mono">{status.folder}</span>{" "}
            <button onClick={doChangeFolder} disabled={busy}>
              Change folder…
            </button>
            {status.code ? ` · join code: ${status.code}` : " · open — no join code"}
            {" · "}
            <button
              onClick={() => {
                setEditName(status.networkName ?? "");
                setEditCode(status.code ?? "");
                setEditReqCode(!!status.code);
                setEditing(!editing);
              }}
              disabled={busy}
            >
              Edit network…
            </button>
          </div>
          {editing && (
            <div className="form-grid" style={{ marginBottom: 10 }}>
              <label>Network name</label>
              <input value={editName} onChange={(e) => setEditName(e.target.value)} />
              <label>Join code</label>
              <div className="toolbar" style={{ gap: 8 }}>
                <label style={{ display: "flex", gap: 6, alignItems: "center" }}>
                  <input
                    type="checkbox"
                    checked={editReqCode}
                    onChange={(e) => setEditReqCode(e.target.checked)}
                  />
                  require join code
                </label>
                <input
                  value={editCode}
                  onChange={(e) => setEditCode(e.target.value)}
                  disabled={!editReqCode}
                  placeholder="code for new joins"
                  style={{ flex: 1 }}
                />
              </div>
              <label>Pack name</label>
              <input
                value={editMeta.name}
                onChange={(e) => setEditMeta((m) => ({ ...m, name: e.target.value }))}
                placeholder="keep current"
              />
              <label>Pack version</label>
              <input
                value={editMeta.ver}
                onChange={(e) => setEditMeta((m) => ({ ...m, ver: e.target.value }))}
                placeholder="keep current"
              />
              <label>MC version</label>
              <input
                value={editMeta.mc}
                onChange={(e) => setEditMeta((m) => ({ ...m, mc: e.target.value }))}
                placeholder="keep current"
              />
              <label>Loader</label>
              <select
                value={editMeta.loader}
                onChange={(e) => setEditMeta((m) => ({ ...m, loader: e.target.value }))}
              >
                <option value="">keep current</option>
                <option value="fabric">fabric</option>
                <option value="forge">forge</option>
                <option value="neoforge">neoforge</option>
                <option value="quilt">quilt</option>
              </select>
              <label>Loader version</label>
              <input
                value={editMeta.lv}
                onChange={(e) => setEditMeta((m) => ({ ...m, lv: e.target.value }))}
                placeholder="keep current"
              />
              <label></label>
              <div className="toolbar" style={{ gap: 6 }}>
                <button className="primary" onClick={doSaveEdit} disabled={busy}>
                  Save
                </button>
                <button onClick={() => setEditing(false)}>Cancel</button>
                <span className="dim">
                  affects new joins only — members already in stay joined
                </span>
              </div>
            </div>
          )}
          <div className="toolbar">
            <select
              value={pubTarget}
              onChange={(e) => setPubTarget(e.target.value)}
            >
              <option value="all">Publish to everyone</option>
              {status.members
                .filter((m) => !m.host)
                .map((m) => (
                  <option key={m.id} value={m.id}>
                    Only: {m.name}
                  </option>
                ))}
            </select>
            <button className="primary" onClick={openPublish} disabled={busy}>
              Publish update…
            </button>
            <button onClick={async () => { await hostStop(); await refresh(); }}>
              Stop hosting
            </button>
          </div>
          {status.latestVersion === 0 && (
            <div className="banner info" style={{ marginTop: 10 }}>
              Nothing published yet — press <b>Publish update</b> to send your
              first modpack version. Members won't see anything to download
              until you do.
            </div>
          )}
          {status.history.length > 0 && (
            <div className="dim" style={{ marginTop: 8 }}>
              {status.history.slice(0, 5).map((h) => (
                <div key={h.version}>
                  v{h.version}{h.note ? ` — ${h.note}` : ""} · {ts(h.ts)}
                </div>
              ))}
            </div>
          )}
        </div>
        <div className="panel">
          <h3>Members</h3>
          {membersTable(true) ?? <div className="dim">Nobody's joined yet.</div>}
        </div>
        {status.latestVersion > 0 && (
          <div className="panel">
            <h3>Load into Minecraft</h3>
            {launchers.length === 0 ? (
              <div className="dim">
                No launcher detected — install Minecraft, TLauncher or
                Prism, or use the Loader tab.
              </div>
            ) : (
              <>
                <div className="dim" style={{ marginBottom: 8 }}>
                  Want to play too? Installs "{status.packName}" (v
                  {status.latestVersion}) from your shared folder — same
                  pack your members get:
                </div>
                {launchers.map((l) => (
                  <button
                    key={l.kind}
                    className="primary"
                    style={{ marginRight: 8, marginBottom: 6 }}
                    disabled={busy}
                    onClick={() => loadSyncedInto(l)}
                  >
                    {l.name}
                    {installedIn === l.name ? " ✓" : ""}
                  </button>
                ))}
              </>
            )}
          </div>
        )}
        {log.length > 0 && (
          <div className="log-box">
            {log.map((l, i) => (
              <div key={i} className={l.startsWith("x") || l.startsWith("!") ? "err" : "ok"}>{l}</div>
            ))}
          </div>
        )}
        {pubOpen && (
          <div className="upd-overlay" onClick={() => setPubOpen(false)}>
            <div className="upd-card" onClick={(e) => e.stopPropagation()} style={{ minWidth: 340 }}>
              <h3>Publish pack update</h3>
              {pubStale && (
                <p className="upd-warn">
                  The shared folder changed since this pack meta was saved —
                  fields were re-detected from the new jars. Check them.
                </p>
              )}
              <div className="form-grid" style={{ marginTop: 10 }}>
                <label>Pack name</label>
                <input
                  value={pubMeta.name}
                  onChange={(e) => setPubMeta((m) => ({ ...m, name: e.target.value }))}
                  placeholder="My Modpack"
                  autoFocus
                />
                <label>Pack version</label>
                <input
                  value={pubMeta.ver}
                  onChange={(e) => setPubMeta((m) => ({ ...m, ver: e.target.value }))}
                  placeholder="e.g. 1.0 / beta 3 (optional)"
                />
                <label>MC version *</label>
                <input
                  value={pubMeta.mc}
                  onChange={(e) => setPubMeta((m) => ({ ...m, mc: e.target.value }))}
                  placeholder="e.g. 1.21.1"
                />
                <label>Loader *</label>
                <select
                  value={pubMeta.loader}
                  onChange={(e) => setPubMeta((m) => ({ ...m, loader: e.target.value }))}
                >
                  <option value="">pick one…</option>
                  <option value="fabric">fabric</option>
                  <option value="forge">forge</option>
                  <option value="neoforge">neoforge</option>
                  <option value="quilt">quilt</option>
                </select>
                <label>Loader version</label>
                <input
                  value={pubMeta.lv}
                  onChange={(e) => setPubMeta((m) => ({ ...m, lv: e.target.value }))}
                  placeholder="blank = latest"
                />
                <label>Update note</label>
                <input
                  value={note}
                  onChange={(e) => setNote(e.target.value)}
                  placeholder="optional"
                />
              </div>
              <div className="upd-actions">
                <button
                  className="primary"
                  onClick={confirmPublish}
                  disabled={busy}
                >
                  Publish
                </button>
                <button onClick={() => setPubOpen(false)}>Cancel</button>
                {(!pubMeta.name.trim() || !pubMeta.mc.trim() || !pubMeta.loader) && (
                  <span className="dim">pack name, MC version and loader are required</span>
                )}
              </div>
            </div>
          </div>
        )}
      </div>
    );
  }

  // ---- joined view ----
  if (status?.mode === "joined") {
    const packName =
      syncedPack?.name || status.packName || status.networkName || "the pack";
    const downloading =
      !!applyProg &&
      applyProg.phase !== "done" &&
      applyProg.phase !== "cancelled";
    // Live guide steps — each reads current status, so the panel checks
    // off what the member already did and points at what's next.
    const guideSteps: { icon: string; text: string; cls: string }[] = [
      {
        icon: "✓",
        text: `Connected to "${status.networkName}" as ${status.displayName}`,
        cls: "done",
      },
    ];
    if (downloading) {
      guideSteps.push({
        icon: "→",
        text: "Downloading the pack — wait for it to finish",
        cls: "now",
      });
    } else if (!status.hostOnline) {
      guideSteps.push({
        icon: "•",
        text: `Wait for the host — the "host offline" badge above means their app isn't reachable right now`,
        cls: "wait",
      });
    } else if (status.pending) {
      guideSteps.push({
        icon: "→",
        text: `Press "Apply update" above — downloads "${status.pending.packName || packName}" v${status.pending.version}`,
        cls: "now",
      });
    } else if (status.appliedVersion > 0) {
      guideSteps.push({
        icon: "✓",
        text: `Modpack downloaded — you're on v${status.appliedVersion}`,
        cls: "done",
      });
    } else if (status.latestVersion > 0) {
      // published but not offered — the member declined this version (or
      // a targeted push went to someone else); nothing to click here.
      guideSteps.push({
        icon: "•",
        text: `v${status.latestVersion} exists but isn't offered to you — you may have declined it; ask the host to re-push or wait for the next update`,
        cls: "wait",
      });
    } else {
      guideSteps.push({
        icon: "•",
        text: `No pack published yet — ask the host to press "Publish update…" on their side`,
        cls: "wait",
      });
    }
    if (installedIn) {
      guideSteps.push({
        icon: "✓",
        text: `Installed into ${installedIn}`,
        cls: "done",
      });
    } else if (status.appliedVersion > 0 && launchers.length === 0) {
      guideSteps.push({
        icon: "•",
        text: `No launcher detected — install TLauncher or Prism first (the list below re-scans automatically)`,
        cls: "wait",
      });
    } else if (status.appliedVersion > 0) {
      guideSteps.push({
        icon: "→",
        text: `Install it — pick your launcher under "Load into Minecraft" below (a popup also appears right after a download)`,
        cls: "now",
      });
    } else {
      guideSteps.push({
        icon: "•",
        text: "Install into your launcher — this step unlocks after the download",
        cls: "wait",
      });
    }
    guideSteps.push(
      installedIn
        ? {
            icon: "→",
            text: `Play — open ${installedIn} and run "${packName}"`,
            cls: "now",
          }
        : { icon: "•", text: "Open your launcher and play", cls: "wait" }
    );
    return (
      <div className="view">
        <div className="panel">
          <h3>
            {status.networkName}{" "}
            <span className={`badge ${status.hostOnline ? "ok" : "err"}`}>
              host {status.hostOnline ? "online" : "offline"}
            </span>{" "}
            <button
              className="guide-btn"
              title="How does this work?"
              onClick={() => {
                uiLog("ui", "guide open (joined)");
                setGuideOpen(true);
              }}
            >
              ?
            </button>
          </h3>
          <div className="dim" style={{ marginBottom: 10 }}>
            {status.hostAddr} · you're <b>{status.displayName}</b> · applied: v{status.appliedVersion}
          </div>
          {installedIn && (
            <div className="banner ok" style={{ marginBottom: 10 }}>
              ✓ Installed to {installedIn} — open it and run "{packName}"
            </div>
          )}
          {status.pending ? (
            <div className="banner info">
              <div>
                Update available: <b>v{status.pending.version}</b>
                {status.pending.note ? ` — ${status.pending.note}` : ""}
              </div>
              <div className="dim">
                {status.pending.fileCount} file(s) · {status.pending.toDownload} to download
                {status.pending.toDelete ? ` · ${status.pending.toDelete} to remove` : ""}
              </div>
              {applyProg ? (
                <div style={{ marginTop: 8 }}>
                  {applyProg.phase === "hashing" ? (
                    <div className="dim">
                      checking local files {applyProg.hashingDone}/
                      {applyProg.hashingTotal}
                      {applyProg.hashingFile
                        ? ` — ${applyProg.hashingFile}`
                        : "…"}
                    </div>
                  ) : (
                    <>
                      <div className="dim">
                        {applyProg.phase === "cancelled"
                          ? "cancelling…"
                          : `downloading ${applyProg.doneFiles}/${applyProg.totalFiles} files`}
                        {applyProg.totalBytes > 0 &&
                          ` · ${(applyProg.doneBytes / 1048576).toFixed(1)}/${(applyProg.totalBytes / 1048576).toFixed(1)} MB · ${Math.round((applyProg.doneBytes / applyProg.totalBytes) * 100)}%`}
                        {speedEta}
                      </div>
                      {applyProg.totalBytes > 0 && (
                        <div className="upd-prog" style={{ flexBasis: "100%" }}>
                          <div
                            style={{
                              width: `${Math.min(100, (applyProg.doneBytes / applyProg.totalBytes) * 100)}%`,
                            }}
                          />
                        </div>
                      )}
                      {applyProg.files
                        .filter((f) => f.done < f.total)
                        .slice(0, 4)
                        .map((f) => (
                          <div className="dim mono file-row" key={f.name}>
                            <span className="file-name">{f.name}</span>
                            <span>
                              {f.total > 0
                                ? `${Math.round((f.done / f.total) * 100)}%`
                                : "…"}
                              {f.retries > 0 && ` · retry ${f.retries}`}
                            </span>
                          </div>
                        ))}
                      {applyProg.failed.length > 0 && (
                        <div className="err">
                          failed: {applyProg.failed.join(", ")}
                        </div>
                      )}
                      <div style={{ marginTop: 6 }}>
                        <button onClick={doCancelApply} disabled={!busy}>
                          Cancel — keep progress for later
                        </button>
                      </div>
                    </>
                  )}
                </div>
              ) : (
                <div style={{ marginTop: 8 }}>
                  <button className="primary" onClick={doApply} disabled={busy}>
                    {busy ? "Applying…" : "Apply update"}
                  </button>{" "}
                  <button onClick={() => doDecline(status.pending!.version)} disabled={busy}>
                    Decline
                  </button>
                </div>
              )}
            </div>
          ) : (
            <div className="dim">
              {status.hostOnline
                ? "You're up to date — nothing pending."
                : "Host unreachable — updates appear when they're back."}
            </div>
          )}
          <div className="toolbar" style={{ marginTop: 10 }}>
            <button onClick={refresh} disabled={busy}>Check for updates</button>
            <button onClick={async () => { await leaveNetwork(); await refresh(); }}>
              Leave network
            </button>
          </div>
        </div>
        {status.appliedVersion > 0 && (
          <div className="panel">
            <h3>Load into Minecraft</h3>
            {launchers.length === 0 ? (
              <div className="dim">
                Pack is synced — but no launcher was detected. Install
                Minecraft, TLauncher or Prism, or use the Loader tab.
              </div>
            ) : (
              <>
                <div className="dim" style={{ marginBottom: 8 }}>
                  Pack synced (v{status.appliedVersion}) — pick a launcher to
                  install it into:
                </div>
                {launchers.map((l) => (
                  <button
                    key={l.kind}
                    className="primary"
                    style={{ marginRight: 8, marginBottom: 6 }}
                    disabled={busy}
                    onClick={() => loadSyncedInto(l)}
                  >
                    {l.name}
                    {installedIn === l.name ? " ✓" : ""}
                  </button>
                ))}
              </>
            )}
          </div>
        )}
        <div className="panel">
          <h3>Members</h3>
          {membersTable(false) ?? <div className="dim">No member data yet.</div>}
        </div>
        {log.length > 0 && (
          <div className="log-box">
            <div className="log-tools">
              <button onClick={doCopyLogs}>copy logs</button>
            </div>
            {log.map((l, i) => (
              <div key={i} className={l.startsWith("x") || l.startsWith("!") ? "err" : "ok"}>{l}</div>
            ))}
          </div>
        )}
        {pickerOpen && (
          <div className="upd-overlay" onClick={() => setPickerOpen(false)}>
            <div className="upd-card" onClick={(e) => e.stopPropagation()}>
              <h3>Pack synced — install it</h3>
              {launchers.length === 0 ? (
                <p className="dim">
                  No launcher was detected — use the Loader tab, or install
                  Minecraft / TLauncher / Prism.
                </p>
              ) : (
                <>
                  <p className="dim">
                    Pick a launcher to install v{status.appliedVersion} into:
                  </p>
                  <div className="upd-actions">
                    {launchers.map((l) => (
                      <button
                        key={l.kind}
                        className="primary"
                        onClick={() => {
                          setPickerOpen(false);
                          loadSyncedInto(l);
                        }}
                      >
                        {l.name}
                        {installedIn === l.name ? " ✓" : ""}
                      </button>
                    ))}
                  </div>
                </>
              )}
              <div className="upd-actions">
                <button onClick={() => setPickerOpen(false)}>Later</button>
              </div>
            </div>
          </div>
        )}
        {guideOpen && (
          <div className="upd-overlay" onClick={() => setGuideOpen(false)}>
            <div className="upd-card" onClick={(e) => e.stopPropagation()}>
              <h3>Getting your modpack — step by step</h3>
              {guideSteps.map((s, i) => (
                <div key={i} className={`guide-row ${s.cls}`}>
                  <span className="guide-ico">{s.icon}</span>
                  <span>{s.text}</span>
                </div>
              ))}
              <p className="dim" style={{ marginTop: 10 }}>
                When the host publishes something new, "Apply update" appears
                again — click it and you're current. The app keeps running
                in the tray, so closing the window doesn't stop updates.
              </p>
              <div className="upd-actions">
                <button onClick={() => setGuideOpen(false)}>Close</button>
              </div>
            </div>
          </div>
        )}
      </div>
    );
  }

  // ---- idle view ----
  return (
    <div className="view">
      <div className="panel">
        <h3>Host a network</h3>
        <div className="form-grid">
          <label>Network name</label>
          <input value={netName} onChange={(e) => setNetName(e.target.value)} placeholder="e.g. smp-squad" />
          <label>Join code</label>
          <div className="toolbar" style={{ gap: 8 }}>
            <label style={{ display: "flex", gap: 6, alignItems: "center" }}>
              <input
                type="checkbox"
                checked={netReqCode}
                onChange={(e) => setNetReqCode(e.target.checked)}
              />
              require join code
            </label>
            <input
              value={netCode}
              onChange={(e) => setNetCode(e.target.value)}
              disabled={!netReqCode}
              placeholder="friends need this to join"
              style={{ flex: 1 }}
            />
          </div>
          <label>Mods folder</label>
          <div className="toolbar" style={{ gap: 6 }}>
            <button onClick={pickFolder}>Pick folder…</button>
            {netFolder && <span className="dim mono">{netFolder}</span>}
          </div>
        </div>
        <div className="dim" style={{ margin: "8px 0" }}>
          Pack details are asked at publish time — a mods folder isn't a
          modpack until you publish it.
        </div>
        <button className="primary" onClick={doHost} disabled={busy || !netFolder}>
          Start hosting
        </button>
      </div>

      <div className="panel">
        <h3>Found networks</h3>
        {found.length === 0 && <div className="dim">Scanning the LAN…</div>}
        {found.map((n) => (
          <div
            className="launcher-card"
            key={n.networkId}
            style={{ opacity: n.live ? 1 : 0.55 }}
          >
            <div style={{ fontSize: "1.4rem" }}>⇄</div>
            <div className="grow">
              <div className="name">
                {n.name}{" "}
                <span className="badge">
                  {n.requiresCode === false
                    ? "open"
                    : n.requiresCode === true
                      ? "locked"
                      : "locked?"}
                </span>{" "}
                {!n.live && <span className="badge miss">offline</span>}
              </div>
              <div className="sub">
                {n.machine} · pack "{n.packName}" ·{" "}
                {n.latestVersion === 0 ? "not published yet" : `v${n.latestVersion}`} · {n.members} member(s)
                {!n.live && ` · last seen ${ago(n.lastSeen)}`}
              </div>
            </div>
            <button
              onClick={() => {
                setJoinAddr(n.addr);
                setJoinNeedsCode(n.requiresCode ?? null);
              }}
            >
              Join
            </button>
          </div>
        ))}
        <div className="toolbar" style={{ marginTop: 10 }}>
          <input
            type="text"
            placeholder="…or host IP (e.g. 26.12.34.56)"
            value={joinAddr}
            onChange={(e) => {
              setJoinAddr(e.target.value);
              setJoinNeedsCode(null);
            }}
            style={{ flex: 1 }}
          />
        </div>
        {joinAddr && (
          <div className="toolbar" style={{ marginTop: 8 }}>
            <input
              type="text"
              placeholder="Your display name"
              value={displayName}
              onChange={(e) => setDisplayName(e.target.value)}
            />
            {joinNeedsCode !== false && (
              <input
                type="text"
                placeholder={
                  joinNeedsCode === null
                    ? "Join code (if required)"
                    : "Join code"
                }
                value={joinCode}
                onChange={(e) => setJoinCode(e.target.value)}
              />
            )}
            <button className="primary" onClick={() => doJoin(joinAddr)} disabled={busy}>
              Join
            </button>
          </div>
        )}
      </div>

      {log.length > 0 && (
        <div className="log-box">
          <div className="log-tools">
            <button onClick={doCopyLogs}>copy logs</button>
          </div>
          {log.map((l, i) => (
            <div key={i} className={l.startsWith("x") || l.startsWith("!") ? "err" : "ok"}>{l}</div>
          ))}
        </div>
      )}
    </div>
  );
}

import { useCallback, useEffect, useRef, useState } from "react";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import {
  applyUpdate,
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
  loadSyncedPack,
  packExists,
  scanNetworks,
} from "../lib/api";
import type {
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

export default function ConnectView(props: Props) {
  const [status, setStatus] = useState<ConnectStatus | null>(null);
  const [found, setFound] = useState<FoundNetwork[]>([]);
  const [log, setLog] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  // host form
  const [netName, setNetName] = useState("");
  const [netCode, setNetCode] = useState("");
  const [netReqCode, setNetReqCode] = useState(true);
  const [netFolder, setNetFolder] = useState("");
  const [note, setNote] = useState("");
  const [pubTarget, setPubTarget] = useState<number | "all">("all");
  // edit-network form (hosting view)
  const [editing, setEditing] = useState(false);
  const [editName, setEditName] = useState("");
  const [editCode, setEditCode] = useState("");
  const [editReqCode, setEditReqCode] = useState(true);
  // join form
  const [joinAddr, setJoinAddr] = useState("");
  const [joinCode, setJoinCode] = useState("");
  const [displayName, setDisplayName] = useState(savedName());
  // post-apply: launcher picker + synced pack meta
  const [launchers, setLaunchers] = useState<LauncherInfo[]>([]);
  const [syncedPack, setSyncedPack] = useState<SyncedPackInfo | null>(null);
  const timer = useRef<number | null>(null);

  const pushLog = (line: string) =>
    setLog((l) => [...l, ...line.split("\n").filter(Boolean)]);

  const refresh = useCallback(async () => {
    try {
      const s = await connectStatus();
      if (s.kicked) {
        pushLog("x you were kicked from the network — rejoin with the code");
      }
      setStatus(s);
    } catch (e) {
      pushLog(`x status: ${e}`);
    }
    try {
      setFound(await scanNetworks());
    } catch {
      /* discovery listener may be off */
    }
  }, []);

  useEffect(() => {
    refresh();
    timer.current = window.setInterval(refresh, 4000);
    // A network announce (new network, version bump, member change)
    // refreshes instantly — found networks and pending updates appear
    // the moment the host publishes, not on the next poll.
    const un = listen("networks-found", refresh);
    return () => {
      if (timer.current) window.clearInterval(timer.current);
      un.then((f) => f());
    };
  }, [refresh]);

  // Once a pack is applied, fetch the launcher list + pack meta so the
  // member can load it straight into their launcher from this tab —
  // and keep them fresh so a newly installed launcher shows up live.
  const applied = status?.appliedVersion ?? 0;
  const joined = status?.mode === "joined";
  const nid = status?.networkId ?? null;
  useEffect(() => {
    if (!joined || applied === 0) return;
    const load = () => {
      detectLaunchers()
        .then((d) => setLaunchers(d.launchers))
        .catch((e) => pushLog(`x launcher detection: ${e}`));
      listSyncedPacks()
        .then((packs) => setSyncedPack(packs.find((p) => p.networkId === nid) ?? null))
        .catch(() => {});
    };
    load();
    const t = window.setInterval(load, 5000);
    return () => window.clearInterval(t);
  }, [joined, applied, nid]);

  const loadSyncedInto = async (l: LauncherInfo) => {
    if (!nid) return;
    const name = syncedPack?.name || status?.networkName || "Synced Pack";
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
      pushLog(await loadSyncedPack(nid, l));
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const pickFolder = async () => {
    const dir = await open({ directory: true, title: "Pick the mods folder to share" });
    if (typeof dir === "string") setNetFolder(dir);
  };

  const doHost = async () => {
    setBusy(true);
    try {
      const s = props.settings;
      pushLog(
        await hostStart(
          netName || "My Network",
          netReqCode ? netCode : "",
          netFolder,
          s.name || netName || "My Modpack",
          s.mcVersion,
          s.loader,
          s.loaderVersion
        )
      );
      await refresh();
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const doPublish = async (allowEmpty = false) => {
    setBusy(true);
    try {
      pushLog(await hostPublish(note, pubTarget === "all" ? null : pubTarget, allowEmpty));
      setNote("");
      setPubTarget("all");
      await refresh();
    } catch (e) {
      const msg = String(e);
      if (msg.includes("EMPTY_PACK")) {
        const ok = await confirm(
          "The mods folder is empty — publishing would remove every mod on every member.\n\nPublish an empty pack anyway?",
          { title: "Empty pack", okLabel: "Publish anyway", cancelLabel: "Cancel" }
        );
        setBusy(false);
        if (ok) await doPublish(true);
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
      pushLog(await joinNetwork(addr, joinCode, displayName.trim()));
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
      pushLog(await hostUpdate(editName, editReqCode ? editCode : ""));
      setEditing(false);
      await refresh();
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const doKick = async (id: number, name: string) => {
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

  const doApply = async () => {
    setBusy(true);
    try {
      pushLog(await applyUpdate());
      await refresh();
    } catch (e) {
      pushLog(`x ${e}`);
    } finally {
      setBusy(false);
    }
  };

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
                      title={`Kick ${m.name}`}
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
            <input
              type="text"
              placeholder="Update note (optional)…"
              value={note}
              onChange={(e) => setNote(e.target.value)}
              style={{ flex: 1 }}
            />
            <select
              value={pubTarget}
              onChange={(e) =>
                setPubTarget(
                  e.target.value === "all" ? "all" : Number(e.target.value)
                )
              }
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
            <button className="primary" onClick={() => doPublish()} disabled={busy}>
              Publish update
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
        {log.length > 0 && (
          <div className="log-box">
            {log.map((l, i) => (
              <div key={i} className={l.startsWith("x") || l.startsWith("!") ? "err" : "ok"}>{l}</div>
            ))}
          </div>
        )}
      </div>
    );
  }

  // ---- joined view ----
  if (status?.mode === "joined") {
    return (
      <div className="view">
        <div className="panel">
          <h3>
            {status.networkName}{" "}
            <span className={`badge ${status.hostOnline ? "ok" : "err"}`}>
              host {status.hostOnline ? "online" : "offline"}
            </span>
          </h3>
          <div className="dim" style={{ marginBottom: 10 }}>
            {status.hostAddr} · you're <b>{status.displayName}</b> · applied: v{status.appliedVersion}
          </div>
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
              <div style={{ marginTop: 8 }}>
                <button className="primary" onClick={doApply} disabled={busy}>
                  Apply update
                </button>{" "}
                <button onClick={() => doDecline(status.pending!.version)} disabled={busy}>
                  Decline
                </button>
              </div>
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
            {log.map((l, i) => (
              <div key={i} className={l.startsWith("x") || l.startsWith("!") ? "err" : "ok"}>{l}</div>
            ))}
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
          Pack targets MC {props.settings.mcVersion || "?"} · {props.settings.loader}
          {props.settings.loaderVersion ? ` ${props.settings.loaderVersion}` : ""} — set in Build tab.
        </div>
        <button className="primary" onClick={doHost} disabled={busy || !netFolder}>
          Start hosting
        </button>
      </div>

      <div className="panel">
        <h3>Found networks</h3>
        {found.length === 0 && <div className="dim">Scanning the LAN…</div>}
        {found.map((n) => (
          <div className="launcher-card" key={n.networkId}>
            <div style={{ fontSize: "1.4rem" }}>⇄</div>
            <div className="grow">
              <div className="name">{n.name}</div>
              <div className="sub">
                {n.machine} · pack "{n.packName}" ·{" "}
                {n.latestVersion === 0 ? "not published yet" : `v${n.latestVersion}`} · {n.members} member(s)
              </div>
            </div>
            <button onClick={() => setJoinAddr(n.addr)}>Join</button>
          </div>
        ))}
        <div className="toolbar" style={{ marginTop: 10 }}>
          <input
            type="text"
            placeholder="…or host IP (e.g. 26.12.34.56)"
            value={joinAddr}
            onChange={(e) => setJoinAddr(e.target.value)}
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
            <input
              type="text"
              placeholder="Join code"
              value={joinCode}
              onChange={(e) => setJoinCode(e.target.value)}
            />
            <button className="primary" onClick={() => doJoin(joinAddr)} disabled={busy}>
              Join
            </button>
          </div>
        )}
      </div>

      {log.length > 0 && (
        <div className="log-box">
          {log.map((l, i) => (
            <div key={i} className={l.startsWith("x") || l.startsWith("!") ? "err" : "ok"}>{l}</div>
          ))}
        </div>
      )}
    </div>
  );
}

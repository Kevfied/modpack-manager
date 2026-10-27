import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---- Types shared with the Rust backend ----

export interface ModInfo {
  path: string;
  filename: string;
  size: number;
  sha1: string;
  sha512: string;
  modId: string | null;
  name: string | null;
  version: string | null;
  loaders: string[]; // loaders declared by the jar's own metadata
  environment: string | null; // client | server | both
  gameVersions: string[]; // MC versions declared in jar metadata (may be empty)
  parseError: string | null;
}

export interface ResolvedMod {
  sha1: string;
  projectId: string | null;
  versionId: string | null;
  projectTitle: string | null;
  projectSlug: string | null;
  versionNumber: string | null;
  gameVersions: string[];
  loaders: string[];
  downloadUrl: string | null;
  fileSize: number | null;
  clientSide: string | null; // required | optional | unsupported
  serverSide: string | null;
}

export interface ScanResult {
  mods: ModInfo[];
  totalSize: number;
}

export interface CompatReport {
  suggestedMcVersion: string | null;
  suggestedLoader: string | null;
  commonGameVersions: string[];
  commonLoaders: string[];
  conflicts: string[];
  missingDeps: string[];
}

export interface PackConfig {
  name: string;
  version: string;
  summary: string;
  mcVersion: string;
  loader: string;
  loaderVersion: string;
  selectedPaths: string[]; // ModInfo.path values to include
}

/** Config fields the user edits; selectedPaths is filled in at call time. */
export type PackSettings = Omit<PackConfig, "selectedPaths">;

export interface BuildResult {
  outputPath: string;
  resolvedCount: number;
  overrideCount: number;
}

export interface LauncherInfo {
  kind: string; // "prism" | "vanilla" | "tlauncher"
  name: string;
  path: string | null; // exe path if applicable
  dataDir: string | null;
}

export interface DetectedLoaders {
  launchers: LauncherInfo[];
  installedVersions: string[]; // entries in .minecraft/versions
}

export interface UpdateInfo {
  filename: string;
  projectTitle: string | null;
  currentVersion: string | null;
  newVersion: string | null;
  downloadUrl: string | null;
}

export interface DependencyWarning {
  modName: string;
  requiredProject: string;
  requiredTitle: string | null;
}

// ---- Commands ----

export const scanFolder = (path: string) =>
  invoke<ScanResult>("scan_folder", { path });

export const scanFiles = (paths: string[]) =>
  invoke<ScanResult>("scan_files", { paths });

export const resolveMods = (mods: ModInfo[]) =>
  invoke<Record<string, ResolvedMod>>("resolve_mods", { mods });

export const compatReport = (
  mods: ModInfo[],
  resolved: Record<string, ResolvedMod>,
  selectedPaths: string[]
) => invoke<CompatReport>("compat_report", { mods, resolved, selectedPaths });

export const getGameVersions = () => invoke<string[]>("get_game_versions");

export const getLoaderVersions = (loader: string, mcVersion: string) =>
  invoke<string[]>("get_loader_versions", { loader, mcVersion });

export const buildMrpack = (config: PackConfig, outDir: string) =>
  invoke<BuildResult>("build_mrpack", { config, outDir });

export const buildZip = (config: PackConfig, outDir: string) =>
  invoke<BuildResult>("build_zip", { config, outDir });

export const buildModlist = (
  config: PackConfig,
  resolved: Record<string, ResolvedMod>,
  outDir: string
) => invoke<BuildResult>("build_modlist", { config, resolved, outDir });

export const detectLaunchers = () =>
  invoke<DetectedLoaders>("detect_launchers");

export const loadIntoLauncher = (launcher: LauncherInfo, config: PackConfig) =>
  invoke<string>("load_into_launcher", { launcher, config });

export const openMrpack = (path: string) =>
  invoke<string>("open_mrpack", { path });

export interface PackFileInfo {
  name: string;
  version?: string;
  mcVersion?: string;
  loader?: string;
  loaderVersion?: string;
}

export const packExists = (launcher: LauncherInfo, name: string) =>
  invoke<boolean>("pack_exists", { launcher, name });

export const packFileInfo = (path: string) =>
  invoke<PackFileInfo>("pack_file_info", { path });

export const loadPackFile = (
  launcher: LauncherInfo,
  path: string,
  config: PackConfig
) => invoke<string>("load_pack_file", { launcher, path, config });

export const checkUpdates = (loader: string, mcVersion: string) =>
  invoke<UpdateInfo[]>("check_updates", { loader, mcVersion });

export const findMissingDeps = (
  mods: ModInfo[],
  resolved: Record<string, ResolvedMod>,
  selectedPaths: string[]
) => invoke<DependencyWarning[]>("find_missing_deps", { mods, resolved, selectedPaths });

// ---- Connect (LAN sync) ----

export interface FoundNetwork {
  networkId: string;
  name: string;
  packName: string;
  machine: string;
  addr: string;
  latestVersion: number;
  members: number;
  appVersion: string; // the host's running app version (display only)
  /** undefined = announce didn't say (host older than 0.1.9) — ask for code. */
  requiresCode?: boolean;
  /** true = announcing right now; false = remembered from a past session. */
  live: boolean;
  /** Epoch seconds of the last announce — for "last seen" display. */
  lastSeen: number;
}

export interface MemberView {
  id: string; // string — real ids exceed 2^53, JS numbers can't carry them
  name: string;
  online: boolean;
  appliedVersion: number;
  upToDate: boolean;
  targeted: boolean; // a member-only update is waiting for them
  host: boolean;
  appVersion: string; // the member's running app version
}

export interface PendingUpdate {
  version: number;
  packName: string;
  note: string;
  fileCount: number;
  toDownload: number;
  toDelete: number;
}

export interface ConnectStatus {
  mode: "idle" | "hosting" | "joined";
  networkName: string | null;
  networkId: string | null;
  code: string | null;
  folder: string | null;
  // host-side pack meta (confirmed at publish time)
  packName: string | null;
  packVersion: string | null;
  mcVersion: string | null;
  loader: string | null;
  loaderVersion: string | null;
  /** Folder the meta was last confirmed for — mismatch → re-suggest. */
  metaFolder: string | null;
  latestVersion: number;
  history: { version: number; note: string; ts: number }[];
  hostAddr: string | null;
  hostOnline: boolean;
  appliedVersion: number;
  displayName: string | null;
  members: MemberView[];
  pending: PendingUpdate | null;
  kicked: boolean; // we were kicked — session cleared, shown once
}

export interface SyncedPackInfo {
  networkId: string;
  name: string;
  version: number;
  mcVersion: string;
  loader: string;
  loaderVersion: string;
  fileCount: number;
}

export const hostStart = (
  name: string,
  code: string,
  folder: string,
  packName: string,
  mcVersion: string,
  loader: string,
  loaderVersion: string
) =>
  invoke<string>("host_start", {
    name,
    code,
    folder,
    packName,
    mcVersion,
    loader,
    loaderVersion,
  });

export interface PublishMeta {
  packName: string;
  packVersion: string;
  mcVersion: string;
  loader: string;
  loaderVersion: string;
}

export const uiLog = (tag: string, msg: string) =>
  invoke<void>("ui_log", { tag, msg }).catch(() => {});

export const hostPublish = (
  note: string,
  target: string | null,
  allowEmpty = false,
  force = false,
  meta?: PublishMeta
) =>
  invoke<string>("host_publish", {
    note,
    target,
    allowEmpty,
    force,
    packName: meta?.packName ?? "",
    packVersion: meta?.packVersion ?? "",
    mcVersion: meta?.mcVersion ?? "",
    loader: meta?.loader ?? "",
    loaderVersion: meta?.loaderVersion ?? "",
  });

export const hostSetFolder = (folder: string) =>
  invoke<{ samePct: number; fileCount: number }>("host_set_folder", {
    folder,
  });

export const hostUpdate = (
  name: string,
  code: string,
  packName: string,
  packVersion: string,
  mcVersion: string,
  loader: string,
  loaderVersion: string
) =>
  invoke<string>("host_update", {
    name,
    code,
    packName,
    packVersion,
    mcVersion,
    loader,
    loaderVersion,
  });

export interface PackMetaSuggest {
  packName: string;
  mcVersion: string;
  loader: string;
  loaderVersion: string;
}

export const suggestPackMeta = (folder: string) =>
  invoke<PackMetaSuggest>("suggest_pack_meta", { folder });

export const hostKick = (memberId: string) =>
  invoke<string>("host_kick", { memberId });

export const hostStop = () => invoke<void>("host_stop");

export const scanNetworks = () => invoke<FoundNetwork[]>("scan_networks");

export const joinNetwork = (addr: string, code: string, displayName: string) =>
  invoke<string>("join_network", { addr, code, displayName });

export const leaveNetwork = () => invoke<void>("leave_network");

/** `/info` for a manual join — `requiresCode` decides the code field. */
export const connectNetworkInfo = (addr: string) =>
  invoke<{ requiresCode?: boolean; name?: string }>("connect_network_info", { addr });

export const connectStatus = () => invoke<ConnectStatus>("connect_status");

export const applyUpdate = () => invoke<string>("apply_update");

/** Stop the running apply — .part files survive for the next resume. */
export const cancelApply = () => invoke<void>("cancel_apply");

/** Live payload of the "connect-apply-progress" event during apply. */
export interface ApplyFileProgress {
  name: string;
  done: number;
  total: number;
  retries: number;
}

export interface ApplyProgress {
  phase: "hashing" | "downloading" | "done" | "cancelled" | "failed";
  doneFiles: number;
  totalFiles: number;
  doneBytes: number;
  totalBytes: number;
  hashingDone: number;
  hashingTotal: number;
  hashingFile: string;
  files: ApplyFileProgress[];
  failed: string[];
}

// ---- Session logs ----

export const openLogsDir = () => invoke<void>("open_logs_dir");

export const tailLog = (n = 200) => invoke<string[]>("tail_log", { n });

/** Mirrors the tray's hidden flag so views can pause their polls. */
export const appHidden = { current: false };

export const declineUpdate = (version: number) =>
  invoke<void>("decline_update", { version });

export const listSyncedPacks = () =>
  invoke<SyncedPackInfo[]>("list_synced_packs");

export const loadSyncedPack = (networkId: string, launcher: LauncherInfo) =>
  invoke<string>("load_synced_pack", { networkId, launcher });

/** Host installs their own published pack — same deploy, live folder. */
export const loadHostPack = (launcher: LauncherInfo) =>
  invoke<string>("load_host_pack", { launcher });

// ---- App updates (LAN, standalone from Connect) ----

export interface AppUpdateStatus {
  state: "idle" | "offer" | "downloading" | "downloaded" | "waiting" | "skipped";
  version: string;
  notes: string;
  size: number;
  source: string; // machine name of the seeder
  progress: number; // 0..1
  runningVersion: string;
  /** New version while a post-update "updated" banner awaits dismissal. */
  justUpdated: string;
  /** Release notes captured at install time for the justUpdated banner. */
  updatedNotes: string;
}

export const appUpdateStatus = () =>
  invoke<AppUpdateStatus>("app_update_status");

export const downloadAppUpdate = () =>
  invoke<string>("download_app_update");

export const cancelAppUpdate = () => invoke<void>("cancel_app_update");

export const installAppUpdate = () => invoke<string>("install_app_update");

/** Dismiss the post-update "updated" banner permanently. */
export const ackAppUpdate = () => invoke<void>("ack_app_update");

/** Open a release's GitHub page in the default browser. */
export const openReleaseNotes = (version: string) =>
  invoke<void>("open_release_notes", { version });

export const skipAppVersion = (version: string) =>
  invoke<void>("skip_app_version", { version });

export const unskipAppVersion = () => invoke<void>("unskip_app_version");

export const pickUpdateBundle = (folder: string) =>
  invoke<string>("pick_update_bundle", { folder });

export const setUpdateHostIp = (addr: string) =>
  invoke<void>("set_update_host_ip", { addr });

export const setAutostart = (on: boolean) =>
  invoke<void>("set_autostart", { on });

// Track the tray-hidden flag once for all views — while hidden, polls
// pause so the app idles at ~0 CPU.
listen("window-hidden", () => (appHidden.current = true));
listen("window-shown", () => (appHidden.current = false));

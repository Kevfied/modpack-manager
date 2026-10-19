import { invoke } from "@tauri-apps/api/core";

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
}

export interface MemberView {
  id: number;
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

export const hostPublish = (
  note: string,
  target: number | null,
  allowEmpty = false
) => invoke<string>("host_publish", { note, target, allowEmpty });

export const hostSetFolder = (folder: string) =>
  invoke<{ samePct: number; fileCount: number }>("host_set_folder", {
    folder,
  });

export const hostUpdate = (name: string, code: string) =>
  invoke<string>("host_update", { name, code });

export const hostKick = (memberId: number) =>
  invoke<string>("host_kick", { memberId });

export const hostStop = () => invoke<void>("host_stop");

export const scanNetworks = () => invoke<FoundNetwork[]>("scan_networks");

export const joinNetwork = (addr: string, code: string, displayName: string) =>
  invoke<string>("join_network", { addr, code, displayName });

export const leaveNetwork = () => invoke<void>("leave_network");

export const connectStatus = () => invoke<ConnectStatus>("connect_status");

export const applyUpdate = () => invoke<string>("apply_update");

export const declineUpdate = (version: number) =>
  invoke<void>("decline_update", { version });

export const listSyncedPacks = () =>
  invoke<SyncedPackInfo[]>("list_synced_packs");

export const loadSyncedPack = (networkId: string, launcher: LauncherInfo) =>
  invoke<string>("load_synced_pack", { networkId, launcher });

// ---- App updates (LAN, standalone from Connect) ----

export interface AppUpdateStatus {
  state: "idle" | "offer" | "downloading" | "downloaded" | "waiting" | "skipped";
  version: string;
  notes: string;
  size: number;
  source: string; // machine name of the seeder
  progress: number; // 0..1
  runningVersion: string;
}

export const appUpdateStatus = () =>
  invoke<AppUpdateStatus>("app_update_status");

export const downloadAppUpdate = () =>
  invoke<string>("download_app_update");

export const cancelAppUpdate = () => invoke<void>("cancel_app_update");

export const installAppUpdate = () => invoke<string>("install_app_update");

export const skipAppVersion = (version: string) =>
  invoke<void>("skip_app_version", { version });

export const unskipAppVersion = () => invoke<void>("unskip_app_version");

export const pickUpdateBundle = (folder: string) =>
  invoke<string>("pick_update_bundle", { folder });

export const setUpdateHostIp = (addr: string) =>
  invoke<void>("set_update_host_ip", { addr });

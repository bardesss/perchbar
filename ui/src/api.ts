// Typed wrappers for the commands in app/src/commands.rs. Argument names are
// the Rust parameter names in camelCase (Tauri converts them).

import { invoke } from "@tauri-apps/api/core";
import { emit, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

export type SettingKind =
  | "text"
  | "number"
  | "select"
  | "toggle"
  | "list"
  | "secret"
  | "location"
  | "color";

export interface SettingField {
  key: string;
  label: string;
  kind: SettingKind;
  default: unknown;
  options: string[];
  required: boolean;
}

export interface Permissions {
  network: string[];
  files: string[];
  sources: string[];
}

export interface WidgetView {
  id: string;
  name: string;
  description: string;
  author: string;
  version: string;
  settings: SettingField[];
  permissions: Permissions;
  /** What the manifest asks for beyond the approval, while blocked. */
  added: Permissions;
  /** Blocked, and nothing was ever approved for it. */
  firstReview: boolean;
  local: boolean;
  blocked: string | null;
  folder: string | null;
}

/** What inspect_folder found in a folder. All text comes from the folder. */
export interface FolderReport {
  folder: string;
  id: string;
  name: string;
  version: string;
  author: string;
  description: string;
  permissions: Permissions;
  /** Set when installing would replace a local widget whose folder is missing. */
  replaces: ReplacedView | null;
  /** Why the folder cannot be installed; empty when it can. */
  errors: string[];
}

/** The missing local widget an install would replace. */
export interface ReplacedView {
  folder: string;
  instances: number;
  /** Whether any of its instances has a saved secret. */
  secrets: boolean;
}

export interface StatusView {
  /** `loading`, `ok`, `stale`, `error`, `hidden` or `blocked`. */
  state: string;
  text: string | null;
  message: string | null;
}

export interface InstanceView {
  id: string;
  widget: string;
  /** Never holds a secret. */
  settings: Record<string, unknown>;
  /** Secret keys with a stored value. */
  secretsSet: string[];
  secretsUnreadable: boolean;
  hidden: boolean;
  maxWidth: number | null;
  status: StatusView;
}

export interface Group {
  id: string;
  members: string[];
  interval: number;
}

/** A local widget whose folder is missing or broken. */
export interface MissingLocalView {
  id: string;
  folder: string;
}

export interface StateView {
  widgets: WidgetView[];
  missingLocal: MissingLocalView[];
  instances: InstanceView[];
  groups: Group[];
  /** Top-level strip order: ungrouped instance ids and group ids. */
  order: string[];
  autostart: boolean;
  version: string;
  logFolder: string;
}

export interface LogLine {
  /** Local time, RFC 3339. */
  at: string;
  level: "info" | "warn" | "error";
  text: string;
}

export interface FieldError {
  key: string;
  message: string;
}

/** What a failed command rejects with. */
export interface CommandError {
  message: string;
  fields?: FieldError[];
}

/** Any rejection as a CommandError. */
export function asCommandError(e: unknown): CommandError {
  if (e && typeof e === "object" && typeof (e as CommandError).message === "string") {
    const { message, fields } = e as CommandError;
    return Array.isArray(fields) ? { message, fields } : { message };
  }
  if (typeof e === "string") return { message: e };
  return { message: "Something went wrong" };
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw asCommandError(e);
  }
}

export const getVersion = () => call<string>("get_version");
export const getState = () => call<StateView>("get_state");
export const addInstance = (widget: string) => call<string>("add_instance", { widget });
export const removeInstance = (id: string) => call<void>("remove_instance", { id });
/** Replaces the whole settings map: send every non-secret field. */
export const updateSettings = (id: string, settings: Record<string, unknown>) =>
  call<void>("update_settings", { id, settings });
export const setSecret = (id: string, key: string, value: string) =>
  call<void>("set_secret", { id, key, value });
export const clearSecret = (id: string, key: string) => call<void>("clear_secret", { id, key });
export const setHidden = (id: string, hidden: boolean) => call<void>("set_hidden", { id, hidden });
export const setMaxWidth = (id: string, maxWidth: number | null) =>
  call<void>("set_max_width", { id, maxWidth });
export const setOrder = (order: string[]) => call<void>("set_order", { order });
export const createGroup = (members: string[], interval: number) =>
  call<string>("create_group", { members, interval });
export const updateGroup = (id: string, members: string[], interval: number) =>
  call<void>("update_group", { id, members, interval });
export const ungroup = (id: string) => call<void>("ungroup", { id });
export const refreshNow = (id: string) => call<void>("refresh_now", { id });
export const getLogs = (id: string) => call<LogLine[]>("get_logs", { id });
export const setAutostart = (on: boolean) => call<void>("set_autostart", { on });
export const openLogFolder = () => call<void>("open_log_folder");
export const inspectFolder = (path: string) => call<FolderReport>("inspect_folder", { path });
/**
 * `permissions` is what the user reviewed; anything more is refused.
 * `replace` confirms taking over a missing local widget with the same id.
 */
export const installLocal = (path: string, permissions: Permissions, replace: boolean) =>
  call<string>("install_local", { path, permissions, replace });
export const reloadWidget = (id: string) => call<void>("reload_widget", { id });
/** `permissions` is what the user reviewed; anything more is refused. */
export const approvePermissions = (id: string, permissions: Permissions) =>
  call<void>("approve_permissions", { id, permissions });
export const removeWidget = (id: string) => call<void>("remove_widget", { id });

/** The native folder picker. Resolves to null when cancelled. */
export async function pickFolder(): Promise<string | null> {
  const picked = await open({ directory: true, multiple: false, title: "Load widget from folder" });
  return typeof picked === "string" ? picked : null;
}

/** Widget state changed (at most every 300 ms). */
export function onStateChanged(cb: () => void): Promise<UnlistenFn> {
  return listen("perchbar://state-changed", () => cb());
}

/** Perchbar asks the page to show one instance. */
export function onFocus(cb: (id: string) => void): Promise<UnlistenFn> {
  return listen<string>("perchbar://focus", (e) => {
    if (typeof e.payload === "string") cb(e.payload);
  });
}

/** Tells Perchbar the page hears focus requests now. */
export function sendReady(): Promise<void> {
  return emit("perchbar://ready");
}

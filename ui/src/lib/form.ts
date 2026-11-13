// The settings form, as plain data. The checks mirror app/src/validate.rs so
// mistakes show while typing; the core still checks everything and its
// answer wins.

import type { CommandError, InstanceView, SettingField, SettingKind, WidgetView } from "../api";

/** Longest `text` value, in characters (validate.rs TEXT_MAX_CHARS). */
export const TEXT_MAX_CHARS = 2000;
/** Longest `location` value and `list` entry (SHORT_MAX_CHARS). */
export const SHORT_MAX_CHARS = 200;
/** Most entries in a list (LIST_MAX_ENTRIES). */
export const LIST_MAX_ENTRIES = 50;
/** Most entries a list may arrive with, empty ones included. */
export const LIST_MAX_RAW_ENTRIES = 200;
/** Longest secret, in characters (secrets.rs SECRET_MAX_CHARS). */
export const SECRET_MAX_CHARS = 4096;
/** Allowed maximum width, in pixels (MAX_WIDTH_RANGE). */
export const MAX_WIDTH_MIN = 40;
export const MAX_WIDTH_MAX = 600;
/** Allowed group rotation interval, in seconds. */
export const GROUP_INTERVAL_MIN = 3;
export const GROUP_INTERVAL_MAX = 3600;

/**
 * What an input holds: a string for text-like kinds and numbers (as typed),
 * a boolean for toggles, a string array for lists, nothing for secrets.
 */
export type FormValue = string | boolean | string[] | undefined;

export interface FormField {
  key: string;
  label: string;
  kind: SettingKind;
  options: string[];
  required: boolean;
  default: unknown;
  value: FormValue;
}

const NUMBER = /^[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$/;
const COLOR = /^#[0-9a-fA-F]{6}$/;

/** Characters as Rust counts them (code points). */
function chars(s: string): number {
  let n = 0;
  for (const _ of s) n++;
  return n;
}

/** The form for an instance: stored values, else defaults. */
export function toFormModel(fields: SettingField[], values: Record<string, unknown>): FormField[] {
  return fields.map((f) => {
    const stored = values[f.key];
    const raw = f.kind === "secret" ? undefined : stored ?? f.default ?? undefined;
    return {
      key: f.key,
      label: f.label,
      kind: f.kind,
      options: f.options,
      required: f.required,
      default: f.default,
      value: inputValue(f.kind, raw),
    };
  });
}

function inputValue(kind: SettingKind, raw: unknown): FormValue {
  switch (kind) {
    case "secret":
      return undefined;
    case "toggle":
      return raw === true;
    case "list":
      return Array.isArray(raw) ? raw.filter((x): x is string => typeof x === "string") : [];
    case "number":
      if (typeof raw === "number") return String(raw);
      return typeof raw === "string" ? raw : "";
    default:
      return typeof raw === "string" ? raw : "";
  }
}

/** The JSON value update_settings takes for an input value. `null` clears
 * the setting, so the default applies. */
export function toWire(field: SettingField, value: unknown): unknown {
  switch (field.kind) {
    case "number": {
      if (typeof value === "number") return value;
      const s = typeof value === "string" ? value.trim() : "";
      return s === "" ? null : Number(s);
    }
    case "select":
    case "color":
      return value === "" || value === undefined ? null : value;
    case "list":
      return Array.isArray(value)
        ? value.map((s) => (typeof s === "string" ? s.trim() : s)).filter((s) => s !== "")
        : value;
    case "secret":
      throw new Error("secrets are saved with set_secret");
    default:
      return value;
  }
}

function isFilled(v: unknown): boolean {
  if (v === null || v === undefined) return false;
  if (typeof v === "string") return v.trim() !== "";
  if (Array.isArray(v)) return v.length > 0;
  return true;
}

/** Why `value` would be refused, or null when it is fine. */
export function checkField(field: SettingField, value: unknown): string | null {
  if (field.kind === "secret") {
    if (typeof value !== "string" || value === "") return "Enter a value, or remove the secret";
    if (chars(value) > SECRET_MAX_CHARS) return `Secrets are at most ${SECRET_MAX_CHARS} characters`;
    return null;
  }
  const problem = checkKind(field, value);
  if (problem) return problem;
  if (field.required) {
    const wire = toWire(field, value);
    const filled = wire === null ? isFilled(field.default) : isFilled(wire);
    if (!filled) return "Required";
  }
  return null;
}

function checkKind(field: SettingField, value: unknown): string | null {
  const text = (max: number) => {
    if (typeof value !== "string") return "Must be text";
    return chars(value) > max ? `At most ${max} characters` : null;
  };
  switch (field.kind) {
    case "text":
      return text(TEXT_MAX_CHARS);
    case "location":
      return text(SHORT_MAX_CHARS);
    case "number":
      if (typeof value === "number") return Number.isFinite(value) ? null : "Must be a number";
      if (typeof value !== "string") return "Must be a number";
      if (value.trim() === "") return null;
      return NUMBER.test(value.trim()) && Number.isFinite(Number(value)) ? null : "Must be a number";
    case "select":
      if (value === "") return null;
      return typeof value === "string" && field.options.includes(value)
        ? null
        : "Must be one of the listed choices";
    case "toggle":
      return typeof value === "boolean" ? null : "Must be on or off";
    case "list": {
      if (!Array.isArray(value)) return "Must be a list";
      if (value.length > LIST_MAX_RAW_ENTRIES) return `At most ${LIST_MAX_ENTRIES} entries`;
      let kept = 0;
      for (const item of value) {
        if (typeof item !== "string") return "Each entry must be text";
        const s = item.trim();
        if (chars(s) > SHORT_MAX_CHARS) return `Each entry is at most ${SHORT_MAX_CHARS} characters`;
        if (s !== "") kept++;
      }
      return kept > LIST_MAX_ENTRIES ? `At most ${LIST_MAX_ENTRIES} entries` : null;
    }
    case "color":
      if (value === "") return null;
      return typeof value === "string" && COLOR.test(value) ? null : "Must be a color like #1a2b3c";
    default:
      return null;
  }
}

/**
 * The whole settings map for update_settings, which replaces what is
 * stored: `changes` (wire values) first, then the stored value, else null
 * (the default applies). Secrets and unknown keys are left out.
 */
export function settingsPayload(
  fields: SettingField[],
  stored: Record<string, unknown>,
  changes: Record<string, unknown>,
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const f of fields) {
    if (f.kind === "secret") continue;
    if (f.key in changes) out[f.key] = changes[f.key];
    else if (f.key in stored) out[f.key] = stored[f.key];
    else out[f.key] = null;
  }
  return out;
}

/** A command error's field messages by setting key. */
export function errorsByKey(err: CommandError): Record<string, string> {
  const out: Record<string, string> = {};
  for (const f of err.fields ?? []) out[f.key] = f.message;
  return out;
}

/** A maximum width as typed: empty means automatic. */
export function parseMaxWidth(raw: string): { value: number | null; error: string | null } {
  const s = raw.trim();
  if (s === "") return { value: null, error: null };
  const n = Number(s);
  if (!/^\d+$/.test(s) || n < MAX_WIDTH_MIN || n > MAX_WIDTH_MAX) {
    return { value: null, error: `Maximum width is ${MAX_WIDTH_MIN} to ${MAX_WIDTH_MAX} pixels` };
  }
  return { value: n, error: null };
}

/**
 * Row names: the widget name, numbered from the second instance of the
 * same widget on, in the order given (pass the order they were added, so a
 * name does not move when rows are reordered). An instance whose widget is
 * not installed shows the widget id.
 */
export function instanceLabels(
  instances: Pick<InstanceView, "id" | "widget">[],
  widgets: Pick<WidgetView, "id" | "name">[],
): Record<string, string> {
  const names = new Map(widgets.map((w) => [w.id, w.name]));
  const seen = new Map<string, number>();
  const out: Record<string, string> = {};
  for (const inst of instances) {
    const n = (seen.get(inst.widget) ?? 0) + 1;
    seen.set(inst.widget, n);
    const name = names.get(inst.widget) ?? inst.widget;
    out[inst.id] = n === 1 ? name : `${name} ${n}`;
  }
  return out;
}

/** The status line under a row's name. */
export function statusLine(inst: Pick<InstanceView, "hidden" | "status">): string {
  if (inst.hidden) return "Hidden";
  const { state, text, message } = inst.status;
  if ((state === "error" || state === "blocked") && message) return message;
  if (text) return text;
  return state === "loading" ? "Loading" : "";
}

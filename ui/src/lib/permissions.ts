// The permission review text (spec 5.4). Items come from a widget folder
// and are shown only through text bindings.

import type { Permissions } from "../api";

export interface PermissionLine {
  lead: string;
  items: string[];
}

/** One line per kind of access asked for, or one line saying there is none. */
export function permissionLines(p: Permissions): PermissionLine[] {
  const lines: PermissionLine[] = [];
  if (p.network.length > 0) lines.push({ lead: "This widget will contact:", items: p.network });
  if (p.files.length > 0) lines.push({ lead: "It will read:", items: p.files });
  if (p.sources.length > 0) lines.push({ lead: "It uses:", items: p.sources });
  return lines.length > 0 ? lines : [{ lead: "No network, file or data access", items: [] }];
}

/**
 * The line above a review of a blocked widget. A first review (nothing
 * approved before) has none: the permission lines say it all (spec 5.4).
 */
export function reviewIntro(name: string, firstReview: boolean): string | undefined {
  if (firstReview) return undefined;
  return `${name} now asks for more than you allowed. It stays off the taskbar until you allow:`;
}

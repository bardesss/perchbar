// Reordering the top-level strip order and a group's members. Moves return
// the new order, or null when nothing would change.

import { moveItem } from "./form";

/**
 * Moves `id` to just before `target` (a drop on the target row), or to the
 * end when `target` is null (a drop after the last row).
 */
export function moveBefore(order: string[], id: string, target: string | null): string[] | null {
  const from = order.indexOf(id);
  if (from < 0 || id === target) return null;
  let to = order.length - 1;
  if (target !== null) {
    const at = order.indexOf(target);
    if (at < 0) return null;
    // Taking the item out shifts the rows after it up by one.
    to = from < at ? at - 1 : at;
  }
  return from === to ? null : moveItem(order, from, to);
}

/** Moves `id` one place up (-1) or down (+1). */
export function moveBy(order: string[], id: string, delta: -1 | 1): string[] | null {
  const from = order.indexOf(id);
  const to = from + delta;
  if (from < 0 || to < 0 || to >= order.length) return null;
  return moveItem(order, from, to);
}

/** Where on a row a drop lands. */
export type DropZone = "before" | "into" | "after";

/**
 * The zone at `offset` pixels down a row `height` pixels tall: the top
 * third inserts before the row, the bottom third after it, the middle
 * groups. Halves when the drop cannot group.
 */
export function dropZone(offset: number, height: number, canGroup: boolean): DropZone {
  if (!canGroup) return offset < height / 2 ? "before" : "after";
  if (offset < height / 3) return "before";
  return offset < (height * 2) / 3 ? "into" : "after";
}

/** What a drop on the strip list does. */
export type DropAction =
  | { kind: "order"; order: string[] }
  | { kind: "create"; members: [string, string] }
  | { kind: "join"; group: string };

/**
 * The drop of top-level row `from` on row `target` (null: after the last
 * row). A single widget dropped into another one makes a group of the
 * two, into a group joins it. Groups do not nest, so a dragged group only
 * reorders.
 */
export function dropAction(
  order: string[],
  isGroup: (id: string) => boolean,
  from: string,
  target: string | null,
  zone: DropZone,
): DropAction | null {
  if (!order.includes(from) || from === target) return null;
  if (target !== null && !order.includes(target)) return null;
  if (zone === "into" && target !== null && !isGroup(from)) {
    return isGroup(target) ? { kind: "join", group: target } : { kind: "create", members: [target, from] };
  }
  // After a row is before the next one, or last.
  const before = zone === "after" && target !== null ? (order[order.indexOf(target) + 1] ?? null) : target;
  if (before === from) return null;
  const next = moveBefore(order, from, before);
  return next ? { kind: "order", order: next } : null;
}

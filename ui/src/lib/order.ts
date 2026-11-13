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

/** What a drop on the strip list does. */
export type DropAction =
  | { kind: "order"; order: string[] }
  | { kind: "create"; members: [string, string] }
  | { kind: "join"; group: string };

/**
 * The drop of top-level row `from` on row `target` (null: after the last
 * row). `into` is a drop on the row itself rather than on the gap above it:
 * a single widget dropped into another one makes a group of the two, into a
 * group joins it. Groups do not nest, so a dragged group only reorders.
 */
export function dropAction(
  order: string[],
  isGroup: (id: string) => boolean,
  from: string,
  target: string | null,
  into: boolean,
): DropAction | null {
  if (!order.includes(from) || from === target) return null;
  if (into && target !== null && order.includes(target) && !isGroup(from)) {
    return isGroup(target) ? { kind: "join", group: target } : { kind: "create", members: [target, from] };
  }
  const next = moveBefore(order, from, target);
  return next ? { kind: "order", order: next } : null;
}

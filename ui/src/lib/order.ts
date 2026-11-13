// Reordering the top-level strip order. Both return the new order, or null
// when nothing would change.

/** Moves `id` to just before `target` (a drop on the target row). */
export function moveBefore(order: string[], id: string, target: string): string[] | null {
  if (id === target || !order.includes(id) || !order.includes(target)) return null;
  const next = order.filter((o) => o !== id);
  next.splice(next.indexOf(target), 0, id);
  return next.every((o, i) => o === order[i]) ? null : next;
}

/** Moves `id` one place up (-1) or down (+1). */
export function moveBy(order: string[], id: string, delta: -1 | 1): string[] | null {
  const from = order.indexOf(id);
  const to = from + delta;
  if (from < 0 || to < 0 || to >= order.length) return null;
  const next = order.slice();
  next.splice(from, 1);
  next.splice(to, 0, id);
  return next;
}

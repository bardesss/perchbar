import { describe, expect, test } from "vitest";
import { dropAction, moveBefore, moveBy } from "./order";

test("dropping on a row inserts before it, also when dragging down", () => {
  expect(moveBefore(["a", "b", "c"], "a", "c")).toEqual(["b", "a", "c"]);
  expect(moveBefore(["a", "b", "c"], "c", "a")).toEqual(["c", "a", "b"]);
});

test("dropping on itself, the next row or an unknown row changes nothing", () => {
  expect(moveBefore(["a", "b", "c"], "a", "b")).toBeNull();
  expect(moveBefore(["a", "b"], "a", "a")).toBeNull();
  expect(moveBefore(["a", "b"], "x", "a")).toBeNull();
  expect(moveBefore(["a", "b"], "a", "x")).toBeNull();
});

test("move up and down by one, stopping at the ends", () => {
  expect(moveBy(["a", "b", "c"], "b", -1)).toEqual(["b", "a", "c"]);
  expect(moveBy(["a", "b", "c"], "b", 1)).toEqual(["a", "c", "b"]);
  expect(moveBy(["a", "b", "c"], "a", -1)).toBeNull();
  expect(moveBy(["a", "b", "c"], "c", 1)).toBeNull();
});

test("dropping after the last row moves to the end", () => {
  expect(moveBefore(["a", "b", "c"], "a", null)).toEqual(["b", "c", "a"]);
  expect(moveBefore(["a", "b", "c"], "c", null)).toBeNull();
});

describe("drop actions", () => {
  const order = ["a", "g", "b"];
  const isGroup = (id: string) => id === "g";

  test("a single dropped into another single makes a group, target first", () => {
    expect(dropAction(order, isGroup, "b", "a", true)).toEqual({ kind: "create", members: ["a", "b"] });
  });

  test("a single dropped into a group joins it", () => {
    expect(dropAction(order, isGroup, "a", "g", true)).toEqual({ kind: "join", group: "g" });
  });

  test("a group dropped into a row only reorders", () => {
    expect(dropAction(order, isGroup, "g", "a", true)).toEqual({ kind: "order", order: ["g", "a", "b"] });
  });

  test("a drop before a row or at the end reorders", () => {
    expect(dropAction(order, isGroup, "b", "a", false)).toEqual({ kind: "order", order: ["b", "a", "g"] });
    expect(dropAction(order, isGroup, "a", null, false)).toEqual({ kind: "order", order: ["g", "b", "a"] });
  });

  test("a drop that changes nothing does nothing", () => {
    expect(dropAction(order, isGroup, "a", "a", true)).toBeNull();
    expect(dropAction(order, isGroup, "a", "g", false)).toBeNull();
    expect(dropAction(order, isGroup, "x", "a", true)).toBeNull();
  });
});

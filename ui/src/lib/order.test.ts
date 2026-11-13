import { describe, expect, test } from "vitest";
import { dropAction, dropZone, moveBefore, moveBy } from "./order";

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

describe("drop zones", () => {
  test("thirds: top inserts before, middle groups, bottom inserts after", () => {
    expect(dropZone(0, 30, true)).toBe("before");
    expect(dropZone(9.9, 30, true)).toBe("before");
    expect(dropZone(10, 30, true)).toBe("into");
    expect(dropZone(19.9, 30, true)).toBe("into");
    expect(dropZone(20, 30, true)).toBe("after");
    expect(dropZone(30, 30, true)).toBe("after");
  });

  test("halves when the drop cannot group", () => {
    expect(dropZone(14, 30, false)).toBe("before");
    expect(dropZone(15, 30, false)).toBe("after");
  });
});

describe("drop actions", () => {
  const order = ["a", "g", "b"];
  const isGroup = (id: string) => id === "g";

  test("a single dropped into another single makes a group, target first", () => {
    expect(dropAction(order, isGroup, "b", "a", "into")).toEqual({ kind: "create", members: ["a", "b"] });
  });

  test("a single dropped into a group joins it", () => {
    expect(dropAction(order, isGroup, "a", "g", "into")).toEqual({ kind: "join", group: "g" });
  });

  test("a group dropped into a row only reorders", () => {
    expect(dropAction(order, isGroup, "g", "a", "into")).toEqual({ kind: "order", order: ["g", "a", "b"] });
  });

  test("a drop before a row or at the end reorders", () => {
    expect(dropAction(order, isGroup, "b", "a", "before")).toEqual({ kind: "order", order: ["b", "a", "g"] });
    expect(dropAction(order, isGroup, "a", null, "before")).toEqual({ kind: "order", order: ["g", "b", "a"] });
  });

  test("a drop after a row lands before the next row, or last", () => {
    expect(dropAction(order, isGroup, "a", "g", "after")).toEqual({ kind: "order", order: ["g", "a", "b"] });
    expect(dropAction(order, isGroup, "a", "b", "after")).toEqual({ kind: "order", order: ["g", "b", "a"] });
    expect(dropAction(order, isGroup, "b", "a", "after")).toEqual({ kind: "order", order: ["a", "b", "g"] });
  });

  test("a drop that changes nothing does nothing", () => {
    expect(dropAction(order, isGroup, "a", "a", "into")).toBeNull();
    expect(dropAction(order, isGroup, "a", "g", "before")).toBeNull();
    expect(dropAction(order, isGroup, "g", "a", "after")).toBeNull();
    expect(dropAction(order, isGroup, "b", "b", "after")).toBeNull();
    expect(dropAction(order, isGroup, "x", "a", "into")).toBeNull();
  });
});

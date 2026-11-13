import { expect, test } from "vitest";
import { moveBefore, moveBy } from "./order";

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

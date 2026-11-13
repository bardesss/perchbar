import { expect, test } from "vitest";

test("app module loads", async () => {
  expect(await import("../App.svelte")).toBeTruthy();
});

import { expect, test } from "vitest";
import { permissionLines, replaceNotice, reviewIntro } from "./permissions";

const none = { network: [], files: [], sources: [] };

test("empty permissions say so", () =>
  expect(permissionLines(none)).toEqual([{ lead: "No network, file or data access", items: [] }]));

test("each kind gets its own line, in order", () =>
  expect(
    permissionLines({ network: ["a.com", "b.com"], files: ["%USERPROFILE%/x.json"], sources: ["cpu"] }),
  ).toEqual([
    { lead: "This widget will contact:", items: ["a.com", "b.com"] },
    { lead: "It will read:", items: ["%USERPROFILE%/x.json"] },
    { lead: "It uses:", items: ["cpu"] },
  ]));

test("kinds with nothing are left out", () =>
  expect(permissionLines({ ...none, sources: ["cpu"] })).toEqual([{ lead: "It uses:", items: ["cpu"] }]));

test("a first review has no 'more than you allowed' intro", () => {
  expect(reviewIntro("My Clock", true)).toBeUndefined();
  expect(reviewIntro("My Clock", false)).toMatch(/^My Clock now asks for more than you allowed/);
});

test("replacing a missing widget says what carries over", () => {
  expect(replaceNotice("my-clock", { folder: "C:/old", instances: 2, secrets: true })).toBe(
    "This replaces my-clock, whose folder is missing (C:/old). Its 2 widgets on your taskbar keep their settings and saved keys.",
  );
  expect(replaceNotice("my-clock", { folder: "C:/old", instances: 1, secrets: false })).toBe(
    "This replaces my-clock, whose folder is missing (C:/old). Its 1 widget on your taskbar keeps its settings.",
  );
  expect(replaceNotice("my-clock", { folder: "C:/old", instances: 0, secrets: false })).toBe(
    "This replaces my-clock, whose folder is missing (C:/old).",
  );
});

test("taking over widgets left from an earlier install says so", () => {
  expect(replaceNotice("my-clock", { folder: null, instances: 2, secrets: true })).toBe(
    "Perchbar still has 2 widgets from an earlier my-clock on your taskbar. This folder takes them over, and they keep their settings and saved keys.",
  );
  expect(replaceNotice("my-clock", { folder: null, instances: 1, secrets: false })).toBe(
    "Perchbar still has 1 widget from an earlier my-clock on your taskbar. This folder takes it over, and it keeps its settings.",
  );
});

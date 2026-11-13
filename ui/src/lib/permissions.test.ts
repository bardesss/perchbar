import { expect, test } from "vitest";
import { permissionLines } from "./permissions";

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

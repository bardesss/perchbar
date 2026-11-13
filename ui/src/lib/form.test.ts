import { describe, expect, test } from "vitest";
import type { SettingField, StatusView } from "../api";
import {
  checkField,
  checkInterval,
  errorsByKey,
  instanceLabels,
  moveItem,
  parseMaxWidth,
  saveErrorMessage,
  settingsPayload,
  statusLine,
  toFormModel,
  toWire,
} from "./form";

function field(key: string, kind: SettingField["kind"], extra: Partial<SettingField> = {}): SettingField {
  return { key, label: key, kind, default: null, options: [], required: false, ...extra };
}

const num = field("rotate", "number");
const sel = field("unit", "select", { options: ["C", "F"] });
const list = field("cities", "list");
const color = field("tint", "color");
const text = field("title", "text");
const place = field("place", "location");
const toggle = field("on", "toggle");
const textWithDefault = field("city", "text", { default: "Amsterdam" });
const secret = field("apiKey", "secret");

test("number field rejects text", () => expect(checkField(num, "abc")).toMatch(/number/i));
test("select must be an option", () => expect(checkField(sel, "XYZ")).not.toBeNull());
test("list caps at 50 entries", () => expect(checkField(list, Array(51).fill("a"))).not.toBeNull());
test("color must be #rrggbb", () => {
  expect(checkField(color, "#12345")).not.toBeNull();
  expect(checkField(color, "#a1b2c3")).toBeNull();
});
test("form model uses defaults for missing values", () =>
  expect(toFormModel([textWithDefault], {})[0].value).toBe("Amsterdam"));
test("secret fields never carry a value", () =>
  expect(toFormModel([secret], { apiKey: "x" })[0].value).toBeUndefined());

describe("checkField mirrors validate.rs", () => {
  test("numbers: typed text and real numbers", () => {
    expect(checkField(num, "12.5")).toBeNull();
    expect(checkField(num, " -3 ")).toBeNull();
    expect(checkField(num, "1e3")).toBeNull();
    expect(checkField(num, 7)).toBeNull();
    expect(checkField(num, "")).toBeNull();
    expect(checkField(num, "0x10")).toMatch(/number/i);
    expect(checkField(num, Infinity)).toMatch(/number/i);
  });

  test("text lengths count characters, not UTF-16 units", () => {
    expect(checkField(text, "x".repeat(2000))).toBeNull();
    expect(checkField(text, "x".repeat(2001))).toMatch(/2000/);
    expect(checkField(text, "\u{1F600}".repeat(2000))).toBeNull();
    expect(checkField(place, "y".repeat(201))).toMatch(/200/);
    expect(checkField(text, 5)).toMatch(/text/i);
  });

  test("select accepts an option or nothing", () => {
    expect(checkField(sel, "F")).toBeNull();
    expect(checkField(sel, "")).toBeNull();
  });

  test("toggle must be on or off", () => {
    expect(checkField(toggle, true)).toBeNull();
    expect(checkField(toggle, "yes")).not.toBeNull();
  });

  test("lists ignore empty entries but cap the raw length", () => {
    expect(checkField(list, [...Array(50).fill("a"), "", " "])).toBeNull();
    expect(checkField(list, [...Array(199).fill(""), "x"])).toBeNull();
    expect(checkField(list, Array(201).fill(""))).not.toBeNull();
    expect(checkField(list, ["z".repeat(201)])).toMatch(/200/);
    expect(checkField(list, [" " + "z".repeat(200) + " "])).toBeNull();
    expect(checkField(list, "a")).not.toBeNull();
  });

  test("required fields: empty is refused unless a default fills it", () => {
    const req = field("city", "location", { required: true });
    expect(checkField(req, "")).toBe("Required");
    expect(checkField(req, "  ")).toBe("Required");
    expect(checkField(req, "Oslo")).toBeNull();
    const reqNum = field("n", "number", { required: true, default: 3 });
    expect(checkField(reqNum, "")).toBeNull();
    const reqList = field("l", "list", { required: true });
    expect(checkField(reqList, ["", " "])).toBe("Required");
  });

  test("secrets: length and empty", () => {
    expect(checkField(secret, "k")).toBeNull();
    expect(checkField(secret, "")).not.toBeNull();
    expect(checkField(secret, "s".repeat(4097))).toMatch(/4096/);
  });
});

describe("toFormModel", () => {
  test("stored values win over defaults, and kinds get input values", () => {
    const model = toFormModel([textWithDefault, num, toggle, list, sel], {
      city: "Oslo",
      rotate: 6,
      on: true,
      cities: ["a", "b"],
    });
    expect(model.map((f) => f.value)).toEqual(["Oslo", "6", true, ["a", "b"], ""]);
  });

  test("a stored null falls back to the default", () =>
    expect(toFormModel([textWithDefault], { city: null })[0].value).toBe("Amsterdam"));

  test("values of the wrong type do not leak into inputs", () => {
    const model = toFormModel([text, toggle, list], { title: 3, on: "yes", cities: [1, "a"] });
    expect(model.map((f) => f.value)).toEqual(["", false, ["a"]]);
  });
});

describe("toWire", () => {
  test("converts input values to what update_settings takes", () => {
    expect(toWire(num, "12.5")).toBe(12.5);
    expect(toWire(num, "")).toBeNull();
    expect(toWire(sel, "")).toBeNull();
    expect(toWire(color, "")).toBeNull();
    expect(toWire(text, "Hi there")).toBe("Hi there");
    expect(toWire(list, [" a ", "", "b"])).toEqual(["a", "b"]);
    expect(toWire(toggle, true)).toBe(true);
  });
});

describe("clearing a text field", () => {
  test("sends null, so the default comes back", () => {
    expect(toWire(text, "")).toBeNull();
    expect(toWire(textWithDefault, "   ")).toBeNull();
    expect(toWire(place, "")).toBeNull();
    const model = toFormModel([textWithDefault], { city: toWire(textWithDefault, "") });
    expect(model[0].value).toBe("Amsterdam");
  });

  test("a required text with a default may be cleared", () =>
    expect(checkField(field("t", "text", { required: true, default: "x" }), "")).toBeNull());
});

describe("saveErrorMessage", () => {
  const fields = [text, secret];
  const apiKey = { ...secret, label: "API key" };

  test("the edited field's own message wins", () =>
    expect(
      saveErrorMessage([text, apiKey], "title", {
        message: "Some settings need fixing",
        fields: [{ key: "title", message: "At most 2000 characters" }],
      }),
    ).toBe("At most 2000 characters"));

  test("a missing secret elsewhere points at that secret", () =>
    expect(
      saveErrorMessage([text, apiKey], "title", {
        message: "Some settings need fixing",
        fields: [{ key: "apiKey", message: "Required" }],
      }),
    ).toBe("Save the API key first"));

  test("another field's problem names that field", () =>
    expect(
      saveErrorMessage([text, place], "title", {
        message: "Some settings need fixing",
        fields: [{ key: "place", message: "Required" }],
      }),
    ).toBe("Fix place first"));

  test("a whole-map problem or a plain error shows its own message", () => {
    expect(
      saveErrorMessage(fields, "title", {
        message: "Some settings need fixing",
        fields: [{ key: "", message: "Settings are at most 16 KB in total" }],
      }),
    ).toBe("Settings are at most 16 KB in total");
    expect(saveErrorMessage(fields, "title", { message: "This widget is not installed" })).toBe(
      "This widget is not installed",
    );
  });
});

describe("settingsPayload", () => {
  test("sends every non-secret field: the change, stored values, null for the rest", () => {
    const fields = [textWithDefault, num, secret, toggle];
    const payload = settingsPayload(fields, { rotate: 6, apiKey: "never", stale: 1 }, { on: true });
    expect(payload).toEqual({ city: null, rotate: 6, on: true });
  });
});

describe("parseMaxWidth", () => {
  test("empty is automatic, 40 to 600 whole pixels otherwise", () => {
    expect(parseMaxWidth("")).toEqual({ value: null, error: null });
    expect(parseMaxWidth(" 120 ")).toEqual({ value: 120, error: null });
    expect(parseMaxWidth("39").error).toMatch(/40 to 600/);
    expect(parseMaxWidth("601").error).not.toBeNull();
    expect(parseMaxWidth("12.5").error).not.toBeNull();
    expect(parseMaxWidth("abc").error).not.toBeNull();
  });
});

describe("errorsByKey", () => {
  test("maps a command error's fields to their keys", () => {
    expect(
      errorsByKey({ message: "Some settings need fixing", fields: [{ key: "unit", message: "Must be one" }] }),
    ).toEqual({ unit: "Must be one" });
    expect(errorsByKey({ message: "x" })).toEqual({});
  });
});

describe("row text", () => {
  const widgets = [{ id: "world-clock", name: "World Clock" }, { id: "btc", name: "Bitcoin Price" }];
  const inst = (id: string, widget: string, status: Partial<StatusView> = {}, hidden = false) => ({
    id,
    widget,
    hidden,
    status: { state: "ok", text: null, message: null, ...status },
  });

  test("instances of the same widget get numbers in the order given", () => {
    const labels = instanceLabels(
      [inst("a", "world-clock"), inst("b", "btc"), inst("c", "world-clock"), inst("d", "gone")],
      widgets,
    );
    expect(labels).toEqual({ a: "World Clock", b: "Bitcoin Price", c: "World Clock 2", d: "gone" });
  });

  test("status line: hidden, then message for errors, then the strip text", () => {
    expect(statusLine(inst("a", "w", { text: "12:00" }, true))).toBe("Hidden");
    expect(statusLine(inst("a", "w", { state: "error", text: "old", message: "No network" }))).toBe("No network");
    expect(statusLine(inst("a", "w", { state: "blocked", message: "Not allowed" }))).toBe("Not allowed");
    expect(statusLine(inst("a", "w", { text: "12:00" }))).toBe("12:00");
    expect(statusLine(inst("a", "w", { state: "loading" }))).toBe("Loading");
    expect(statusLine(inst("a", "w", { state: "ok" }))).toBe("");
  });
});

test("moveItem moves down and up", () => {
  expect(moveItem(["a", "b", "c"], 0, 2)).toEqual(["b", "c", "a"]);
  expect(moveItem(["a", "b", "c"], 2, 0)).toEqual(["c", "a", "b"]);
});

test("moveItem leaves the list alone and returns a copy", () => {
  const list = ["a", "b", "c"];
  expect(moveItem(list, 1, 1)).toEqual(["a", "b", "c"]);
  expect(moveItem(list, -1, 1)).toEqual(["a", "b", "c"]);
  expect(moveItem(list, 0, 3)).toEqual(["a", "b", "c"]);
  expect(moveItem(list, 0, 1)).not.toBe(list);
  expect(list).toEqual(["a", "b", "c"]);
});

test("interval bounds", () => {
  expect(checkInterval(2)).not.toBeNull();
  expect(checkInterval(3)).toBeNull();
  expect(checkInterval(3601)).not.toBeNull();
});

test("interval as typed: whole seconds only", () => {
  expect(checkInterval("8")).toBeNull();
  expect(checkInterval(" 3600 ")).toBeNull();
  expect(checkInterval("")).not.toBeNull();
  expect(checkInterval("4.5")).not.toBeNull();
  expect(checkInterval(4.5)).not.toBeNull();
  expect(checkInterval("1e3")).not.toBeNull();
  expect(checkInterval(null)).not.toBeNull();
});

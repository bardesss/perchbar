import { expect, test, vi } from "vitest";
import { createSaver, keyedSavers, type Slot } from "./saving";

function slot(draft: unknown) {
  const s = {
    value: draft as unknown,
    error: null as string | null,
    saved: false,
    draft: () => s.value,
    clearDraft: () => (s.value = undefined),
    setError: (m: string | null) => (s.error = m),
    setSaved: (on: boolean) => (s.saved = on),
  };
  return s satisfies Slot;
}

test("jobs run one at a time, in the order queued", async () => {
  const log: string[] = [];
  const saver = createSaver(async () => {});
  let release!: () => void;
  const first = saver.run(async () => {
    log.push("first start");
    await new Promise<void>((r) => (release = r));
    log.push("first end");
  });
  const second = saver.run(async () => {
    log.push("second");
  });
  await Promise.resolve();
  expect(log).toEqual(["first start"]);
  release();
  await Promise.all([first, second]);
  expect(log).toEqual(["first start", "first end", "second"]);
});

test("a failed job does not stop the queue", async () => {
  const saver = createSaver(async () => {});
  const failed = saver.run(async () => {
    throw new Error("no");
  });
  await expect(failed).rejects.toThrow("no");
  let ran = false;
  await saver.run(async () => {
    ran = true;
  });
  expect(ran).toBe(true);
});

test("a save sends, waits for the refetch, then drops the draft and flashes saved", async () => {
  vi.useFakeTimers();
  try {
    const log: string[] = [];
    const saver = createSaver(async () => {
      log.push("refetched");
    });
    const s = slot("8");
    s.error = "old";
    await saver.save("interval", s, "8", async () => {
      log.push("sent");
    }, () => log.push("failed"));
    expect(log).toEqual(["sent", "refetched"]);
    expect(s.value).toBeUndefined();
    expect(s.error).toBeNull();
    expect(s.saved).toBe(true);
    vi.advanceTimersByTime(2000);
    expect(s.saved).toBe(false);
  } finally {
    vi.useRealTimers();
  }
});

test("a save of a draft edited again meanwhile is skipped", async () => {
  const saver = createSaver(async () => {});
  const s = slot("9");
  let sent = false;
  await saver.save("interval", s, "8", async () => {
    sent = true;
  }, () => {});
  expect(sent).toBe(false);
  expect(s.value).toBe("9");
});

test("a refused save reports the error and keeps the draft", async () => {
  const saver = createSaver(async () => {});
  const s = slot("8");
  let message = "";
  await saver.save("interval", s, "8", async () => {
    throw { message: "Rotation interval is 3 to 3600 seconds" };
  }, (e) => (message = e.message));
  expect(message).toBe("Rotation interval is 3 to 3600 seconds");
  expect(s.value).toBe("8");
});

test("one group's changes share a queue, each reading what the last one stored", async () => {
  // An interval save from the editor, then a join from the list.
  let stored = { members: ["a", "b"], interval: 8 };
  const forGroup = keyedSavers(async () => {});
  expect(forGroup("g")).toBe(forGroup("g"));
  const interval = forGroup("g").run(async () => {
    const next = { ...stored, interval: 4 };
    await new Promise((r) => setTimeout(r, 5));
    stored = next;
  });
  const join = forGroup("g").run(async () => {
    stored = { ...stored, members: [...stored.members, "c"] };
  });
  await Promise.all([interval, join]);
  expect(stored).toEqual({ members: ["a", "b", "c"], interval: 4 });
});

test("different groups do not wait for each other", async () => {
  const forGroup = keyedSavers(async () => {});
  let release!: () => void;
  const slow = forGroup("g1").run(() => new Promise<void>((r) => (release = r)));
  let other = false;
  await forGroup("g2").run(async () => {
    other = true;
  });
  expect(other).toBe(true);
  release();
  await slow;
});

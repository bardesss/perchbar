import { expect, test } from "vitest";
import { latestRefresher } from "./latest";

/** A fetch whose answers the test hands out by hand. */
function manualFetch() {
  const pending: ((v: number) => void)[] = [];
  const fetch = () => new Promise<number>((resolve) => pending.push(resolve));
  return { fetch, answer: (i: number, v: number) => pending[i](v) };
}

test("a refresh overtaken by a newer one still applies its answer before resolving", async () => {
  const f = manualFetch();
  const applied: number[] = [];
  const refresh = latestRefresher(f.fetch, (v) => applied.push(v), () => {});
  let firstDone = false;
  const first = refresh().then(() => (firstDone = true));
  const second = refresh();
  f.answer(0, 1);
  await first;
  // The caller of the first refresh sees state at least as new as its own fetch.
  expect(firstDone).toBe(true);
  expect(applied).toEqual([1]);
  f.answer(1, 2);
  await second;
  expect(applied).toEqual([1, 2]);
});

test("an answer older than one already applied is dropped", async () => {
  const f = manualFetch();
  const applied: number[] = [];
  const refresh = latestRefresher(f.fetch, (v) => applied.push(v), () => {});
  const first = refresh();
  const second = refresh();
  f.answer(1, 2);
  await second;
  f.answer(0, 1);
  await first;
  expect(applied).toEqual([2]);
});

test("a failure is reported only when nothing newer was applied", async () => {
  let n = 0;
  const failures: unknown[] = [];
  const refresh = latestRefresher(
    () => (++n === 1 ? Promise.reject("boom") : Promise.resolve(n)),
    () => {},
    (e) => failures.push(e),
  );
  await refresh();
  expect(failures).toEqual(["boom"]);
});

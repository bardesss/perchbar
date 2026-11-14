// State refetches that may overlap. Each answer newer than the last one
// applied is applied, older ones are dropped, and every refresh resolves
// only once state at least as new as its own fetch is in. So a caller that
// awaits refresh() may read the state it just changed.

export function latestRefresher<T>(
  fetch: () => Promise<T>,
  apply: (value: T) => void,
  fail: (error: unknown) => void,
): () => Promise<void> {
  let started = 0;
  let applied = 0;
  return async () => {
    const mine = ++started;
    try {
      const value = await fetch();
      if (mine > applied) {
        applied = mine;
        apply(value);
      }
    } catch (e) {
      if (mine > applied) fail(e);
    }
  };
}

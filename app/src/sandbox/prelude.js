// Evaluated before widget.js in every fresh context. Captures the hidden
// __host_* functions installed by Rust, deletes them from the global object
// (widget code can never reach them) and installs the widget-facing API.
// The script's completion value is the function Rust uses to call the
// widget's default export; it resolves to the JSON text of the result.
(() => {
  "use strict";
  const g = globalThis;
  const hostFetch = g.__host_fetch;
  const hostStorageGet = g.__host_storage_get;
  const hostStorageSet = g.__host_storage_set;
  const hostLog = g.__host_log;
  const hostTimeNow = g.__host_time_now;
  for (const name of [
    "__host_fetch",
    "__host_storage_get",
    "__host_storage_set",
    "__host_log",
    "__host_time_now",
  ]) {
    delete g[name];
  }

  const show = (v) => {
    if (typeof v === "string") return v;
    try {
      const s = JSON.stringify(v);
      return s === undefined ? String(v) : s;
    } catch {
      return String(v);
    }
  };
  const logAt =
    (level) =>
    (...args) => {
      hostLog(args.map(show).join(" "), level);
    };
  const log = logAt("info");

  const group = (digits) => digits.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  const number = (n, decimals = 0) => {
    const x = Number(n);
    if (!Number.isFinite(x)) return String(x);
    const fixed = Math.abs(x).toFixed(decimals);
    const [int, frac] = fixed.split(".");
    const sign = x < 0 && Number(fixed) !== 0 ? "-" : "";
    return sign + group(int) + (frac ? "." + frac : "");
  };
  const SYMBOLS = { USD: "$", EUR: "€", GBP: "£" };
  const money = (n, currency) => {
    const x = Number(n);
    const code = String(currency).toUpperCase();
    const decimals = Math.abs(x) < 1000 ? 2 : 0;
    const digits = number(Math.abs(x), decimals);
    const sign = x < 0 && digits.replace(/[0.,]/g, "") !== "" ? "-" : "";
    const symbol = SYMBOLS[code];
    return symbol ? sign + symbol + digits : sign + digits + " " + code;
  };
  const percent = (fraction, decimals = 0) => number(Number(fraction) * 100, decimals) + "%";
  g.format = Object.freeze({ number, money, percent });

  g.console = Object.freeze({
    log,
    info: log,
    warn: logAt("warn"),
    error: logAt("error"),
  });

  g.fetch = async function fetch(url, init = {}) {
    const headers = Object.entries(init.headers ?? {}).map(([k, v]) => [String(k), String(v)]);
    const req = {
      method: String(init.method ?? "GET").toUpperCase(),
      url: String(url),
      headers,
      body: init.body == null ? null : String(init.body),
    };
    const resp = JSON.parse(await hostFetch(JSON.stringify(req)));
    const body = resp.body;
    return Object.freeze({
      ok: resp.status >= 200 && resp.status < 300,
      status: resp.status,
      headers: Object.fromEntries(resp.headers.map(([k, v]) => [k.toLowerCase(), v])),
      text: async () => body,
      json: async () => JSON.parse(body),
    });
  };

  const storage = Object.freeze({
    get: (key) => {
      const json = hostStorageGet(String(key));
      return json === undefined ? null : JSON.parse(json);
    },
    set: (key, value) => {
      const json = JSON.stringify(value);
      hostStorageSet(String(key), json === undefined ? "null" : json);
    },
  });

  const time = Object.freeze({
    now: (tz) => JSON.parse(hostTimeNow(tz === undefined ? null : String(tz))),
  });

  return async (update, settingsJson) => {
    if (typeof update !== "function") {
      throw new TypeError("widget.js must have a default export that is a function");
    }
    const settings = JSON.parse(settingsJson);
    const result = await update({ settings, storage, log, time });
    return JSON.stringify(result);
  };
})()

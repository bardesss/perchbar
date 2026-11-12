// Bitcoin Price: the Coinbase spot price of one coin, colored against the
// previous price, with a sparkline of the last 24 prices.
const HISTORY = 24;
const SYMBOLS = { USD: "$", EUR: "€", GBP: "£" };

export default async function update({ settings, storage, time }) {
  const coin = settings.coin ?? "BTC";
  const currency = settings.currency ?? "USD";
  const pair = `${coin}-${currency}`;

  // Coinbase answers {"data":{"amount":"67120.41", ...}}.
  const res = await fetch(`https://api.coinbase.com/v2/prices/${encodeURIComponent(pair)}/spot`);
  if (!res.ok) throw new Error(`Coinbase returned HTTP ${res.status} for ${pair}`);
  const price = Number((await res.json())?.data?.amount);
  if (!Number.isFinite(price)) throw new Error(`Coinbase sent no price for ${pair}`);

  // Storage is per instance; start over when the pair changes.
  const saved = storage.get("pair") === pair ? storage.get("history") ?? [] : [];
  const last = saved[saved.length - 1];
  const history = [...saved, price].slice(-HISTORY);
  storage.set("pair", pair);
  storage.set("history", history);

  // Color the price against the previous run; unchanged stays default.
  const color = last === undefined || price === last ? "default" : price > last ? "up" : "down";

  const blocks = [
    // The "bitcoin" icon falls back to the text "₿" when the font lacks it.
    coin === "BTC" ? { type: "icon", name: "bitcoin" } : { type: "text", value: coin },
    { type: "text", value: format.money(price, currency), color },
  ];
  if (history.length >= 2) {
    const trend = history[history.length - 1] >= history[0] ? "up" : "down";
    blocks.push({ type: "spark", values: history, color: trend });
  }

  const exact = (SYMBOLS[currency] ?? "") + format.number(price, price < 1 ? 4 : 2);
  const tooltip = `${pair} ${exact} · updated ${time.now().hhmm}`;
  return { blocks, tooltip };
}

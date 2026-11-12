// World Clock: the time in each configured city. Each entry of the "cities"
// setting is "Label|IANA time zone", for example "Tokyo|Asia/Tokyo".

// Minutes east of UTC as "UTC+9", "UTC+5:30" or "UTC-3".
function utcOffset(minutes) {
  const sign = minutes < 0 ? "-" : "+";
  const abs = Math.abs(minutes);
  const mins = abs % 60;
  return `UTC${sign}${Math.floor(abs / 60)}${mins ? ":" + String(mins).padStart(2, "0") : ""}`;
}

export default async function update({ settings, time }) {
  const cities = settings.cities ?? [];
  if (cities.length === 0) throw new Error("Add at least one city in the settings");

  const frames = cities.map((entry) => {
    // "Tokyo|Asia/Tokyo"; without a label the zone's last part is used.
    const [label, zone] = entry.includes("|") ? entry.split("|") : [entry.split("/").pop(), entry];
    let t;
    try {
      t = time.now(zone.trim());
    } catch (e) {
      throw new Error(`Unknown time zone "${zone.trim()}" in "${entry}"`);
    }
    const name = label.trim();
    return {
      blocks: [{ type: "text", value: `${name} ${t.hhmm}` }],
      tooltip: `${name} · ${t.weekday} · ${utcOffset(t.offsetMinutes)}`,
    };
  });

  // One city: a single frame. More: one frame per city, shown in turn.
  if (frames.length === 1) return frames[0];
  const rotate = Math.max(2, Math.round(Number(settings.rotate) || 6));
  return { frames, rotate };
}

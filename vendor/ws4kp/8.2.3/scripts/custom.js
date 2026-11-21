// The lobby's regional forecast tours the country, a round of pages per region. WeatherStar
// loads this file itself. Changing location in place restarts the round on the old region's
// data, so the page reloads on the next region instead, as the round ends.
const REGIONS = [
  [41.9, -87.6], // Great Lakes, where the lobby's frame opens
  [40.6, -74.5], // Northeast
  [37.6, -84.0], // Ohio Valley
  [33.2, -84.4], // Southeast
  [28.2, -81.8], // Florida
  [31.2, -96.8], // Texas
  [39.2, -95.8], // Plains
  [40.2, -106.5], // Rockies
  [34.0, -115.5], // Southwest
  [38.4, -121.5], // Northern California
  [46.4, -121.2], // Northwest
];

const regionTitle = document.querySelector("#regional-forecast-html .title.dual .bottom");
let forecastShown = false;
new MutationObserver(() => {
  if (regionTitle.textContent !== "Observations") {
    forecastShown = true;
    return;
  }
  if (!forecastShown) return;
  const query = new URLSearchParams(location.search);
  let here = null;
  try {
    here = JSON.parse(query.get("latLon"));
  } catch {}
  const current = REGIONS.findIndex(([lat, lon]) => lat === here?.lat && lon === here?.lon);
  const [lat, lon] = REGIONS[(current + 1) % REGIONS.length];
  query.set("latLon", JSON.stringify({ lat, lon }));
  // Hidden so the old region's first page never shows while the next one loads.
  document.body.style.visibility = "hidden";
  location.replace(`?${query}`);
}).observe(regionTitle, { childList: true });

// The lobby's regional forecast tours the country, a round of pages per region. After each region
// it stops where a severe or extreme warning is in force on land, when there is one it has not
// visited in the last 10 minutes, so WeatherStar's alerts screen and red ticker show. WeatherStar
// loads this file itself. Changing location in place restarts the round on the old stop's data,
// so the page reloads on the next stop instead, as the round ends.
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
const WARNINGS =
  "https://api.weather.gov/alerts/active?status=actual&message_type=alert&severity=Extreme,Severe&region_type=land";
const WARNINGS_FRESH_FOR = 10 * 60_000;
// A stop WeatherStar failed to load never ends its round; the tour moves on anyway.
const LONGEST_STOP = 5 * 60_000;

const query = new URLSearchParams(location.search);
let here = null;
try {
  here = JSON.parse(query.get("latLon"));
} catch {}
const regionHere = REGIONS.findIndex(([lat, lon]) => lat === here?.lat && lon === here?.lon);

// WeatherStar's map covers the lower 48 only.
const inLower48 = ([lat, lon]) => lat > 24 && lat < 50 && lon > -125 && lon < -66;

// The middle of a warning's largest outline, so a coastal area's islands never pull it out to sea.
const middle = (geometry) => {
  const rings = geometry.type === "Polygon" ? geometry.coordinates : geometry.coordinates.flat();
  const ring = rings.reduce((largest, next) => (next.length > largest.length ? next : largest));
  const lat = ring.reduce((sum, [, lat]) => sum + lat, 0) / ring.length;
  const lon = ring.reduce((sum, [lon]) => sum + lon, 0) / ring.length;
  return [+lat.toFixed(4), +lon.toFixed(4)];
};

const getJson = (url) => fetch(url).then((response) => (response.ok ? response.json() : null));

// Whether the warning is in force at that point, as WeatherStar's alerts screen will ask.
const appliesAt = async ([lat, lon], id) => {
  const alerts = await getJson(
    `https://api.weather.gov/alerts/active?point=${lat},${lon}&status=actual`,
  ).catch(() => null);
  return alerts?.features?.some((alert) => alert.id === id) ?? false;
};

// The tour's place across reloads: the last region shown, and the warnings, with those visited.
const loadTour = () => {
  try {
    return JSON.parse(sessionStorage.getItem("tour")) ?? {};
  } catch {
    return {};
  }
};
const saveTour = (tour) => {
  try {
    sessionStorage.setItem("tour", JSON.stringify(tour));
  } catch {}
};

// Worked out while this stop plays, so the next is ready when its round ends.
const nextStop = (async () => {
  const tour = loadTour();
  if (regionHere >= 0) {
    tour.region = regionHere;
    if (!(Date.now() - tour.fetched < WARNINGS_FRESH_FOR)) {
      const warnings = await getJson(WARNINGS).catch(() => null);
      tour.fetched = Date.now();
      tour.visited = [];
      tour.warnings = (warnings?.features ?? []).map((warning) => ({
        id: warning.id,
        at: warning.geometry && middle(warning.geometry),
        zone: warning.properties.affectedZones?.[0],
      }));
    }
    for (const warning of tour.warnings) {
      if (tour.visited.includes(warning.id)) continue;
      tour.visited.push(warning.id);
      const zone = !warning.at && warning.zone && (await getJson(warning.zone).catch(() => null));
      const at = warning.at ?? (zone?.geometry && middle(zone.geometry));
      if (at && inLower48(at) && (await appliesAt(at, warning.id))) {
        saveTour(tour);
        return at;
      }
    }
  }
  tour.region = ((tour.region ?? -1) + 1) % REGIONS.length;
  saveTour(tour);
  return REGIONS[tour.region];
})();

let leaving = false;
const leave = async () => {
  if (leaving) return;
  leaving = true;
  // Hidden so the old stop's first page never shows while the next one loads.
  document.body.style.visibility = "hidden";
  const [lat, lon] = await nextStop.catch(() => REGIONS[(regionHere + 1) % REGIONS.length]);
  query.set("latLon", JSON.stringify({ lat, lon }));
  location.replace(`?${query}`);
};
setTimeout(leave, LONGEST_STOP);

const regionTitle = document.querySelector("#regional-forecast-html .title.dual .bottom");
let forecastShown = false;
new MutationObserver(() => {
  if (regionTitle.textContent !== "Observations") {
    forecastShown = true;
    return;
  }
  if (forecastShown) leave();
}).observe(regionTitle, { childList: true });

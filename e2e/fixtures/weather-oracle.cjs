"use strict";

// The domain MockOracle supplies announcements and attestations, but has no weather
// model. Exercise the real HTTP forecast cache with explicit, synthetic station data.
const { createServer } = require("node:http");
const DAY = 86_400_000;
const stations = [
  { station_id: "KORD", station_name: "Chicago O'Hare International Airport", latitude: 41.98, longitude: -87.9 },
  { station_id: "KJFK", station_name: "John F. Kennedy International Airport", latitude: 40.64, longitude: -73.78 },
  { station_id: "KLAX", station_name: "Los Angeles International Airport", latitude: 33.94, longitude: -118.41 },
];
const forecasts = { KORD: [70, 50, 10], KJFK: [68, 48, 12], KLAX: [75, 58, 8] };

function forecastRows(params) {
  const start = Date.parse(params.get("start"));
  const end = Date.parse(params.get("end"));
  const issued = start - 60 * 60 * 1000;
  const generatedStart = Date.parse(params.get("generated_start"));
  const generatedEnd = Date.parse(params.get("generated_end"));
  if (!Number.isFinite(start) || !Number.isFinite(end) || start >= end || end - start > 7 * DAY) {
    throw new Error("Expected a forecast window of at most seven days");
  }
  if (params.get("temperature_unit") !== "fahrenheit") {
    throw new Error("Expected the coordinator's Fahrenheit query");
  }
  // One synthetic issue before the window, eligible only inside the requested issue cutoff.
  if (!Number.isFinite(generatedStart) || !Number.isFinite(generatedEnd)) {
    throw new Error("Expected the coordinator's explicit forecast issue cutoff");
  }
  if (issued < generatedStart || issued > generatedEnd) return [];
  const ids = new Set((params.get("station_ids") || "").split(","));
  const rows = [];
  for (const { station_id } of stations.filter((station) => ids.has(station.station_id))) {
    const [temp_high, temp_low, wind_speed] = forecasts[station_id];
    for (let day = Math.floor(start / DAY) * DAY; day < end; day += DAY) {
      rows.push({
        station_id, date: new Date(day).toISOString().slice(0, 10),
        start_time: new Date(Math.max(day, start)).toISOString(),
        end_time: new Date(Math.min(day + DAY, end)).toISOString(),
        temp_high, temp_low, wind_speed, temp_unit_code: "F",
        wind_direction: null, humidity_max: null, humidity_min: null,
        precip_chance: null, rain_amt: null, snow_amt: null, ice_amt: null,
      });
    }
  }
  return rows;
}

const server = createServer((request, response) => {
  const url = new URL(request.url, "http://127.0.0.1:9992");
  response.setHeader("Content-Type", "application/json");
  try {
    if (request.method !== "GET") {
      response.writeHead(405).end(JSON.stringify({ error: "Read-only weather fixture" }));
    } else if (url.pathname === "/stations") {
      response.end(JSON.stringify(stations));
    } else if (/^\/oracle\/events\/[^/]+$/.test(url.pathname)) {
      // A newly created event has no stored readings. Its UI must obtain the early forecast.
      response.end(JSON.stringify({ readings: [], entries: [], attestation: null }));
    } else if (url.pathname === "/stations/forecasts") {
      response.end(JSON.stringify(forecastRows(url.searchParams)));
    } else if (url.pathname === "/stations/observations") {
      response.end("[]");
    } else {
      response.writeHead(404).end(JSON.stringify({ error: "Unknown weather fixture route" }));
    }
  } catch (error) {
    response.writeHead(400).end(JSON.stringify({ error: error.message }));
  }
});
server.listen(9992, "127.0.0.1");
for (const signal of ["SIGINT", "SIGTERM"]) process.on(signal, () => server.close());

// Location Selector - Map & Table Interactions

// Track selected stations
const selectedStations = new Set();

// Map zoom/pan state
let mapScale = 1;
let mapTranslateX = 0;
let mapTranslateY = 0;
let isPanning = false;
let panStartX = 0;
let panStartY = 0;

const MIN_SCALE = 1;
const MAX_SCALE = 5;
const ZOOM_SENSITIVITY = 0.001;

// Switch between map and table views
window.switchLocationView = function (view) {
  const mapView = document.getElementById("location-map-view");
  const tableView = document.getElementById("location-table-view");
  const tabs = document.querySelectorAll(".tabs li[data-view]");

  if (!mapView || !tableView) return;

  // Update tab active state
  tabs.forEach((tab) => {
    if (tab.dataset.view === view) {
      tab.classList.add("is-active");
    } else {
      tab.classList.remove("is-active");
    }
  });

  // Show/hide views
  if (view === "map") {
    mapView.style.display = "block";
    tableView.style.display = "none";
  } else {
    mapView.style.display = "none";
    tableView.style.display = "block";
  }

  // Persist preference
  localStorage.setItem("locationView", view);
};

// Toggle station selection from map marker
window.toggleStationSelection = function (marker, event) {
  // Prevent triggering when panning
  if (event && event.defaultPrevented) return;

  const stationId = marker.dataset.stationId;

  if (selectedStations.has(stationId)) {
    selectedStations.delete(stationId);
    marker.classList.remove("selected");
  } else {
    selectedStations.add(stationId);
    marker.classList.add("selected");
  }

  // Sync with table checkboxes
  syncTableCheckbox(stationId, selectedStations.has(stationId));
  updateSelectedCount();
  updateHiddenInput();
};

// Update station selection from table checkbox
window.updateStationSelection = function (checkbox) {
  const stationId = checkbox.dataset.stationId;

  if (checkbox.checked) {
    selectedStations.add(stationId);
  } else {
    selectedStations.delete(stationId);
  }

  // Sync with map marker
  syncMapMarker(stationId, checkbox.checked);
  // Update region checkbox state
  updateRegionCheckbox(checkbox.dataset.region);
  updateSelectedCount();
  updateHiddenInput();
};

// Toggle all stations in a region
window.toggleRegion = function (regionCheckbox) {
  const region = regionCheckbox.dataset.region;
  const isChecked = regionCheckbox.checked;

  // Get all station checkboxes in this region
  const stationCheckboxes = document.querySelectorAll(
    `.station-checkbox[data-region="${region}"]`,
  );

  stationCheckboxes.forEach((checkbox) => {
    checkbox.checked = isChecked;
    const stationId = checkbox.dataset.stationId;

    if (isChecked) {
      selectedStations.add(stationId);
    } else {
      selectedStations.delete(stationId);
    }

    // Sync with map
    syncMapMarker(stationId, isChecked);
  });

  updateSelectedCount();
  updateHiddenInput();
};

// Select all stations
window.selectAllStations = function () {
  document.querySelectorAll(".station-checkbox").forEach((checkbox) => {
    checkbox.checked = true;
    selectedStations.add(checkbox.dataset.stationId);
  });

  document.querySelectorAll(".region-checkbox").forEach((checkbox) => {
    checkbox.checked = true;
  });

  document.querySelectorAll(".station-marker").forEach((marker) => {
    marker.classList.add("selected");
  });

  updateSelectedCount();
  updateHiddenInput();
};

// Clear all selections
window.clearAllStations = function () {
  selectedStations.clear();

  document.querySelectorAll(".station-checkbox").forEach((checkbox) => {
    checkbox.checked = false;
  });

  document.querySelectorAll(".region-checkbox").forEach((checkbox) => {
    checkbox.checked = false;
  });

  document.querySelectorAll(".station-marker").forEach((marker) => {
    marker.classList.remove("selected");
  });

  updateSelectedCount();
  updateHiddenInput();
};

// Sync table checkbox with map selection
function syncTableCheckbox(stationId, isSelected) {
  const checkbox = document.querySelector(
    `.station-checkbox[data-station-id="${stationId}"]`,
  );
  if (checkbox) {
    checkbox.checked = isSelected;
    updateRegionCheckbox(checkbox.dataset.region);
  }
}

// Sync map marker with table selection
function syncMapMarker(stationId, isSelected) {
  const marker = document.querySelector(
    `.station-marker[data-station-id="${stationId}"]`,
  );
  if (marker) {
    if (isSelected) {
      marker.classList.add("selected");
    } else {
      marker.classList.remove("selected");
    }
  }
}

// Update region checkbox based on station selections
function updateRegionCheckbox(region) {
  const regionCheckbox = document.querySelector(
    `.region-checkbox[data-region="${region}"]`,
  );
  if (!regionCheckbox) return;

  const stationCheckboxes = document.querySelectorAll(
    `.station-checkbox[data-region="${region}"]`,
  );

  const allChecked = Array.from(stationCheckboxes).every((cb) => cb.checked);
  const someChecked = Array.from(stationCheckboxes).some((cb) => cb.checked);

  regionCheckbox.checked = allChecked;
  regionCheckbox.indeterminate = someChecked && !allChecked;
}

// Update selected count display
function updateSelectedCount() {
  const countEl = document.getElementById("selected-count");
  if (countEl) {
    const count = selectedStations.size;
    countEl.textContent = `${count} selected`;

    // Update style based on count
    countEl.classList.remove("is-info", "is-success", "is-warning");
    if (count === 0) {
      countEl.classList.add("is-info");
    } else if (count <= 5) {
      countEl.classList.add("is-success");
    } else {
      countEl.classList.add("is-warning");
    }
  }
}

// Update hidden inputs for form submission
// Creates multiple hidden inputs with name="locations" for each selected station
function updateHiddenInput() {
  const container = document.getElementById("locations-container");
  if (!container) return;

  // Clear existing hidden inputs
  container.innerHTML = "";

  // Create a hidden input for each selected station
  selectedStations.forEach((stationId) => {
    const input = document.createElement("input");
    input.type = "hidden";
    input.name = "locations";
    input.value = stationId;
    container.appendChild(input);
  });
}

// ============================================
// Map Zoom/Pan Functions
// ============================================

function applyMapTransform() {
  const mapContent = document.querySelector(".map-zoomable");
  if (!mapContent) return;

  mapContent.style.transform = `translate(${mapTranslateX}px, ${mapTranslateY}px) scale(${mapScale})`;

  // Scale markers inversely so they stay a consistent visual size
  const markers = document.querySelectorAll(".station-marker");
  const inverseScale = 1 / mapScale;
  markers.forEach((marker) => {
    marker.style.transform = `scale(${inverseScale})`;
  });
}

function constrainPan() {
  const mapWrapper = document.querySelector(".map-wrapper");
  if (!mapWrapper) return;

  const rect = mapWrapper.getBoundingClientRect();
  const scaledWidth = rect.width * mapScale;
  const scaledHeight = rect.height * mapScale;

  // Calculate max pan values to keep map in view
  const maxPanX = (scaledWidth - rect.width) / 2;
  const maxPanY = (scaledHeight - rect.height) / 2;

  // Constrain translation
  if (mapScale <= 1) {
    mapTranslateX = 0;
    mapTranslateY = 0;
  } else {
    mapTranslateX = Math.max(-maxPanX, Math.min(maxPanX, mapTranslateX));
    mapTranslateY = Math.max(-maxPanY, Math.min(maxPanY, mapTranslateY));
  }
}

window.handleMapWheel = function (event) {
  event.preventDefault();

  const mapWrapper = document.querySelector(".map-wrapper");
  if (!mapWrapper) return;

  const rect = mapWrapper.getBoundingClientRect();

  // Get mouse position relative to map center
  const mouseX = event.clientX - rect.left - rect.width / 2;
  const mouseY = event.clientY - rect.top - rect.height / 2;

  // Calculate zoom
  const delta = -event.deltaY * ZOOM_SENSITIVITY;
  const newScale = Math.max(
    MIN_SCALE,
    Math.min(MAX_SCALE, mapScale * (1 + delta)),
  );

  if (newScale !== mapScale) {
    // Adjust translation to zoom toward mouse position
    const scaleRatio = newScale / mapScale;
    mapTranslateX = mouseX - (mouseX - mapTranslateX) * scaleRatio;
    mapTranslateY = mouseY - (mouseY - mapTranslateY) * scaleRatio;

    mapScale = newScale;
    constrainPan();
    applyMapTransform();
    updateZoomDisplay();
  }
};

window.handleMapMouseDown = function (event) {
  // Only start pan on left click and not on markers
  if (event.button !== 0) return;
  if (event.target.classList.contains("station-marker")) return;

  isPanning = true;
  panStartX = event.clientX - mapTranslateX;
  panStartY = event.clientY - mapTranslateY;

  const mapWrapper = document.querySelector(".map-wrapper");
  if (mapWrapper) {
    mapWrapper.classList.add("is-panning");
  }

  event.preventDefault();
};

window.handleMapMouseMove = function (event) {
  if (!isPanning) return;

  mapTranslateX = event.clientX - panStartX;
  mapTranslateY = event.clientY - panStartY;

  constrainPan();
  applyMapTransform();
};

window.handleMapMouseUp = function () {
  isPanning = false;

  const mapWrapper = document.querySelector(".map-wrapper");
  if (mapWrapper) {
    mapWrapper.classList.remove("is-panning");
  }
};

window.zoomIn = function () {
  mapScale = Math.min(MAX_SCALE, mapScale * 1.5);
  constrainPan();
  applyMapTransform();
  updateZoomDisplay();
};

window.zoomOut = function () {
  mapScale = Math.max(MIN_SCALE, mapScale / 1.5);
  constrainPan();
  applyMapTransform();
  updateZoomDisplay();
};

window.resetZoom = function () {
  mapScale = 1;
  mapTranslateX = 0;
  mapTranslateY = 0;
  applyMapTransform();
  updateZoomDisplay();
};

function updateZoomDisplay() {
  const zoomLevel = document.getElementById("zoom-level");
  if (zoomLevel) {
    zoomLevel.textContent = `${Math.round(mapScale * 100)}%`;
  }
}

// ============================================
// Station Popup with Forecast Grid (fetched from Oracle API)
// ============================================

// Cache fetched forecast data per station to avoid re-fetching on every hover
const forecastCache = {};
let popupHideTimer = null;
let currentPopupMarker = null;

document.addEventListener("mouseover", function (e) {
  if (e.target.classList.contains("station-marker")) {
    clearTimeout(popupHideTimer);
    showStationPopup(e.target);
  }
  // Keep popup visible when hovering the popup itself
  const popup = document.getElementById("station-popup");
  if (popup && popup.contains(e.target)) {
    clearTimeout(popupHideTimer);
  }
});

document.addEventListener("mouseout", function (e) {
  if (e.target.classList.contains("station-marker")) {
    popupHideTimer = setTimeout(hideStationPopup, 200);
  }
  const popup = document.getElementById("station-popup");
  if (popup && popup.contains(e.target)) {
    popupHideTimer = setTimeout(hideStationPopup, 200);
  }
});

function showStationPopup(marker) {
  const popup = document.getElementById("station-popup");
  if (!popup) return;

  currentPopupMarker = marker;

  const stationId = marker.dataset.stationId;
  const stationName = marker.dataset.stationName;
  const state = marker.dataset.state || "";
  const iata = marker.dataset.iata || "";

  // Populate popup header
  popup.querySelector(".popup-station-id").textContent = stationId;
  const iataEl = popup.querySelector(".popup-iata");
  if (iata) {
    iataEl.textContent = iata;
    iataEl.style.display = "inline-block";
  } else {
    iataEl.style.display = "none";
  }

  const nameText = [stationName, state].filter(Boolean).join(", ");
  popup.querySelector(".popup-name").textContent = nameText;

  // Position popup near marker
  const mapWrapper = document.querySelector(".map-wrapper");
  const mapRect = mapWrapper.getBoundingClientRect();
  const markerRect = marker.getBoundingClientRect();

  const popupWidth = 360;
  const popupHeight = 280;

  let left = markerRect.left - mapRect.left + markerRect.width / 2;
  let top = markerRect.top - mapRect.top - 10;

  // Adjust horizontal to stay in bounds
  if (left + popupWidth / 2 > mapRect.width) {
    left = mapRect.width - popupWidth / 2 - 10;
  }
  if (left - popupWidth / 2 < 0) {
    left = popupWidth / 2 + 10;
  }

  // Position above marker, or below if too close to top
  if (top < popupHeight) {
    top = markerRect.top - mapRect.top + markerRect.height + 10;
  } else {
    top = top - popupHeight;
  }

  popup.style.left = `${left}px`;
  popup.style.top = `${top}px`;
  popup.style.transform = "translateX(-50%)";
  popup.style.display = "block";

  // Use cached data or fetch from oracle
  if (forecastCache[stationId]) {
    applyForecastData(popup, forecastCache[stationId]);
  } else {
    // Reset values to loading state
    popup.querySelectorAll("[data-field]").forEach((el) => {
      el.textContent = "-";
    });
    fetchStationForecast(stationId, popup);
  }
}

function hideStationPopup() {
  const popup = document.getElementById("station-popup");
  if (popup) {
    popup.style.display = "none";
  }
  currentPopupMarker = null;
}

// Fetch forecast data from oracle API
async function fetchStationForecast(stationId, popup) {
  const loadingEl = popup.querySelector(".popup-loading");
  if (loadingEl) loadingEl.style.display = "block";

  try {
    const oracleBase = typeof ORACLE_BASE !== "undefined" ? ORACLE_BASE : "";

    const today = new Date();
    const yesterday = new Date(today);
    yesterday.setDate(yesterday.getDate() - 1);
    const dayAfterTomorrow = new Date(today);
    dayAfterTomorrow.setDate(dayAfterTomorrow.getDate() + 2);

    const startDate = yesterday.toISOString();
    const endDate = dayAfterTomorrow.toISOString();

    const [forecastRes, obsRes] = await Promise.all([
      fetch(
        `${oracleBase}/stations/forecasts?station_ids=${stationId}&start=${encodeURIComponent(startDate)}&end=${encodeURIComponent(endDate)}`,
      ),
      fetch(
        `${oracleBase}/stations/observations?station_ids=${stationId}&start=${encodeURIComponent(startDate)}&end=${encodeURIComponent(endDate)}`,
      ),
    ]);

    const forecasts = forecastRes.ok ? await forecastRes.json() : [];
    const observations = obsRes.ok ? await obsRes.json() : [];

    const data = { forecasts, observations };
    forecastCache[stationId] = data;
    applyForecastData(popup, data);
  } catch (err) {
    console.error("Error fetching forecast:", err);
    popup.querySelectorAll("[data-field]").forEach((el) => {
      el.textContent = "?";
    });
  } finally {
    if (loadingEl) loadingEl.style.display = "none";
  }
}

// Apply cached or freshly fetched forecast data to the popup
function applyForecastData(popup, data) {
  const { forecasts, observations } = data;

  const today = new Date();
  const yesterday = new Date(today);
  yesterday.setDate(yesterday.getDate() - 1);
  const tomorrow = new Date(today);
  tomorrow.setDate(tomorrow.getDate() + 1);

  const formatDateKey = (d) => d.toISOString().split("T")[0];
  const yesterdayKey = formatDateKey(yesterday);
  const todayKey = formatDateKey(today);
  const tomorrowKey = formatDateKey(tomorrow);

  // Index by date
  const forecastByDate = {};
  forecasts.forEach((f) => {
    forecastByDate[f.date] = f;
  });

  const obsByDate = {};
  observations.forEach((o) => {
    const date = o.date || (o.start_time ? o.start_time.split("T")[0] : null);
    if (date) obsByDate[date] = o;
  });

  // Formatters
  const formatTemp = (high, low) => {
    if (high != null && low != null)
      return `${Math.round(high)}° / ${Math.round(low)}°`;
    if (high != null) return `${Math.round(high)}°`;
    if (low != null) return `${Math.round(low)}°`;
    return null;
  };
  const formatWind = (speed) =>
    speed != null ? `${Math.round(speed)} mph` : null;
  const formatPrecip = (chance) => (chance != null ? `${chance}%` : null);
  const formatAmount = (amount) =>
    amount != null && amount > 0 ? `${amount.toFixed(2)}"` : null;
  const formatHumidity = (max, min) => {
    if (max != null && min != null) return `${min}-${max}%`;
    if (max != null) return `${max}%`;
    if (min != null) return `${min}%`;
    return null;
  };

  const setValue = (field, value) => {
    const el = popup.querySelector(`[data-field="${field}"]`);
    if (el) el.textContent = value || "-";
  };

  // Yesterday
  const yObs = obsByDate[yesterdayKey];
  const yFc = forecastByDate[yesterdayKey];
  setValue(
    "yesterday-temp-actual",
    yObs ? formatTemp(yObs.temp_high, yObs.temp_low) : null,
  );
  setValue(
    "yesterday-temp-forecast",
    yFc ? formatTemp(yFc.temp_high, yFc.temp_low) : null,
  );
  setValue(
    "yesterday-wind",
    yObs
      ? formatWind(yObs.wind_speed)
      : yFc
        ? formatWind(yFc.wind_speed)
        : null,
  );
  setValue(
    "yesterday-precip-chance",
    yFc ? formatPrecip(yFc.precip_chance) : null,
  );
  setValue(
    "yesterday-rain",
    yObs
      ? formatAmount(yObs.rain_amt)
      : yFc
        ? formatAmount(yFc.rain_amt)
        : null,
  );
  setValue(
    "yesterday-snow",
    yObs
      ? formatAmount(yObs.snow_amt)
      : yFc
        ? formatAmount(yFc.snow_amt)
        : null,
  );
  setValue(
    "yesterday-humidity",
    yObs
      ? formatHumidity(yObs.humidity, yObs.humidity)
      : yFc
        ? formatHumidity(yFc.humidity_max, yFc.humidity_min)
        : null,
  );

  // Today
  const tObs = obsByDate[todayKey];
  const tFc = forecastByDate[todayKey];
  setValue(
    "today-temp-actual",
    tObs ? formatTemp(tObs.temp_high, tObs.temp_low) : null,
  );
  setValue(
    "today-temp-forecast",
    tFc ? formatTemp(tFc.temp_high, tFc.temp_low) : null,
  );
  setValue(
    "today-wind",
    tObs
      ? formatWind(tObs.wind_speed)
      : tFc
        ? formatWind(tFc.wind_speed)
        : null,
  );
  setValue("today-precip-chance", tFc ? formatPrecip(tFc.precip_chance) : null);
  setValue(
    "today-rain",
    tObs
      ? formatAmount(tObs.rain_amt)
      : tFc
        ? formatAmount(tFc.rain_amt)
        : null,
  );
  setValue(
    "today-snow",
    tObs
      ? formatAmount(tObs.snow_amt)
      : tFc
        ? formatAmount(tFc.snow_amt)
        : null,
  );
  setValue(
    "today-humidity",
    tObs
      ? formatHumidity(tObs.humidity, tObs.humidity)
      : tFc
        ? formatHumidity(tFc.humidity_max, tFc.humidity_min)
        : null,
  );

  // Tomorrow
  const tmFc = forecastByDate[tomorrowKey];
  setValue(
    "tomorrow-temp-forecast",
    tmFc ? formatTemp(tmFc.temp_high, tmFc.temp_low) : null,
  );
  setValue("tomorrow-wind", tmFc ? formatWind(tmFc.wind_speed) : null);
  setValue(
    "tomorrow-precip-chance",
    tmFc ? formatPrecip(tmFc.precip_chance) : null,
  );
  setValue("tomorrow-rain", tmFc ? formatAmount(tmFc.rain_amt) : null);
  setValue("tomorrow-snow", tmFc ? formatAmount(tmFc.snow_amt) : null);
  setValue(
    "tomorrow-humidity",
    tmFc ? formatHumidity(tmFc.humidity_max, tmFc.humidity_min) : null,
  );
}

// ============================================
// Initialization
// ============================================

function initMapZoom() {
  const mapWrapper = document.querySelector(".map-wrapper");
  if (!mapWrapper) return;

  // Mouse wheel zoom
  mapWrapper.addEventListener("wheel", handleMapWheel, { passive: false });

  // Pan with mouse drag
  mapWrapper.addEventListener("mousedown", handleMapMouseDown);
  document.addEventListener("mousemove", handleMapMouseMove);
  document.addEventListener("mouseup", handleMapMouseUp);

  // Prevent context menu on map
  mapWrapper.addEventListener("contextmenu", (e) => e.preventDefault());

  // Reset zoom state
  mapScale = 1;
  mapTranslateX = 0;
  mapTranslateY = 0;
  updateZoomDisplay();
}

// Initialize view preference on page load
document.addEventListener("DOMContentLoaded", function () {
  const savedView = localStorage.getItem("locationView") || "map";
  const mapView = document.getElementById("location-map-view");
  const tableView = document.getElementById("location-table-view");

  if (mapView && tableView) {
    switchLocationView(savedView);
  }

  initMapZoom();
});

// Re-initialize after HTMX swaps
document.addEventListener("htmx:afterSwap", function (e) {
  if (
    e.target.querySelector(".location-map-container") ||
    e.target.querySelector(".location-table-container")
  ) {
    const savedView = localStorage.getItem("locationView") || "map";
    switchLocationView(savedView);
    initMapZoom();
  }
});

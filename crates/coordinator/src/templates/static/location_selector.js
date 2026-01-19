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
// Tooltip with Weather Data (SSR - read from data attributes)
// ============================================

document.addEventListener("mouseover", function (e) {
  if (e.target.classList.contains("station-marker")) {
    showTooltip(e.target);
  }
});

document.addEventListener("mouseout", function (e) {
  if (e.target.classList.contains("station-marker")) {
    hideTooltip();
  }
});

function showTooltip(marker) {
  const tooltip = document.getElementById("station-tooltip");
  if (!tooltip) return;

  const stationId = marker.dataset.stationId;
  const stationName = marker.dataset.stationName;

  // Read weather data from data attributes (SSR)
  const todayActualHigh = marker.dataset.todayActualHigh;
  const todayActualLow = marker.dataset.todayActualLow;
  const todayForecastHigh = marker.dataset.todayForecastHigh;
  const todayForecastLow = marker.dataset.todayForecastLow;
  const tomorrowForecastHigh = marker.dataset.tomorrowForecastHigh;
  const tomorrowForecastLow = marker.dataset.tomorrowForecastLow;

  // Update basic info
  tooltip.querySelector(".tooltip-station-id").textContent = stationId;
  tooltip.querySelector(".tooltip-name").textContent = stationName;

  // Build weather HTML from SSR data
  const weatherSection = tooltip.querySelector(".tooltip-weather");
  if (weatherSection) {
    let html = "";

    // Today's weather - actual and forecast
    html += '<div class="tooltip-weather-day">';
    html += "<strong>Today</strong>";

    // Actual (current) weather
    if (todayActualHigh || todayActualLow) {
      const high = todayActualHigh ? `${todayActualHigh}°` : "-";
      const low = todayActualLow ? `${todayActualLow}°` : "-";
      html += `<div class="weather-temp">${high} / ${low}</div>`;
    } else {
      html += '<div class="weather-temp has-text-grey">-</div>';
    }

    // Forecast for today
    if (todayForecastHigh || todayForecastLow) {
      const high = todayForecastHigh ? `${todayForecastHigh}°` : "-";
      const low = todayForecastLow ? `${todayForecastLow}°` : "-";
      html += `<div class="weather-forecast">${high} / ${low} <span class="forecast-label">fcst</span></div>`;
    }
    html += "</div>";

    // Tomorrow's forecast
    html += '<div class="tooltip-weather-day">';
    html += "<strong>Tomorrow</strong>";
    if (tomorrowForecastHigh || tomorrowForecastLow) {
      const high = tomorrowForecastHigh ? `${tomorrowForecastHigh}°` : "-";
      const low = tomorrowForecastLow ? `${tomorrowForecastLow}°` : "-";
      html += `<div class="weather-temp">${high} / ${low}</div>`;
    } else {
      html += '<div class="weather-temp has-text-grey">No forecast</div>';
    }
    html += "</div>";

    weatherSection.innerHTML = html;
  }

  // Position tooltip to the right of the marker
  const mapWrapper = document.querySelector(".map-wrapper");
  const mapRect = mapWrapper.getBoundingClientRect();
  const markerRect = marker.getBoundingClientRect();

  const tooltipWidth = 220;
  const tooltipHeight = 120;
  const offset = 15; // gap between marker and tooltip

  let left = markerRect.right - mapRect.left + offset;
  let top =
    markerRect.top - mapRect.top + markerRect.height / 2 - tooltipHeight / 2;

  // If tooltip would go off the right edge, position to the left of marker instead
  if (left + tooltipWidth > mapRect.width) {
    left = markerRect.left - mapRect.left - tooltipWidth - offset;
  }

  // Keep tooltip within vertical bounds
  if (top < 10) {
    top = 10;
  }
  if (top + tooltipHeight > mapRect.height - 10) {
    top = mapRect.height - tooltipHeight - 10;
  }

  tooltip.style.left = `${left}px`;
  tooltip.style.top = `${top}px`;
  tooltip.style.transform = "none";
  tooltip.style.display = "block";
}

function hideTooltip() {
  const tooltip = document.getElementById("station-tooltip");
  if (tooltip) {
    tooltip.style.display = "none";
  }
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

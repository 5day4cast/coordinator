// Entries Map - zoom/pan and click-to-show-weather-popup

(function () {
  var scale = 1;
  var translateX = 0;
  var translateY = 0;
  var isPanning = false;
  var panStartX = 0;
  var panStartY = 0;
  var currentStationId = null;

  var MIN_SCALE = 1;
  var MAX_SCALE = 5;
  var ZOOM_SENSITIVITY = 0.001;

  function getWrapper() {
    return document.getElementById("entries-map-wrapper");
  }

  function getZoomable() {
    return document.getElementById("entries-map-zoomable");
  }

  function getPopup() {
    return document.getElementById("entries-station-popup");
  }

  function applyTransform() {
    var el = getZoomable();
    if (el) {
      el.style.transform =
        "translate(" +
        translateX +
        "px, " +
        translateY +
        "px) scale(" +
        scale +
        ")";
    }
    var label = document.getElementById("entries-zoom-level");
    if (label) {
      label.textContent = Math.round(scale * 100) + "%";
    }
  }

  function constrainPan() {
    var wrapper = getWrapper();
    if (!wrapper) return;
    var rect = wrapper.getBoundingClientRect();
    var maxX = ((scale - 1) * rect.width) / 2;
    var maxY = ((scale - 1) * rect.height) / 2;
    translateX = Math.max(-maxX, Math.min(maxX, translateX));
    translateY = Math.max(-maxY, Math.min(maxY, translateY));
  }

  // Zoom
  window.entriesMapZoomIn = function () {
    scale = Math.min(MAX_SCALE, scale + 0.5);
    constrainPan();
    applyTransform();
  };

  window.entriesMapZoomOut = function () {
    scale = Math.max(MIN_SCALE, scale - 0.5);
    constrainPan();
    applyTransform();
  };

  window.entriesMapResetZoom = function () {
    scale = 1;
    translateX = 0;
    translateY = 0;
    applyTransform();
  };

  // Scroll wheel zoom
  function handleWheel(e) {
    e.preventDefault();
    var delta = -e.deltaY * ZOOM_SENSITIVITY;
    scale = Math.max(MIN_SCALE, Math.min(MAX_SCALE, scale + delta));
    constrainPan();
    applyTransform();
  }

  // Pan
  function handleMouseDown(e) {
    if (scale <= 1) return;
    isPanning = true;
    panStartX = e.clientX - translateX;
    panStartY = e.clientY - translateY;
    var wrapper = getWrapper();
    if (wrapper) wrapper.classList.add("is-panning");
    e.preventDefault();
  }

  function handleMouseMove(e) {
    if (!isPanning) return;
    translateX = e.clientX - panStartX;
    translateY = e.clientY - panStartY;
    constrainPan();
    applyTransform();
  }

  function handleMouseUp() {
    isPanning = false;
    var wrapper = getWrapper();
    if (wrapper) wrapper.classList.remove("is-panning");
  }

  // Touch pan
  function handleTouchStart(e) {
    if (scale <= 1 || e.touches.length !== 1) return;
    isPanning = true;
    panStartX = e.touches[0].clientX - translateX;
    panStartY = e.touches[0].clientY - translateY;
  }

  function handleTouchMove(e) {
    if (!isPanning || e.touches.length !== 1) return;
    e.preventDefault();
    translateX = e.touches[0].clientX - panStartX;
    translateY = e.touches[0].clientY - panStartY;
    constrainPan();
    applyTransform();
  }

  function handleTouchEnd() {
    isPanning = false;
  }

  // Show weather popup on station click
  window.showStationWeather = function (marker, stationId) {
    var popup = getPopup();
    if (!popup) return;

    currentStationId = stationId;

    var stationName = marker.dataset.stationName || "";
    var state = marker.dataset.state || "";

    popup.querySelector(".popup-station-id").textContent = stationId;
    popup.querySelector(".popup-name").textContent =
      stationName + (state ? ", " + state : "");

    // Populate weather grid
    var forecastHigh = marker.dataset.forecastHigh;
    var forecastLow = marker.dataset.forecastLow;
    var actualHigh = marker.dataset.actualHigh;
    var actualLow = marker.dataset.actualLow;
    var wind = marker.dataset.wind;

    setField(popup, "forecast-high", forecastHigh, "\u00B0F");
    setField(popup, "forecast-low", forecastLow, "\u00B0F");
    setField(popup, "actual-high", actualHigh, "\u00B0F");
    setField(popup, "actual-low", actualLow, "\u00B0F");
    setField(popup, "wind", wind, " mph");

    // Position popup near the marker
    var wrapper = getWrapper();
    if (!wrapper) return;
    var wrapperRect = wrapper.getBoundingClientRect();
    var markerRect = marker.getBoundingClientRect();

    var left = markerRect.left - wrapperRect.left + markerRect.width / 2;
    var top = markerRect.top - wrapperRect.top - 10;

    popup.style.left = left + "px";
    popup.style.top = top + "px";
    popup.style.transform = "translate(-50%, -100%)";
    popup.style.display = "block";
  };

  function setField(popup, field, value, suffix) {
    var el = popup.querySelector('[data-field="' + field + '"]');
    if (el) {
      el.textContent = value ? value + suffix : "-";
    }
  }

  window.hideStationPopup = function () {
    var popup = getPopup();
    if (popup) popup.style.display = "none";
    currentStationId = null;
  };

  // "Go to picks" button in popup
  window.scrollToStationFromPopup = function () {
    if (currentStationId) {
      scrollToStation(currentStationId);
      hideStationPopup();
    }
  };

  // Click station marker -> scroll to station picks
  window.scrollToStation = function (stationId) {
    var stationBox = document.querySelector(
      '[data-station="' + stationId + '"]',
    );
    if (stationBox) {
      stationBox.scrollIntoView({ behavior: "smooth", block: "center" });
      // Briefly highlight
      stationBox.style.transition = "box-shadow 0.3s ease";
      stationBox.style.boxShadow = "0 0 0 3px #485fc7";
      setTimeout(function () {
        stationBox.style.boxShadow = "";
      }, 1500);
    }
  };

  // Close popup when clicking outside
  function handleDocumentClick(e) {
    var popup = getPopup();
    if (!popup || popup.style.display === "none") return;
    // Don't close if clicking on a marker or inside the popup
    if (
      e.target.closest(".station-marker") ||
      e.target.closest(".station-popup")
    )
      return;
    hideStationPopup();
  }

  // Setup on load
  function setup() {
    var wrapper = getWrapper();
    if (!wrapper) return;

    wrapper.addEventListener("wheel", handleWheel, { passive: false });
    wrapper.addEventListener("mousedown", handleMouseDown);
    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);
    wrapper.addEventListener("touchstart", handleTouchStart, {
      passive: true,
    });
    wrapper.addEventListener("touchmove", handleTouchMove, { passive: false });
    wrapper.addEventListener("touchend", handleTouchEnd);

    document.addEventListener("click", handleDocumentClick);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", setup);
  } else {
    setup();
  }

  // Re-setup after HTMX content swaps (for SPA navigation)
  document.addEventListener("htmx:afterSettle", function () {
    setup();
  });
})();

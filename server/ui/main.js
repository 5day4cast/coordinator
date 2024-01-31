import { submitDownloadRequest } from "./data_access.js";
import { COMPETITIONS, displayCompetitions } from "./competitions.js";
import { hideAllContainers, showContainer } from "./navbar.js";

const apiBase = API_BASE;
console.log("api location:", apiBase);

// Download last 4 hour's files on initial load
submitDownloadRequest();

displayCompetitions(COMPETITIONS);
import { queryDb } from './data_access.js';
import { displayLeaderboard } from './leader_board.js';

let currentMaps = {};

export function displayCompetitions() {
    let $competitionsDataTable = document.getElementById("competitionsDataTable");
    let $tbody = $competitionsDataTable.querySelector("tbody");
    if (!$tbody) {
        $tbody = document.createElement("tbody");
        $competitionsDataTable.appendChild($tbody);
    }
    const competitions = get_competitions(); //TODO: will be async to a backend
    competitions.forEach(competition => {
        let $row = document.createElement("tr");

        // Exclude the "cities" property
        Object.keys(competition).forEach(key => {
            if (key !== "cities" && key !== "id") {
                const cell = document.createElement("td");
                cell.textContent = competition[key];
                $row.appendChild(cell);
            }
        });

        $row.addEventListener("click", () => {
            handleCompetitionClick($row, competition);
        });

        $tbody.appendChild($row);
    });

}

function get_competitions() {
    var date = new Date();
    const oneHoursAgoUTCDate = new Date(date.getTime() - (1 * 3600 * 1000));
    const twoHoursAgoUTCDate = new Date(date.getTime() - (2 * 3600 * 1000));
    const oneHourFromNowUTCDate = new Date(date.getTime() + (1 * 3600 * 1000));
    const twelveHoursFromNowUTCDate = new Date(date.getTime() + (12 * 3600 * 1000));

    const rfc3339TimetwelveHoursFromNow = twelveHoursFromNowUTCDate.toISOString();
    const rfc3339TwoHoursAgoUTCDate = twoHoursAgoUTCDate.toISOString();
    const rfc3339TimeOneHourFromNow = oneHourFromNowUTCDate.toISOString();
    const rfc3339TimeOneHourAgo = oneHoursAgoUTCDate.toISOString();

    //NOTE: for real competitions we should be doing this over 12 or 24 hour windows
    //observation reports don't always get created every hour for every station's value
    return [
        {
            "id": "671657f5-a437-453e-b9fa-4c50705dc607",
            "name": "Tiger Roar Challenge",
            "startTime": rfc3339TimeOneHourFromNow,
            "endTime": rfc3339TimetwelveHoursFromNow,
            "status": "live",
            "totalPrizePoolAmt": "$60",
            "totalEntries": 30,
            "cities": ["KGRB", "KBOI", "KRAP", "KJAN", "KPWN"]
        },
        {
            "id": "70bc176c-4b30-46c0-8720-b1535d15ba34",
            "name": "Unicorn Gallop Grand Prix",
            "startTime": rfc3339TwoHoursAgoUTCDate,
            "endTime": rfc3339TimeOneHourFromNow,
            "status": "running",
            "totalPrizePoolAmt": "$20",
            "totalEntries": 10,
            "cities": ["KTPA", "KMIA", "KATL", "KSDF", "KBNA"]
        },
        {
            "id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "name": "Gryphon's Claws Tournament",
            "startTime": rfc3339TwoHoursAgoUTCDate,
            "endTime": rfc3339TimeOneHourAgo,
            "status": "completed",
            "totalPrizePoolAmt": "$16",
            "totalEntries": 8,
            "cities": ["KBFM", "KBHM", "KMSY", "KLIT", "KMCI"]
        },
        {
            "id": "57bd5d1e-a7ae-422e-8673-81ebb6227bf8",
            "name": "Mermaid's Song Showcase",
            "startTime": rfc3339TimeOneHourFromNow,
            "endTime": rfc3339TimetwelveHoursFromNow,
            "status": "live",
            "totalPrizePoolAmt": "$60",
            "totalEntries": 30,
            "cities": ["KSTL", "KCID", "KMSP", "KABQ", "KTUL"]
        },
        {
            "id": "cdf5b892-8d21-4264-ab65-9bc3e80e535d",
            "name": "Chimera Chase Extravaganza",
            "startTime": rfc3339TimeOneHourFromNow,
            "endTime": rfc3339TimetwelveHoursFromNow,
            "status": "live",
            "totalPrizePoolAmt": "$20",
            "totalEntries": 10,
            "cities": ["KBOS", "KSEA", "KDEN", "KIND", "KCLT"]
        },
    ];
}

function handleCompetitionClick(row, competition) {
    console.log(row);
    const parentElement = row.parentElement;
    const rows = parentElement.querySelectorAll("tr");
    rows.forEach(currentRow => {
        if (currentRow != row) {
            currentRow.classList.remove('is-selected');
        }
    });
    row.classList.toggle('is-selected');
    let rowIsSelected = row.classList.contains('is-selected');
    if (competition['status'] == 'live') {
        makeCompetitionMap(competition).then(result => {
            showCurrentCompetition(rowIsSelected);
        }).catch(error => {
            console.error(error);
        });
    } else {
        hideCurrentCompetition();
        displayLeaderboard(competition, rowIsSelected).then(result => {
            console.log("leaderboard displayed");
        }).catch(error => {
            console.error(error);
        })
    }
}

function showCurrentCompetition(isSelected) {
    let $currentCompetitionCurrent = document.getElementById("currentCompetition");
    if (!isSelected) {
        console.log('is not selected');
        $currentCompetitionCurrent.classList.add('hidden');
        return
    }
    $currentCompetitionCurrent.classList.remove('hidden');
}

function hideCurrentCompetition() {
    let $currentCompetitionCurrent = document.getElementById("currentCompetition");
    $currentCompetitionCurrent.classList.add('hidden');
}

async function makeCompetitionMap(competition) {

    console.log(competition);
    let oldMap = currentMaps["map"]; // Retrieve map instance by div ID
    console.log(oldMap);
    if (oldMap !== undefined) {
        oldMap.remove();
    }
    console.log("creating map");
    const map = L.map('map', { dragging: false, trackResize: true }).setView([39.8283, -98.5795], 4.4); // Centered on the US
    L.tileLayer('https://tiles.stadiamaps.com/tiles/stamen_toner_background/{z}/{x}/{y}{r}.{ext}', {
        minZoom: 4,
        maxZoom: 7,
        attribution: '&copy; <a href="https://www.stadiamaps.com/" target="_blank">Stadia Maps</a> &copy; <a href="https://www.stamen.com/" target="_blank">Stamen Design</a> &copy; <a href="https://openmaptiles.org/" target="_blank">OpenMapTiles</a> &copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors',
        ext: 'png',
        maxBounds: [
            [25.84, -124.67], // Southwest coordinates (latitude, longitude)
            [49.38, -66.95]   // Northeast coordinates (latitude, longitude)
        ]
    }).addTo(map);
    const points = await getCompetitionPoints(competition.cities);
    let stations_to_cities = getStations(); //TODO: make async for backend code
    points.forEach(point => {
        let marker = L.circleMarker([point.latitude, point.longitude], {
        }).addTo(map);

        let location_name = stations_to_cities[point.station_id];
        // Extend the pop here
        marker.bindPopup(`${location_name} (${point.station_id})`).openPopup();
    });
    console.log("creating map 2");

    currentMaps['map'] = map;
}

async function getCompetitionPoints(station_ids) {
    const query = `SELECT station_id, latitude, longitude FROM observations WHERE station_id IN ('${station_ids.join('\', \'')}')  GROUP BY station_id, station_name, latitude, longitude;`;
    return queryDb(query);
}
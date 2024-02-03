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
    return [
        {
            "id": "671657f5-a437-453e-b9fa-4c50705dc607",
            "name": "Tiger Roar Challenge",
            "startTime": "2024-02-25T00:00:00Z",
            "endTime": "2024-02-26T00:00:00Z",
            "status": "live",
            "totalPrizePoolAmt": "$60",
            "totalEntries": 30,
            "cities": ["KGRB", "KBOI", "KRAP", "KJAN", "KPWN"]
        },
        {
            "id": "4539963e-80b4-43e1-bd94-bf47c7a665ec",
            "name": "Phoenix Flight Showdown",
            "startTime": "2024-02-25T00:00:00Z",
            "endTime": "2024-02-26T00:00:00Z",
            "status": "completed",
            "totalPrizePoolAmt": "$20",
            "totalEntries": 10,
            "cities": ["KMHT", "KBTV", "KHFD", "KRDU", "KBWI"]
        },
        {
            "id": "626da387-df90-40a1-9f64-1fcf5f13fba3",
            "name": "Dragon's Breath Competition",
            "startTime": "2024-02-25T00:00:00Z",
            "endTime": "2024-02-26T00:00:00Z",
            "status": "running",
            "totalPrizePoolAmt": "$20",
            "totalEntries": 10,
            "cities": ["KEWR", "KBUF", "KPIT", "KCRW", "KCHS"]
        },
        {
            "id": "70bc176c-4b30-46c0-8720-b1535d15ba34",
            "name": "Unicorn Gallop Grand Prix",
            "startTime": "2024-02-24T00:00:00Z",
            "endTime": "2024-02-25T00:00:00Z",
            "status": "running",
            "totalPrizePoolAmt": "$20",
            "totalEntries": 10,
            "cities": ["KTPA", "KMIA", "KATL", "KSDF", "KBNA"]
        },
        {
            "id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "name": "Gryphon's Claws Tournament",
            "startTime": "2024-02-24T00:00:00Z",
            "endTime": "2024-02-25T00:00:00Z",
            "status": "completed",
            "totalPrizePoolAmt": "$16",
            "totalEntries": 8,
            "cities": ["KBFM", "KBHM", "KMSY", "KLIT", "KMCI"]
        },
        {
            "id": "57bd5d1e-a7ae-422e-8673-81ebb6227bf8",
            "name": "Mermaid's Song Showcase",
            "startTime": "2024-02-24T00:00:00Z",
            "endTime": "2024-02-25T00:00:00Z",
            "status": "live",
            "totalPrizePoolAmt": "$60",
            "totalEntries": 30,
            "cities": ["KSTL", "KCID", "KMSP", "KABQ", "KTUL"]
        },
        {
            "id": "12d58c34-d61d-4205-8677-8b8b99502324",
            "name": "Centaur Sprint Invitational",
            "startTime": "2024-02-23T00:00:00Z",
            "endTime": "2024-02-24T00:00:00Z",
            "status": "completed",
            "totalPrizePoolAmt": "$10",
            "totalEntries": 5,
            "cities": ["KICT", "KOMA", "KFSD", "KBIS", "KJAC"]
        },
        {
            "id": "58ee4971-d451-44d3-a072-c328c57af49c",
            "name": "Kraken's Dive Challenge",
            "startTime": "2024-02-23T00:00:00Z",
            "endTime": "2024-02-24T00:00:00Z",
            "status": "running",
            "totalPrizePoolAmt": "$40",
            "totalEntries": 20,
            "cities": ["KGTF", "KPDX", "KIDA", "KJAX", "KLAS"]
        },
        {
            "id": "cdf5b892-8d21-4264-ab65-9bc3e80e535d",
            "name": "Chimera Chase Extravaganza",
            "startTime": "2024-02-23T00:00:00Z",
            "endTime": "2024-02-24T00:00:00Z",
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
        makeCompetitionMap(competition, rowIsSelected).then(result => {
            console.log("map displayed")

        }).catch(error => {
            console.error(error);
        });
    } else {
        displayLeaderboard(competition, rowIsSelected).then(result => {
            console.log("leaderboard displayed");
        }).catch(error => {
            console.error(error);
        })
    }
}

async function makeCompetitionMap(competition, isSelected) {
    let $currentCompetitionCurrent = document.getElementById("currentCompetition");
    if (!isSelected) {
        console.log('is not selected');
        $currentCompetitionCurrent.classList.add('hidden');
        return
    }
    $currentCompetitionCurrent.classList.remove('hidden');

    console.log(competition);
    let oldMap = currentMaps["map"]; // Retrieve map instance by div ID
    console.log(oldMap);
    if (oldMap !== undefined) {
        oldMap.remove();
    }
    console.log("creating map");
    var map = L.map('map', { dragging: false, trackResize: true }).setView([39.8283, -98.5795], 4.5); // Centered on the US
    //NOTE: hitting issue in browser with this tile "NS_BINDING_ABORTED", probably need to download an actual png and use that instead
    L.tileLayer('https://tiles.stadiamaps.com/tiles/stamen_toner_background/{z}/{x}/{y}{r}.{ext}', {
        minZoom: 4.3,
        maxZoom: 7,
        attribution: '&copy; <a href="https://www.stadiamaps.com/" target="_blank">Stadia Maps</a> &copy; <a href="https://www.stamen.com/" target="_blank">Stamen Design</a> &copy; <a href="https://openmaptiles.org/" target="_blank">OpenMapTiles</a> &copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors',
        ext: 'png',
        maxBounds: [
            [25.84, -124.67], // Southwest coordinates (latitude, longitude)
            [49.38, -66.95]   // Northeast coordinates (latitude, longitude)
        ]
    }).addTo(map);

    const points = await getCompetitionPoints(competition.cities);
    let stations_to_cities = get_stations(); //TODO: make async for backend code
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

function get_stations() {
    return {
        "KDCA": "Washington, D.C.",
        "KLGA": "New York City, NY",
        "KLAX": "Los Angeles, CA",
        "KORD": "Chicago, IL",
        "KPHL": "Philadelphia, PA",
        "KIAH": "Houston, TX",
        "KPHX": "Phoenix, AZ",
        "KSJC": "San Jose, CA",
        "KSFO": "San Francisco, CA",
        "KCMH": "Columbus, OH",
        "KDTW": "Detroit, MI",
        "KCLT": "Charlotte, NC",
        "KIND": "Indianapolis, IN",
        "KDEN": "Denver, CO",
        "KSEA": "Seattle, WA",
        "KBOS": "Boston, MA",
        "KLAS": "Las Vegas, NV",
        "KJAX": "Jacksonville, FL",
        "KIDA": "Idaho Falls, ID",
        "KPDX": "Portland, OR",
        "KGTF": "Great Falls, MT",
        "KJAC": "Jackson, WY",
        "KBIS": "Bismarck, ND",
        "KFSD": "Sioux Falls, SD",
        "KOMA": "Omaha, NE",
        "KICT": "Wichita, KS",
        "KTUL": "Tulsa, OK",
        "KABQ": "Albuquerque, NM",
        "KMSP": "Minneapolis, MN",
        "KCID": "Cedar Rapids, IA",
        "KSTL": "St. Louis, MO",
        "KMCI": "Kansas City, MO",
        "KLIT": "Little Rock, AR",
        "KMSY": "New Orleans, LA",
        "KBHM": "Birmingham, AL",
        "KBFM": "Mobile, AL",
        "KBNA": "Nashville, TN",
        "KSDF": "Louisville, KY",
        "KATL": "Atlanta, GA",
        "KMIA": "Miami, FL",
        "KTPA": "Tampa, FL",
        "KCHS": "Charleston, SC",
        "KCRW": "Charleston, WV",
        "KPIT": "Pittsburgh, PA",
        "KBUF": "Buffalo, NY",
        "KEWR": "Newark, NJ",
        "KBWI": "Baltimore, MD",
        "KRDU": "Raleigh, NC",
        "KHFD": "Hartford, CT",
        "KBTV": "Burlington, VT",
        "KMHT": "Manchester, NH",
        "KPWM": "Portland, ME",
        "KJAN": "Jackson, MS",
        "KRAP": "Rapid City, SD",
        "KBOI": "Boise, ID",
        "KGRB": "Green Bay, WI",
    };
}
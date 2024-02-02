import { queryDb } from './data_access.js';

// Define the number of arrays and the maximum number of items per array
const numArrays = 9;
const maxItemsPerArray = 5;
const seed = 123;

// Function to generate a pseudo-random number based on a seed
function seededRandom(seed) {
    let x = Math.sin(seed++) * 10000;
    return x - Math.floor(x);
}
const stations_to_cities = {
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
}

let station_ids = Object.keys(stations_to_cities);
// Shuffle the keys using the seeded random number generator
const shuffledKeys = station_ids.sort(() => seededRandom(seed) - 0.5);

// Split the shuffled keys into arrays of random lengths
const arrays = [];
let startIndex = 0;
for (let i = 0; i < numArrays; i++) {
    const endIndex = Math.min(startIndex + Math.floor(seededRandom(seed) * (maxItemsPerArray - 3)) + 4, station_ids.length);
    arrays.push(shuffledKeys.slice(startIndex, endIndex));
    startIndex = endIndex;
}

console.log(arrays);

let competitions = [
    {
        "name": "Tiger Roar Challenge",
        "startTime": "2024-02-25T00:00:00Z",
        "endTime": "2024-02-26T00:00:00Z",
        "totalPrizePoolAmt": "$50,000",
        "totalEntries": 500,
        "cities": []
    },
    {
        "name": "Phoenix Flight Showdown",
        "startTime": "2024-02-25T00:00:00Z",
        "endTime": "2024-02-26T00:00:00Z",
        "totalPrizePoolAmt": "$100,000",
        "totalEntries": 300,
        "cities": []
    },
    {
        "name": "Dragon's Breath Competition",
        "startTime": "2024-02-25T00:00:00Z",
        "endTime": "2024-02-26T00:00:00Z",
        "totalPrizePoolAmt": "$75,000",
        "totalEntries": 200,
        "cities": []
    },
    {
        "name": "Unicorn Gallop Grand Prix",
        "startTime": "2024-02-24T00:00:00Z",
        "endTime": "2024-02-25T00:00:00Z",
        "totalPrizePoolAmt": "$200,000",
        "totalEntries": 400,
        "cities": []
    },
    {
        "name": "Gryphon's Claws Tournament",
        "startTime": "2024-02-24T00:00:00Z",
        "endTime": "2024-02-25T00:00:00Z",
        "totalPrizePoolAmt": "$150,000",
        "totalEntries": 600,
        "cities": []
    },
    {
        "name": "Mermaid's Song Showcase",
        "startTime": "2024-02-24T00:00:00Z",
        "endTime": "2024-02-25T00:00:00Z",
        "totalPrizePoolAmt": "$300,000",
        "totalEntries": 1000,
        "cities": []
    },
    {
        "name": "Centaur Sprint Invitational",
        "startTime": "2024-02-23T00:00:00Z",
        "endTime": "2024-02-24T00:00:00Z",
        "totalPrizePoolAmt": "$80,000",
        "totalEntries": 150,
        "cities": []
    },
    {
        "name": "Kraken's Dive Challenge",
        "startTime": "2024-02-23T00:00:00Z",
        "endTime": "2024-02-24T00:00:00Z",
        "totalPrizePoolAmt": "$120,000",
        "totalEntries": 400,
        "cities": []
    },
    {
        "name": "Chimera Chase Extravaganza",
        "startTime": "2024-02-23T00:00:00Z",
        "endTime": "2024-02-24T00:00:00Z",
        "totalPrizePoolAmt": "$250,000",
        "totalEntries": 800,
        "cities": []
    },
];


competitions.forEach((competition, index) => {
    competition.cities = arrays[index];
});

let currentMaps = {};


export async function getCompetitionPoints(station_ids) {
    const query = `SELECT station_id, latitude, longitude FROM observations WHERE station_id IN ('${station_ids.join('\', \'')}')  GROUP BY station_id, station_name, latitude, longitude;`;
    return queryDb(query);
}

export function displayCompetitions() {
    let $competitionsDataTable = document.getElementById("competitionsDataTable");

    let $tbody = $competitionsDataTable.querySelector("tbody");
    if (!$tbody) {
        $tbody = document.createElement("tbody");
        $competitionsDataTable.appendChild($tbody);
    }
    competitions.forEach(competition => {
        let $row = document.createElement("tr");

        // Exclude the "cities" property
        Object.keys(competition).forEach(key => {
            if (key !== "cities") {
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
    makeCompetitionMap(competition, rowIsSelected).then(result => {
        console.log("map displayed")
    }).catch(error => {
        console.error(error);
    });
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

    var map = L.map('map', { dragging: false, trackResize: true }).setView([39.8283, -98.5795], 4.5); // Centered on the US
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
    points.forEach(point => {
        let marker = L.circleMarker([point.latitude, point.longitude], {
        }).addTo(map);

        let location_name = stations_to_cities[point.station_id];
        // Extend the pop here
        marker.bindPopup(`${location_name} (${point.station_id})`).openPopup();
    });

    currentMaps['map'] = map;
}
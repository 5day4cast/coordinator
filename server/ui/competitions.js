import { queryDb } from './data_access.js';

// Define the number of arrays and the maximum number of items per array
const numArrays = 9;
const maxItemsPerArray = 10;
const seed = 123;

// Function to generate a pseudo-random number based on a seed
function seededRandom(seed) {
    let x = Math.sin(seed++) * 10000;
    return x - Math.floor(x);
}
const stations_to_cities = {
    "KDCA": "LWX",
    "KLGA": "OKX",
    "KLAX": "LOX",
    "KORD": "LOT",
    "KPHL": "PHI",
    "KIAH": "HGX",
    "KPHX": "PSR",
    "KSJC": "MTR",
    "KSFO": "MTR",
    "KCMH": "ILN",
    "KDTW": "DTX",
    "KCLT": "GSP",
    "KIND": "IND",
    "KDEN": "BOU",
    "KSEA": "SEW",
    "KBOS": "BOX",
    "KLAS": "VEF",
    "KJAX": "JAX",
    "KIDA": "PIH",
    "KPDX": "PQR",
    "KGTF": "TFX",
    "KJAC": "RIW",
    "KBIS": "BIS",
    "KFSD": "FSD",
    "KOMA": "OAX",
    "KICT": "ICT",
    "KTUL": "TSA",
    "KABQ": "ABQ",
    "KMSP": "MPX",
    "KCID": "DVN",
    "KSTL": "LSX",
    "KMCI": "EAX",
    "KLIT": "LZK",
    "KMSY": "LIX",
    "KBHM": "BMX",
    "KBFM": "MOB",
    "KBNA": "OHX",
    "KSDF": "LMK",
    "KATL": "FFC",
    "KMIA": "MFL",
    "KTPA": "TBW",
    "KCHS": "CHS",
    "KCRW": "RLX",
    "KPIT": "PBZ",
    "KBUF": "BUF",
    "KEWR": "OKX",
    "KBWI": "LWX",
    "KRDU": "RAH",
    "KHFD": "BOX",
    "KBTV": "BTV",
    "KMHT": "GYX",
    "KPWM": "GYX",
    "KJAN": "JAN",
    "KRAP": "UNR",
    "KBOI": "BOI",
    "KGRB": "GRB",
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

export const COMPETITIONS = [
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


COMPETITIONS.forEach((competition, index) => {
    competition.cities = arrays[index];
});

let currentMaps = {};


export async function getCompetitionPoints(station_ids) {
    const query = `SELECT latitude, longitude FROM observations WHERE station_id IN ('${station_ids.join('\', \'')}')  GROUP BY station_id, station_name, latitude, longitude;`;
    return queryDb(queryResult);
}

export function displayCompetitions(competitions) {
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
    /*
    this is extra, need a button for them to make an entry
    makeCompetitionMap(competition, rowIsSelected).then(result => {
        console.log("map displayed")
    }).catch(error => {
        console.error(error);
    });*/
}

async function makeCompetitionMap(competition, isSelected) {
    let $currentCompetitionCurrent = document.getElementById("currentCompetition");
    if (!isSelected) {
        $currentCompetitionCurrent.classList.add('hidden');
        return
    }
    $currentCompetitionCurrent.classList.toggle('hidden');

    console.log(competition);
    let oldMap = currentMaps["map"]; // Retrieve map instance by div ID
    if (oldMap !== undefined) {
        oldMap.remove();
    }

    let map = L.map('map').setView([0, 0], 2);
    currentMaps['map'] = map;

    const svgImageUrl = `${API_BASE}/ui/us.svg`;
    console.log(svgImageUrl);

    var bounds = [
        [25.84, -124.67], // Southwest coordinates (latitude, longitude)
        [49.38, -66.95]   // Northeast coordinates (latitude, longitude)
    ];
    map.fitBounds(bounds);
    L.imageOverlay(svgImageUrl, bounds).addTo(map);

    // Create custom SVG marker
    let svgMarker = '<svg width="20" height="20" viewBox="0 0 20 20" fill="none" xmlns="http://www.w3.org/2000/svg"><circle cx="10" cy="10" r="10" fill="red"/><circle cx="10" cy="10" r="6" fill="white"/><circle cx="10" cy="10" r="2" fill="black"/></svg>';

    // Example coordinates
    const points = await getCompetitionPoints(competition.cities);

    // Add markers to the map
    points.forEach(point => {
        let marker = L.marker([point.latitude, point.longitude], {
            icon: L.divIcon({
                html: svgMarker,
                iconSize: [20, 20],
                className: 'city-point'
            })
        }).addTo(map);

        // Add click event listener to marker
        marker.on('click', function () {
            console.log('Clicked at: ' + point.latitude + ', ' + point.longitude);
        });
    });
}
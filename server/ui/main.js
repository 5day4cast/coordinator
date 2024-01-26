import * as duckdb from 'https://cdn.jsdelivr.net/npm/@duckdb/duckdb-wasm@1.27.1-dev125.0/+esm';

// Setup duckdb
const JSDELIVR_BUNDLES = duckdb.getJsDelivrBundles();

const bundle = await duckdb.selectBundle(JSDELIVR_BUNDLES);
// Select a bundle based on browser checks
const worker_url = URL.createObjectURL(
    new Blob([`importScripts("${bundle.mainWorker}");`], { type: 'text/javascript' })
);

// Instantiate the asynchronus version of DuckDB-wasm
const worker = new Worker(worker_url);
const logger = new duckdb.ConsoleLogger();
const db = new duckdb.AsyncDuckDB(logger, worker);
await db.instantiate(bundle.mainModule, bundle.pthreadWorker);
URL.revokeObjectURL(worker_url);

const apiBase = API_BASE;
console.log("api location:", apiBase);
const parquetFileApi = "https://www.4casttruth.win";
console.log("parquets location:", apiBase);

/*
# navbar code
*/
const $navbarItems = document.querySelectorAll('.navbar-item');
const $navDivs = document.querySelectorAll('a[id$="NavClick"]');
const $navbarBurgers = Array.prototype.slice.call(document.querySelectorAll('.navbar-burger'), 0);

// Add a click event on each of them
$navbarBurgers.forEach(el => {
    el.addEventListener('click', () => {

        // Get the target from the "data-target" attribute
        const target = el.dataset.target;
        const $target = document.getElementById(target);

        // Toggle the "is-active" class on both the "navbar-burger" and the "navbar-menu"
        el.classList.toggle('is-active');
        $target.classList.toggle('is-active');

    });
});

$navbarItems.forEach(function ($navbarItem) {
    $navbarItem.addEventListener('click', function (event) {
        event.preventDefault();
        // Hide all containers
        hideAllContainers();
        // Extract the ID from the clicked navbar item
        const targetContainerId = this.id.replace('NavClick', '');
        // Show the corresponding container
        console.log(targetContainerId);
        showContainer(targetContainerId);
    });
});

// Function to hide all containers
function hideAllContainers() {
    $navDivs.forEach(function ($container) {
        const containerId = $container.id.split("NavClick")[0];
        const $containerToHide = document.getElementById(containerId);
        if ($containerToHide) {
            $containerToHide.classList.add('hidden');
        }
    });
}

// Function to show a specific container
function showContainer(containerId) {
    const $containerToShow = document.getElementById(containerId);
    if ($containerToShow) {
        $containerToShow.classList.remove('hidden');
    }
}


/*
# all competitions
*/
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



// Define the number of arrays and the maximum number of items per array
const numArrays = 9;
const maxItemsPerArray = 10;
const seed = 123;

// Function to generate a pseudo-random number based on a seed
function seededRandom(seed) {
    let x = Math.sin(seed++) * 10000;
    return x - Math.floor(x);
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

const competitions = [
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

displayCompetitions(competitions);

function displayCompetitions(competitions) {
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
let currentMaps = {};

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



async function getCompetitionPoints(station_ids) {
    const query = `SELECT latitude, longitude FROM observations WHERE station_id IN ('${station_ids.join('\', \'')}')  GROUP BY station_id, station_name, latitude, longitude;`;
    try {
        const conn = await db.connect();
        const queryResult = await conn.query(query);
        const results = buildObjectFromResult(queryResult);
        await conn.close();
        return results;
    } catch (error) {
        console.error(error);
    }
}

function buildObjectFromResult(queryResult) {
    // build object properties
    let baseObject = {};
    for (const [index, column] of Object.entries(queryResult.schema.fields)) {
        baseObject[column.name] = '';
    }
    const keys = Object.keys(baseObject);
    // add object rows
    let results = [];
    for (const batch_index in queryResult.batches) {
        const row_count = queryResult.batches[batch_index].data.length;
        console.log(row_count);
        let data_grid = [];
        for (const column_index in queryResult.batches[batch_index].data.children) {
            const column = queryResult.batches[batch_index].data.children[column_index];
            console.log(column);
            let values = column.values;
            const array_type = getArrayType(values);
            if (array_type == 'BigInt64Array') {
                values = formatInts(values);
            }
            if (array_type == 'Uint8Array') {
                const offSets = column.valueOffsets;
                values = convertUintArrayToStrings(values, offSets);
            }
            data_grid.push(values);
        }
        console.log(data_grid);
        for (let row_index = 0; row_index < row_count; row_index++) {
            const newItem = { ...baseObject };
            for (const column_index in queryResult.batches[batch_index].data.children) {
                const propertyName = keys[column_index];
                newItem[propertyName] = data_grid[column_index][row_index];
            }
            results.push(newItem);
        }

    }
    console.log(results);
    return results;
}

async function forecasts(stations_ids) {


    //TODO: set the query for which locations the competitions
    try {
        const conn = await db.connect();
        const queryResult = await conn.query(rawQuery);
        console.log("queryResult", queryResult);
        await conn.close();
    } catch (error) {
        console.error(error);
    }
}

async function observations(stations_ids) {
    //TODO: set the query for which locations the competitions
    const rawQuery = "";
    try {
        const conn = await db.connect();
        const queryResult = await conn.query(rawQuery);
        console.log("queryResult", queryResult);
        await conn.close();
    } catch (error) {
        console.error(error);
    }
}


/*
# data download code
*/

// Download last 4 hour's files on initial load
submitDownloadRequest(null);

async function submitDownloadRequest(event) {
    if (event !== null) { event.preventDefault() };
    try {
        const fileNames = await fetchFileNames();
        console.log(`Files to download: ${fileNames}`);
        await loadFiles(fileNames);
        console.log('Successfully download parquet files');
    } catch (error) {
        console.error('Error to download files:', error);
    }
}

function fetchFileNames() {
    const currentUTCDate = new Date();
    const fourHoursAgoUTCDate = new Date(currentUTCDate.getTime() - (4 * 3600 * 1000));
    const rfc3339TimeFourHoursAgo = fourHoursAgoUTCDate.toISOString();
    const rfc3339TimeUTC = currentUTCDate.toISOString();

    return new Promise((resolve, reject) => {
        let url = `${parquetFileApi}/files?start=${rfc3339TimeFourHoursAgo}&end=${rfc3339TimeUTC}`;
        console.log(`Requesting: ${url}`)
        fetch(url)
            .then(response => {
                if (!response.ok) {
                    throw new Error(`HTTP error! Status: ${response.status}`);
                }
                return response.json();
            })
            .then(data => {
                console.log(data);
                resolve(data.file_names);
            })
            .catch(error => {
                console.error("Error fetching file names:", error)
                reject(error);
            });
    });
}

async function loadFiles(fileNames) {
    const conn = await db.connect();
    let observation_files = [];
    let forecast_files = [];
    for (const fileName of fileNames) {
        let url = `${parquetFileApi}/file/${fileName}`;
        if (fileName.includes("observations")) {
            observation_files.push(url);
        } else {
            forecast_files.push(url);
        }
        await db.registerFileURL(fileName, url, duckdb.DuckDBDataProtocol.HTTP, false);
        const res = await fetch(url);
        await db.registerFileBuffer('buffer.parquet', new Uint8Array(await res.arrayBuffer()));
    }
    if (Array.isArray(observation_files) && observation_files.length > 0) {
        await conn.query(`
        CREATE TABLE observations AS SELECT * FROM read_parquet(['${observation_files.join('\', \'')}'], union_by_name = true);
        `);
    }

    if (Array.isArray(forecast_files) && forecast_files.length > 0) {
        let files =
            await conn.query(`
    CREATE TABLE forecasts AS SELECT * FROM read_parquet(['${forecast_files.join('\', \'')}'], union_by_name = true);
    `);
    }
    await conn.close();
}

function getArrayType(arr) {
    if (arr instanceof Uint8Array) {
        return 'Uint8Array';
    } else if (arr instanceof Float64Array) {
        return 'Float64Array';
    } else if (arr instanceof BigInt64Array) {
        return 'BigInt64Array';
    } else {
        return 'Unknown';
    }
}

function getType(arr) {
    if (arr instanceof Uint8Array) {
        return 'Text';
    } else if (arr instanceof Float64Array) {
        return 'Float64';
    } else if (arr instanceof BigInt64Array) {
        return 'BigInt64';
    } else {
        return 'Unknown';
    }
}

function convertUintArrayToStrings(uint8Array, valueOffsets) {
    const textDecoder = new TextDecoder('utf-8');
    // Array to store the decoded strings
    const decodedStrings = [];

    for (let i = 0; i < valueOffsets.length; i++) {
        const start = (i === 0) ? 0 : valueOffsets[i - 1]; // Start position for the first string is 0
        const end = valueOffsets[i];
        const stringBytes = uint8Array.subarray(start, end);
        const decodedString = textDecoder.decode(stringBytes);
        if (decodedString.length != 0) {
            decodedStrings.push(decodedString);
        }
    }

    console.log(decodedStrings);
    return decodedStrings
}


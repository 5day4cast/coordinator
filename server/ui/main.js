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
const stations_ids = Object.keys(stations_to_cities);
console.log(stations_ids);

async function forecasts(stations_ids) {

    //TODO: set the query for which locations the competitions
    const rawQuery = document.getElementById('customQuery').value;
    try {
        const conn = await db.connect();
        const queryResult = await conn.query(rawQuery);
        loadTable("queryResult", queryResult);
        await conn.close();
    } catch (error) {
        console.error(error);
    }
}

async function observations(stations_ids) {
    //TODO: set the query for which locations the competitions
    const rawQuery = document.getElementById('customQuery').value;
    try {
        const conn = await db.connect();
        const queryResult = await conn.query(rawQuery);
        loadTable("queryResult", queryResult);
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


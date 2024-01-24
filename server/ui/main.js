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

// Setting the date
const currentUTCDate = new Date();
const oneDayAgoUTCDate = new Date(currentUTCDate.getTime() - 86400000);
const rfc3339TimeOneDayAgo = oneDayAgoUTCDate.toISOString();
const startTime = document.getElementById('start');
startTime.value = rfc3339TimeOneDayAgo;

const rfc3339TimeUTC = currentUTCDate.toISOString();
const endTime = document.getElementById('end');
endTime.value = rfc3339TimeUTC;

// Download todays files on initial load
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
    const startTime = document.getElementById('start').value;
    const endTime = document.getElementById('end').value;
    const forecasts = document.getElementById('forecasts').checked;
    const observations = document.getElementById('observations').checked;

    return new Promise((resolve, reject) => {
        let url = `${parquetFileApi}/files?start=${startTime}&end=${endTime}&observations=${observations}&forecasts=${forecasts}`;
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
        const observations = await conn.query(`SELECT * FROM observations LIMIT 1;`);
        loadSchema("observations", observations);
    }

    if (Array.isArray(forecast_files) && forecast_files.length > 0) {
        let files =
            await conn.query(`
    CREATE TABLE forecasts AS SELECT * FROM read_parquet(['${forecast_files.join('\', \'')}'], union_by_name = true);
    `);
        const forecasts = await conn.query(`SELECT * FROM forecasts LIMIT 1;`);
        loadSchema("forecasts", forecasts);
    }
    await conn.close();
}

/*
async function runQuery(event) {
    const rawQuery = document.getElementById('customQuery').value;
    try {
        const conn = await db.connect();
        const queryResult = await conn.query(rawQuery);
        loadTable("queryResult", queryResult);
        await conn.close();
    } catch (error) {
        displayQueryErr(error);
    }
}*/

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


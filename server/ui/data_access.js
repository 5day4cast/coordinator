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


const parquetFileApi = "https://www.4casttruth.win";
console.log("parquets location:", parquetFileApi);

export async  function submitDownloadRequest() {
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

export async function queryDb(query) {
    try {
        const conn = await db.connect();
        console.log(query);
        const queryResult = await conn.query(query);
        console.log(queryResult);
        const results = buildObjectFromResult(queryResult);
        await conn.close();
        return results;
    } catch (error) {
        console.error(error);
    }
}

function formatInts(intArray) {
    const maxSafeInteger = BigInt(Number.MAX_SAFE_INTEGER);
    let formattedVals = [];
    for (let i = 0; i < intArray.length; i++) {
        if (intArray[i] > maxSafeInteger || intArray[i] < -maxSafeInteger) {
            formattedVals[i] = "NaN";
        } else {
            formattedVals[i] = `${intArray[i]}`
        }
    }

    return formattedVals
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

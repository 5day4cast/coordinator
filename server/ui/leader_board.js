import { queryDb } from './data_access.js';

//TODO:
// display the entries in a table
// have the table replace the competition table
// highlight the entries that placed when competition is completed

export async function displayLeaderboard(competition, rowIsSelected) {
    console.log(competition);
    const entries = getEntries(competition); //Would normally be async as need to grab entries from remote
    // if we get no readings during the competition window 
    // we should cancel the competition and refund people
    const currentReadings = await getReadings(competition);
    console.log(currentReadings);
    const lastForecasts = await getLastForecast(competition);
    console.log(lastForecasts);
    const entryScores = calculateScores(currentReadings, lastForecasts, entries);
    console.log(entryScores);
    displayScore(entryScores);
}

function displayScore(entryScores) {
    let $competitionsDataTable = document.getElementById("competitionLeaderboardData");
    let $tbody = $competitionsDataTable.querySelector("tbody");
    if (!$tbody) {
        $tbody = document.createElement("tbody");
        $competitionsDataTable.appendChild($tbody);
    }
    entryScores.forEach((entryScore, index) => {
        let $row = document.createElement("tr");

        const rank = document.createElement("td");
        cell.textContent = index;
        $row.appendChild(rank);

        const cellId = document.createElement("td");
        cell.textContent = entryScore['id'];
        $row.appendChild(cellId);

        const cellScore = document.createElement("td");
        cell.textContent = entryScore['score'];
        $row.appendChild(cellScore);

        $row.addEventListener("click", () => {
            handleEntryClick($row, entry);
        });

        $tbody.appendChild($row);
    });

}

function handleEntryClick($row, entry) {
    console.log(row);
    console.log(entry);
}

function getEntries(competition) {
    //TODO: change to be a request to remove server for list of enteries in competition
    const entries = [

        {
            "id": "1965d723-6119-433f-a171-609c215f30d3",
            "user_id": "2bbcddd2-e034-4bf3-974d-c44719f71d2e",
            "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "options": [
                {
                    "station_id": "KBFM",
                    "wind_speed": {
                        "val": "par"
                    },
                    "temp_high": {
                        "val": "over"
                    },
                    "temp_low": {
                        "val": "par"
                    }
                }
            ]
        },
        {
            "id": "c0f3da7b-0f35-418f-9d3f-1e9621ed4f21",
            "user_id": "3d65e025-49b9-4a2d-9f6d-3b73f34880c1",
            "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "options": [
                {
                    "station_id": "KBHM",
                    "wind_speed": {
                        "val": "par"
                    },
                    "temp_high": {
                        "val": "under"
                    },
                    "temp_low": {
                        "val": "over"
                    }
                }
            ]
        },
        {
            "id": "1965d723-6119-433f-a171-609c215f30d3",
            "user_id": "2bbcddd2-e034-4bf3-974d-c44719f71d2e",
            "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "options": [
                {
                    "station_id": "KBFM",
                    "wind_speed": {
                        "val": "over"
                    },
                    "temp_high": {
                        "val": "par"
                    },
                    "temp_low": {
                        "val": "under"
                    }
                }
            ]
        },
        {
            "id": "c0f3da7b-0f35-418f-9d3f-1e9621ed4f21",
            "user_id": "3d65e025-49b9-4a2d-9f6d-3b73f34880c1",
            "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "options": [
                {
                    "station_id": "KBHM",
                    "wind_speed": {
                        "val": "over"
                    },
                    "temp_high": {
                        "val": "par"
                    },
                    "temp_low": {
                        "val": "under"
                    }
                }
            ]
        },
        {
            "id": "4e6f8a5d-6fe7-4987-8821-12d51b801e5b",
            "user_id": "3b3b0255-1c7d-4a65-afd7-1fe75ac9e540",
            "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "options": [
                {
                    "station_id": "KMSY",
                    "wind_speed": {
                        "val": "over"
                    },
                    "temp_high": {
                        "val": "par"
                    },
                    "temp_low": {
                        "val": "under"
                    }
                }
            ]
        },
        {
            "id": "13f23a1c-3bcf-4d8e-8fa3-241d289ae106",
            "user_id": "54cfb591-26f7-45a5-bd12-094a88b47d84",
            "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "options": [
                {
                    "station_id": "KLIT",
                    "wind_speed": {
                        "val": "over"
                    },
                    "temp_high": {
                        "val": "par"
                    },
                    "temp_low": {
                        "val": "under"
                    }
                }
            ]
        },
        {
            "id": "891d2314-3eb2-4410-ba06-857a9a302aa2",
            "user_id": "7a6ee926-4d89-4a95-aa9b-df732d14ef9b",
            "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "options": [
                {
                    "station_id": "KMCI",
                    "wind_speed": {
                        "val": "over"
                    },
                    "temp_high": {
                        "val": "par"
                    },
                    "temp_low": {
                        "val": "under"
                    }
                }
            ]
        },
        {
            "id": "eb474ef0-7f37-4a34-a205-09890b2c7d9b",
            "user_id": "53b1732f-5179-49aa-b8d7-c1318f27b16d",
            "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "options": [
                {
                    "station_id": "KBFM",
                    "wind_speed": {
                        "val": "over"
                    },
                    "temp_high": {
                        "val": "par"
                    },
                    "temp_low": {
                        "val": "under"
                    }
                }
            ]
        },
        {
            "id": "d4d01269-6170-4d64-976d-2989b57cbb05",
            "user_id": "0b42e158-d164-4de4-9a11-17fb0483f86c",
            "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
            "options": [
                {
                    "station_id": "KBHM",
                    "wind_speed": {
                        "val": "over"
                    },
                    "temp_high": {
                        "val": "par"
                    },
                    "temp_low": {
                        "val": "under"
                    }
                }
            ]
        }
    ];
    return entries
}

async function getReadings(competition) {
    console.log(competition);
    // remove this once hooked up to the backend, just used for now for testing

    const startTime = new Date(competition.startTime);
    const threeHoursAgoUTCDate = new Date(startTime.getTime() - (3 * 3600 * 1000));

    const station_ids = competition.cities;
    // change to `AND generated_at >= '${competition.startTime}'::TIMESTAMPTZ`
    // once observation data is definitely there

    const query = `
        SELECT 
            station_id, 
            min(generated_at) as start_time, 
            max(generated_at) as end_time, 
            min(temperature_value) as temp_low, 
            max(temperature_value) as temp_high, 
            max(wind_speed) as wind_speed 
        FROM observations 
        WHERE station_id IN ('${station_ids.join('\', \'')}') 
            AND generated_at <= '${competition.endTime}'::TIMESTAMPTZ 
            AND generated_at >= '${threeHoursAgoUTCDate.toISOString()}'::TIMESTAMPTZ 
        GROUP BY station_id;`;
    const readings = await queryDb(query);
    const station_readings = {};
    for (let reading of readings) {
        station_readings[reading.station_id] = reading;
    }

    return station_readings;
}

async function getLastForecast(competition) {
    console.log(competition);
    const station_ids = competition.cities;
    const query = `
    SELECT 
        station_id, 
        max(generated_at) as last_time, 
        last(max_temp) as temp_high, 
        last(min_temp) as temp_low, 
        last(wind_speed) as wind_speed 
    FROM forecasts 
    WHERE station_id IN ('${station_ids.join('\', \'')}') 
        AND begin_time >= '${competition.startTime}'::TIMESTAMPTZ 
        AND end_time <= '${competition.endTime}'::TIMESTAMPTZ 
    GROUP BY station_id;`;
    const forecasts = await queryDb(query);
    const station_forecast = {};
    for (let forecast of forecasts) {
        station_forecast[forecast.station_id] = forecasts;
    }
    return station_forecast;
}

function calculateScores(weatherReadings, lastForecasts, entries) {
    for (let entry of entries) {
        let currentScore = 0;
        for (let option of entry.options) {
            const station_id = option.station_id;
            console.log(lastForecasts);
            console.log(weatherReadings);
            console.log(station_id);
            const forecast = lastForecasts[station_id];
            console.log(forecast);
            const observation = weatherReadings[station_id];
            console.log(observation);
            Object.keys(option).forEach((key) => {
                if (key == "station_id") {
                    return;
                }
                const optionScore = calculateOptionScore(forecast[key], observation[key], option[key].val);
                currentScore += optionScore;
            });
        }
        entries[i]['score'] = currentScore;
    }
    entries.sort((a, b) => b.score - a.score);
    return entries;
}

function calculateOptionScore(forecast_val, observation_val, entry_val) {
    if (forecast_val > observation_val) {
        return (entry_val == "over") ? 1 : 0
    } else if (forecast_val == observation_val) {
        return (entry_val == "par") ? 2 : 0
    } else {
        return (entry_val == "under") ? 1 : 0
    }
}
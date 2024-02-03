import { queryDb } from './data_access.js';

//TODO:
// create fake entries and scores of those entries
// display the entries in a table
// have the table replace the competition table
// highlight the entries that placed when competition is completed
function get_entries(competition) {
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
                        "forecast_val": "25 knots",
                        "forecast_time": "2024-02-24T12:00:00Z",
                        "val": "high"
                    },
                    "temp_high": {
                        "forecast_val": "72°F",
                        "forecast_time": "2024-02-24T15:00:00Z",
                        "val": "par"
                    },
                    "temp_low": {
                        "forecast_val": "60°F",
                        "forecast_time": "2024-02-24T03:00:00Z",
                        "val": "low"
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
                        "forecast_val": "20 knots",
                        "forecast_time": "2024-02-24T11:00:00Z",
                        "val": "high"
                    },
                    "temp_high": {
                        "forecast_val": "70°F",
                        "forecast_time": "2024-02-24T14:00:00Z",
                        "val": "par"
                    },
                    "temp_low": {
                        "forecast_val": "55°F",
                        "forecast_time": "2024-02-24T02:00:00Z",
                        "val": "low"
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
                        "forecast_val": "25 knots",
                        "forecast_time": "2024-02-24T12:00:00Z",
                        "val": "high"
                    },
                    "temp_high": {
                        "forecast_val": "72°F",
                        "forecast_time": "2024-02-24T15:00:00Z",
                        "val": "par"
                    },
                    "temp_low": {
                        "forecast_val": "60°F",
                        "forecast_time": "2024-02-24T03:00:00Z",
                        "val": "low"
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
                        "forecast_val": "20 knots",
                        "forecast_time": "2024-02-24T11:00:00Z",
                        "val": "high"
                    },
                    "temp_high": {
                        "forecast_val": "70°F",
                        "forecast_time": "2024-02-24T14:00:00Z",
                        "val": "par"
                    },
                    "temp_low": {
                        "forecast_val": "55°F",
                        "forecast_time": "2024-02-24T02:00:00Z",
                        "val": "low"
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
                        "forecast_val": "18 knots",
                        "forecast_time": "2024-02-24T10:00:00Z",
                        "val": "high"
                    },
                    "temp_high": {
                        "forecast_val": "68°F",
                        "forecast_time": "2024-02-24T13:00:00Z",
                        "val": "par"
                    },
                    "temp_low": {
                        "forecast_val": "52°F",
                        "forecast_time": "2024-02-24T01:00:00Z",
                        "val": "low"
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
                        "forecast_val": "22 knots",
                        "forecast_time": "2024-02-24T09:00:00Z",
                        "val": "high"
                    },
                    "temp_high": {
                        "forecast_val": "69°F",
                        "forecast_time": "2024-02-24T12:00:00Z",
                        "val": "par"
                    },
                    "temp_low": {
                        "forecast_val": "54°F",
                        "forecast_time": "2024-02-24T00:00:00Z",
                        "val": "low"
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
                        "forecast_val": "19 knots",
                        "forecast_time": "2024-02-24T08:00:00Z",
                        "val": "high"
                    },
                    "temp_high": {
                        "forecast_val": "67°F",
                        "forecast_time": "2024-02-24T11:00:00Z",
                        "val": "par"
                    },
                    "temp_low": {
                        "forecast_val": "53°F",
                        "forecast_time": "2024-02-24T23:00:00Z",
                        "val": "low"
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
                        "forecast_val": "23 knots",
                        "forecast_time": "2024-02-24T07:00:00Z",
                        "val": "high"
                    },
                    "temp_high": {
                        "forecast_val": "68°F",
                        "forecast_time": "2024-02-24T10:00:00Z",
                        "val": "par"
                    },
                    "temp_low": {
                        "forecast_val": "55°F",
                        "forecast_time": "2024-02-24T22:00:00Z",
                        "val": "low"
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
                        "forecast_val": "21 knots",
                        "forecast_time": "2024-02-24T06:00:00Z",
                        "val": "high"
                    },
                    "temp_high": {
                        "forecast_val": "71°F",
                        "forecast_time": "2024-02-24T09:00:00Z",
                        "val": "par"
                    },
                    "temp_low": {
                        "forecast_val": "56°F",
                        "forecast_time": "2024-02-24T21:00:00Z",
                        "val": "low"
                    }
                }
            ]
        }
    ];
    return entries
}



export async function displayLeaderboard(competition, rowIsSelected) {
    const entries = get_entries(competition); //Would normally be async as need to grab entries from remote
    const currentReadings = get_readings(competition); // """
    console.log(currentReadings);
    const last_forecasts = get_last_forecast(competition);
    const scores = calculatScores(currentReadings, last_forecasts, entries);

}

function get_readings(competition) {
    const station_ids = competition.cities;
    //TODO: change start and end time to be the start and end time of the competition
    const query = `SELECT station_id, min(generated_at) as start_time, max(generated_at) as end_time, min(temperature_value) as temp_min, max(temperature_value) as max_temp, max(wind_speed) as wind_speed FROM observations WHERE station_id IN ('${station_ids.join('\', \'')}') AND generated_at <= NOW() AND generated_at >= (NOW()::TIMESTAMP - INTERVAL 12 hours) GROUP BY station_id;`;
    return queryDb(query);
}

function get_last_forecast(competition) {
    const station_ids = competition.cities;
    const query = `SELECT station_id, max(generated_at) as last_time, last(max_temp) as max_temp, last(min_temp) as min_temp, last(wind_speed) as wind_speed FROM forecasts WHERE station_id IN ('${station_ids.join('\', \'')}') AND generated_at <= '${competition.startTime}'::TIMESTAMPTZ GROUP BY station_id;`;
    return queryDb(query);
}

function calculatScores(weatherReadings, last_forecasts, entries) {
    console.log(weatherReadings);
    console.log(last_forecasts);
    console.log(entries);
}
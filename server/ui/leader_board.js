import { WeatherData } from './weather_data.js';

//TODO:
// highlight the entries that placed/won money when competition is completed
class LeaderBoard {
    constructor(base_url, competition) {
        this.competition = competition;
        this.weather_data = new WeatherData(base_url);
    }

    async init() {
        this.clearTable();
        // if we get no readings during the competition window 
        // we should cancel the competition and refund people
        Promise.all([
            this.getEntries(this.competition),
            this.getReadings(this.competition),
            this.weather_data.get_competition_last_forecast(this.competition)
        ]).then(([entries, observations, lastForecasts]) => {
            const entryScores = this.calculateScores(observations, lastForecasts, entries);
            this.displayScore(entryScores);
        });
    }

    async getReadings(competition) {
        const observations = await this.weather_data.get_observations(competition.cities, {
            'start': competition.startTime,
            'end': competition.endTime
        });
        const station_observations = {};
        for (let observation of observations) {
            station_observations[observation.station_id] = observation;
        }
        return station_observations;
    }

    calculateScores(weatherReadings, lastForecasts, entries) {
        for (let entry of entries) {
            let currentScore = 0;
            for (let option of entry.options) {
                const station_id = option.station_id;
                const forecast = lastForecasts[station_id];
                if (!forecast) {
                    console.error("no forecast found for:", station_id);
                    continue;
                }
                const observation = weatherReadings[station_id];
                if (!forecast) {
                    console.error("no observations found for:", station_id);
                    continue;
                }
                Object.keys(option).forEach((key) => {
                    if (key == "station_id") {
                        return;
                    }
                    const optionScore = this.calculateOptionScore(forecast[key], observation[key], option[key].val);
                    option[key]['score'] = optionScore;
                    option[key]['forecast'] = forecast[key];
                    option[key]['observation'] = observation[key];
                    currentScore += optionScore;
                });
            }
            entry['score'] = currentScore;
        }
        entries.sort((a, b) => b.score - a.score);
        return entries;
    }

    calculateOptionScore(forecast_val, observation_val, entry_val) {
        if (forecast_val > observation_val) {
            return (entry_val == "over") ? 1 : 0
        } else if (forecast_val == observation_val) {
            return (entry_val == "par") ? 2 : 0
        } else {
            return (entry_val == "under") ? 1 : 0
        }
    }

    clearTable() {
        let $competitionsDataTable = document.getElementById("competitionLeaderboardData");
        let $tbody = $competitionsDataTable.querySelector("tbody");
        if ($tbody) {
            while ($tbody.firstChild) {
                $tbody.removeChild($tbody.firstChild);
            }
        }
    }

    displayScore(entryScores) {
        let $competitionsDataTable = document.getElementById("competitionLeaderboardData");
        let $tbody = $competitionsDataTable.querySelector("tbody");
        if (!$tbody) {
            $tbody = document.createElement("tbody");
            $competitionsDataTable.appendChild($tbody);
        }
        entryScores.forEach((entryScore, index) => {
            let $row = document.createElement("tr");

            const rank = document.createElement("td");
            rank.textContent = index+1;
            $row.appendChild(rank);
            if (entryScore['score'] == undefined || entryScore['score'] == null) {
                console.error("no score found for entry:", entryScore['id']);
                return
            }

            const cellId = document.createElement("td");
            cellId.textContent = entryScore['id'];
            $row.appendChild(cellId);

            const cellScore = document.createElement("td");
            cellScore.textContent = entryScore['score'];
            $row.appendChild(cellScore);

            $row.addEventListener("click", () => {
                this.handleEntryClick($row, entryScore);
            });

            $tbody.appendChild($row);
            this.showLeaderboard();
        });
    }

    showLeaderboard() {
        let $currentCompetitionCurrent = document.getElementById("competitionLeaderboard");
        $currentCompetitionCurrent.classList.remove('hidden');
    }

    hideLeaderboard() {
        let $currentCompetitionCurrent = document.getElementById("competitionLeaderboard");
        $currentCompetitionCurrent.classList.add('hidden');
    }

    getEntries(competition) {
        if (competition['id'] == '295ecf23-ef65-4708-9314-0fc7614b623d') {
            return new Promise((resolve, reject) => {
                setTimeout(() => {
                    resolve(completedEntries)
                }, 500)
            });
        } else {
            return new Promise((resolve, reject) => {
                setTimeout(() => {
                    resolve(runningEntries)
                }, 500)
            });
        }
    }

    handleEntryClick($row, entry) {
        const $parentElement = $row.parentElement;
        const $rows = $parentElement.querySelectorAll("tr");
        $rows.forEach($currentRow => {
            if ($currentRow != $row) {
                $currentRow.classList.remove('is-selected');
            }
        });
        $row.classList.toggle('is-selected');
        this.showEntryScores(entry);
    }


    showEntryScores(entry) {
        let $entryScoreModal = document.getElementById("entryScore");
        this.clearEntry();

        let $entryValues = document.getElementById('entryValues');
        let $competitionId = document.createElement('h3');
        $competitionId.textContent = `Competition: ${entry.competition_id}`
        $entryValues.appendChild($competitionId);

        for (let option of entry['options']) {
            let $stationDiv = document.createElement('div');
            if (option['station_id']) {
                let $stationHeader = document.createElement('h5');
                $stationHeader.textContent = `Station: ${option.station_id}`;
                $stationHeader.classList.add("ml-2");
                $stationDiv.appendChild($stationHeader);
            }
            let $stationList = document.createElement('ul');
            if (option['wind_speed']) {
                this.buildEntryScorePick($stationList, "Wind Speed", option['wind_speed']);
            }
            if (option['temp_high']) {
                this.buildEntryScorePick($stationList, "High Temp", option['temp_high']);
            }

            if (option['temp_low']) {
                this.buildEntryScorePick($stationList, "Low Temp", option['temp_low']);
            }

            $stationDiv.appendChild($stationList);
            $entryValues.appendChild($stationDiv);

        }

        let $totalPts = document.createElement('h6');
        $totalPts.textContent = `Total Points: ${entry.score}`
        $entryValues.appendChild($totalPts);


        if (!$entryScoreModal.classList.contains('is-active')) {
            $entryScoreModal.classList.add('is-active');
        }
    }

    buildEntryScorePick($stationList, type, option) {
        let $optionListItem = document.createElement('li');
        // forecast val, observation val, val, score
        //$optionListItem.textContent = `Wind Speed ${} ${} ${} ${}`
        $optionListItem.classList.add("ml-4");
        $optionListItem.textContent = `${type}: `;
        let $breakdown = document.createElement('ul');

        let $forecast = document.createElement('li');
        $forecast.classList.add("ml-6");
        $forecast.textContent = `Forecast: ${option['forecast']}`;
        $breakdown.appendChild($forecast);

        let $observation = document.createElement('li');
        $observation.classList.add("ml-6");
        $observation.textContent = `Observation: ${option['observation']}`;
        $breakdown.appendChild($observation);

        let $pick = document.createElement('li');
        $pick.classList.add("ml-6");
        $pick.textContent = `Pick: ${option['val']}`;
        $breakdown.appendChild($pick);

        let $score = document.createElement('li');
        $score.classList.add("ml-6");
        $score.textContent = `Score: ${option['score']}`;
        $breakdown.appendChild($score);
        $optionListItem.appendChild($breakdown);
        $stationList.appendChild($optionListItem);
    }

    hideEntry() {
        let $entryScoreModal = document.getElementById("entryScore");
        if ($entryScoreModal.classList.contains('is-active')) {
            $entryScoreModal.classList.remove('is-active');
        }
    }

    clearEntry() {
        let $entryValues = document.getElementById('entryValues');
        if ($entryValues) {
            while ($entryValues.firstChild) {
                $entryValues.removeChild($entryValues.firstChild);
            }
        }
    }

}

const runningEntries = [

    {
        "id": "1965d723-6119-433f-a171-609c215f30d3",
        "user_id": "2bbcddd2-e034-4bf3-974d-c44719f71d2e",
        "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
        "options": [
            {
                "station_id": "KTPA",
                "wind_speed": {
                    "val": "par"
                },
            },
            {
                "station_id": "v",
                "wind_speed": {
                    "val": "par"
                },
                "temp_high": {
                    "val": "over"
                },
            }
        ]
    },
    {
        "id": "c0f3da7b-0f35-418f-9d3f-1e9621ed4f21",
        "user_id": "3d65e025-49b9-4a2d-9f6d-3b73f34880c1",
        "competition_id": "295ecf23-ef65-4708-9314-0fc7614b623d",
        "options": [
            {
                "station_id": "KMIA",
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
                "station_id": "KATL",
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
                "station_id": "KSDF",
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
                "station_id": "KBNA",
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
                "station_id": "KBNA",
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
                "station_id": "KSDF",
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
                "station_id": "KMIA",
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
                "station_id": "KATL",
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
//TODO: change to be a request to remove server for list of enteries in competition
const completedEntries = [

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
            },
            {
                "station_id": "KMSY",
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

export { LeaderBoard };

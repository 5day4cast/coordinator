import { WeatherData } from './weather_data.js';
import { LeaderBoard } from './leader_board.js';
import { Entry } from './entry.js';

export async function displayCompetitions(apiBase) {
    let $competitionsDataTable = document.getElementById("competitionsDataTable");
    let $tbody = $competitionsDataTable.querySelector("tbody");
    if (!$tbody) {
        $tbody = document.createElement("tbody");
        $competitionsDataTable.appendChild($tbody);
    }
    let comp = new Competitions(apiBase, $competitionsDataTable, $tbody);
    await comp.init();
    console.log("initialized competitions");
}

class Competitions {
    constructor(base_url, $competitionsDataTable, $tbody) {
        this.weather_data = new WeatherData(base_url);
        this.base_url = base_url;
        this.currentMaps = {};
        this.$competitionsDataTable = $competitionsDataTable;
        this.$tbody = $tbody;
    }

    async init() {
        Promise.all([
            this.get_stations(),
            this.get_competitions()
        ]).then(([stations, competitions]) => {
            this.stations = stations;
            this.competitions = competitions;
            this.competitions.forEach(competition => {
                let $row = document.createElement("tr");
                // Exclude the "cities" property
                Object.keys(competition).forEach(key => {
                    if (key !== "cities" && key !== "id") {
                        const cell = document.createElement("td");
                        cell.textContent = competition[key];
                        $row.appendChild(cell);
                    }
                });
                //TODO: change text depending on what competition state we're at
                const cell = document.createElement("td");
                if (competition.status == 'live') {
                    cell.textContent = "Create Entry";
                } else {
                    cell.textContent = "View Competition"
                }
                $row.appendChild(cell);

                $row.addEventListener("click", (event) => {
                    this.handleCompetitionClick($row, competition);
                    if (event.target.tagName === 'TD') {
                        if (event.target === event.target.parentNode.lastElementChild) {
                            if (competition.status == 'live') {
                                let entry = new Entry(this.base_url, this.stations, competition);
                                entry.init().then(() => {
                                    entry.showEntry();
                                });
                            }
                        }
                    }
                });
                this.$tbody.appendChild($row);
            });
        }).catch(error => {
            console.error("Error occurred while fetching data:", error);
        });
    }

    async get_competitions() {
        let events = fetch(`${this.base_url}/oracle/events`).await;
        console.log(events);
        return events;
    }

    handleCompetitionClick(row, competition) {
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
            if (this.leader_board) {
                this.leader_board.hideLeaderboard();
            }
            this.makeCompetitionMap(competition).then(result => {
                this.showCurrentCompetition(rowIsSelected);
            }).catch(error => {
                console.error(error);
            });
        } else {
            this.hideCurrentCompetition();
            this.leader_board = new LeaderBoard(this.base_url, competition);
            this.leader_board.init().then(result => {
                console.log("leaderboard displayed");
            }).catch(error => {
                console.error(error);
            })
        }
    }

    showCurrentCompetition(isSelected) {
        let $currentCompetitionCurrent = document.getElementById("currentCompetition");
        if (!isSelected) {
            $currentCompetitionCurrent.classList.add('hidden');
            return
        }
        $currentCompetitionCurrent.classList.remove('hidden');
    }

    hideCurrentCompetition() {
        let $currentCompetitionCurrent = document.getElementById("currentCompetition");
        $currentCompetitionCurrent.classList.add('hidden');
    }

    async makeCompetitionMap(competition) {
        let oldMap = this.currentMaps["map"]; // Retrieve map instance by div ID
        if (oldMap !== undefined) {
            oldMap.remove();
        }
        const map = L.map('map', { dragging: false, trackResize: true }).setView([39.8283, -98.5795], 4.4); // Centered on the US
        L.tileLayer('https://tiles.stadiamaps.com/tiles/stamen_toner_background/{z}/{x}/{y}{r}.{ext}', {
            minZoom: 4,
            maxZoom: 7,
            attribution: '&copy; <a href="https://www.stadiamaps.com/" target="_blank">Stadia Maps</a> &copy; <a href="https://www.stamen.com/" target="_blank">Stamen Design</a> &copy; <a href="https://openmaptiles.org/" target="_blank">OpenMapTiles</a> &copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors',
            ext: 'png',
            maxBounds: [
                [25.84, -124.67], // Southwest coordinates (latitude, longitude)
                [49.38, -66.95] // Northeast coordinates (latitude, longitude)
            ]
        }).addTo(map);
        const points = await this.getCompetitionPoints(competition.cities);
        points.forEach(point => {
            let marker = L.circleMarker([point.latitude, point.longitude], {}).addTo(map);
            // Extend the pop here
            marker.bindPopup(`${point.station_name} (${point.station_id})`).openPopup();
        });
        if (map) {
            map.invalidateSize();
        }
        this.currentMaps['map'] = map;
    }

    async getCompetitionPoints(station_ids) {
        let competitionPoints = [];
        for (let station_id of station_ids) {
            let station = this.stations[station_id];
            if (station) {
                competitionPoints.push(station);
            }
        }
        return competitionPoints;
    }

    async get_stations() {
        const stations = await this.weather_data.get_stations();
        let stations_mapping = {};
        for (let station of stations) {
            stations_mapping[station.station_id] = station
        }
        return stations_mapping;
    }
}

export { Competitions };
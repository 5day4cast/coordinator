import { WeatherData } from './weather_data.js';
import { LeaderBoard } from './leader_board.js';

export async function displayEntry(apiBase, stations, competition) {

}


class Entry {
    constructor(base_url, stations, competition) {
        this.weather_data = new WeatherData(base_url);
        this.base_url = base_url;
        this.competition = competition;
        this.stations = stations;
        this.entry = { 'comptition_id': this.competition.id, 'submit': {} };
    }

    async init() {
        return Promise.all([
            this.weather_data.get_competition_last_forecast(this.competition)
        ]).then(([competition_forecasts]) => {
            console.log(competition_forecasts);
            this.competition_forecasts = competition_forecasts;
            this.entry['options'] = [];
            for (const station_id in competition_forecasts) {
                const forecast = competition_forecasts[station_id];
                const option = {
                    'station_id': station_id,
                    'date': forecast['date'],
                    'temp_high': forecast['temp_high'],
                    'temp_low': forecast['temp_low'],
                    'wind_speed': forecast['wind_speed']
                }
                this.entry['options'].push(option);
                this.entry['submit'][station_id] = {};
            }

        })
    }

    showEntry() {
        let $entryModal = document.getElementById("entry");
        this.clearEntry();

        let $entryValues = document.getElementById('entryContent');
        let $competitionId = document.createElement('h3');
        $competitionId.textContent = `Competition: ${this.competition.id}`
        $entryValues.appendChild($competitionId);
        for (let option of this.entry['options']) {
            let $stationDiv = document.createElement('div');
            if (option['station_id']) {
                let $stationHeader = document.createElement('h5');
                $stationHeader.textContent = `Station: ${option.station_id}`;
                $stationHeader.classList.add("ml-2");
                $stationDiv.appendChild($stationHeader);
            }
            let $stationList = document.createElement('ul');
            if (option['wind_speed']) {
                this.buildEntry($stationList, option.station_id, "Wind Speed", 'wind_speed', option['wind_speed']);
            }
            if (option['temp_high']) {
                this.buildEntry($stationList, option.station_id, "High Temp", 'temp_high', option['temp_high']);
            }

            if (option['temp_low']) {
                this.buildEntry($stationList, option.station_id, "Low Temp", 'temp_low', option['temp_low']);
            }

            $stationDiv.appendChild($stationList);
            $entryValues.appendChild($stationDiv);

        }
        let $submitEntry = document.getElementById('submitEntry');
        $submitEntry.addEventListener("click", ($event) => {
            $event.target.classList.add("is-loading");
            this.submit($event);
        });

        if (!$entryModal.classList.contains('is-active')) {
            $entryModal.classList.add('is-active');
        }
    }

    buildEntry($stationList, station_id, type_view, type, val) {
        let $optionListItem = document.createElement('li');
        // forecast val, observation val, val, score
        //$optionListItem.textContent = `Wind Speed ${} ${} ${} ${}`
        $optionListItem.classList.add("ml-4");
        $optionListItem.textContent = `${type_view}: `;
        let $breakdown = document.createElement('ul');

        let $forecast = document.createElement('li');
        $forecast.classList.add("ml-6");
        $forecast.textContent = `Forecast: ${val}`;
        $breakdown.appendChild($forecast);

        let $pick = document.createElement('li');
        $pick.classList.add("ml-6");
        this.buildEntryButtons($pick, station_id, type);
        $breakdown.appendChild($pick);

        $optionListItem.appendChild($breakdown);
        $stationList.appendChild($optionListItem);
    }

    buildEntryButtons($pick, station_id, weather_type) {
        let $overButton = document.createElement('button');
        $overButton.classList.add("button");
        $overButton.classList.add("is-info");
        $overButton.classList.add("is-outlined");
        $overButton.textContent = 'Over';
        $overButton.id = `${station_id}_${weather_type}_over`;
        $overButton.addEventListener("click", ($event) => { this.handleEntryClick($event, station_id, weather_type, 'over'); });
        $pick.appendChild($overButton);

        // Create and append the "Par" button
        let $parButton = document.createElement('button');
        $parButton.textContent = 'Par';
        $parButton.id = `${station_id}_${weather_type}_par`;
        $parButton.classList.add("button");
        $parButton.classList.add("is-primary");
        $parButton.classList.add("is-outlined");
        $parButton.addEventListener("click", ($event) => { this.handleEntryClick($event, station_id, weather_type, 'par'); });
        $pick.appendChild($parButton);


        // Create and append the "Under" button
        let $underButton = document.createElement('button');
        $underButton.textContent = 'Under';
        $underButton.id = `${station_id}_${weather_type}_under`;
        $underButton.classList.add("button");
        $underButton.classList.add("is-link");
        $underButton.classList.add("is-outlined");
        $underButton.addEventListener("click", ($event) => { this.handleEntryClick($event, station_id, weather_type, 'under'); });
        $pick.appendChild($underButton);
    }

    hideEntry() {
        let $entryScoreModal = document.getElementById("entry");
        if ($entryScoreModal.classList.contains('is-active')) {
            $entryScoreModal.classList.remove('is-active');
        }
    }

    clearEntry() {
        let $entryValues = document.getElementById('entryContent');
        if ($entryValues) {
            while ($entryValues.firstChild) {
                $entryValues.removeChild($entryValues.firstChild);
            }
        }
    }

    handleEntryClick($event, station_id, weather_type, selected_val) {
        const $buttons = document.getElementsByTagName('button');
        const pattern = `${station_id}_${weather_type}`;
        console.log(pattern);
        $event.target.classList.toggle('is-active');
        $event.target.classList.toggle('is-outlined');

        for (let $button of $buttons) {
            console.log($button);
            if ($button.id.includes(pattern) && $button.id != `${pattern}_${selected_val}`) {
                console.log('removing');
                $button.classList.remove('is-active');
                $button.classList.add('is-outlined');
            }
        }
        this.entry['submit'][station_id][weather_type] = selected_val;
    }

    submit($event) {
        setTimeout(() => {
            console.log("entry: ", this.entry);
            $event.target.classList.remove("is-loading");
            this.showSuccess();
        }, 300);
    }

    showSuccess() {
        let $success = document.getElementById('successMessage');
        $success.classList.remove('hidden');
        setTimeout(() => {
            $success.classList.add('hidden');
            this.hideEntry();
            this.clearEntry();
        }, 600);
    }
}

export { Entry };
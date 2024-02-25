

class WeatherData {
    constructor(base_url) {
        this.base_url = base_url
    }
    async get_stations() {
        let response = await fetch(`${this.base_url}/stations`);
        if (!response.ok) {
            throw new Error(`Failed to get stations, status: ${response.status}`)
        }
        return response.json()
    }

    async get_forecasts(station_ids, time_range) {
        let stations = station_ids.join(',');
        let response = await fetch(`${this.base_url}/stations/forecasts?start=${time_range.start}&end=${time_range.end}&station_ids=${stations}`);
        console.log(response);
        if (!response.ok) {
            throw new Error(`Failed to get stations, status: ${response.status}`)
        }
        return response.json()
    }

    async get_observations(station_ids, time_range) {
        let stations = station_ids.join(',');
        let response = await fetch(`${this.base_url}/stations/observations?start=${time_range.start}&end=${time_range.end}&station_ids=${stations}`)
        if (!response.ok) {
            throw new Error(`Failed to get stations, status: ${response.status}`)
        }
        return response.json()
    }
}

export { WeatherData };
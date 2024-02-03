export function displayEntries() {
  let $entriesDataTable = document.getElementById("entriesDataTable");
  let $tbody = $entriesDataTable.querySelector("tbody");
  if (!$tbody) {
    $tbody = document.createElement("tbody");
    $entriesDataTable.appendChild($tbody);
  }
  const stationMap = getStations()
  const entries = get_entries(); //TODO: will be async to a backend
  entries.forEach(competition => {
    let $row = document.createElement("tr");
    Object.keys(competition).forEach(key => {
      if (key !== "id") {
        const cell = document.createElement("td");
        let competititonDataValue = competition[key]
        if (key == 'cities') {
          let citiesText = ''
          // convert station code to human readable city name
          competititonDataValue.forEach((cityCode, index) => {
            let suffix = " - "
            if (index == competititonDataValue.length - 1) {
              suffix = ""
            }
            let cityName = stationMap[cityCode]
            citiesText = citiesText + cityName + suffix
          });
          competititonDataValue = citiesText
        }
        cell.textContent = competititonDataValue;
        $row.appendChild(cell);
      }
    });

    $row.addEventListener("click", () => {
      handleCompetitionClick($row, competition);
    });

    $tbody.appendChild($row);
  });

}

function get_entries() {
  const currentUTCDate = new Date();
  const twelveHoursFromNowUTCDate = new Date(currentUTCDate.getTime() + (12 * 3600 * 1000));
  const daysOfWeek = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
  const dayOfWeekAbbrev = daysOfWeek[twelveHoursFromNowUTCDate.getDay()];
  const hour = twelveHoursFromNowUTCDate.getHours();
  const liveAt = `${dayOfWeekAbbrev} ${hour}:00`;
  if (twelveHoursFromNowUTCDate < currentUTCDate) {
    liveAt = '-';
  }

  return [
    {
      "id": "671657f5-a437-453e-b9fa-4c50705dc607",
      "name": "Tiger Roar Challenge",
      "status": "live",
      "liveAt": liveAt,
      "entryFee": '$1',
      "totalPrizePoolAmt": "$60",
      "cities": ["KGRB", "KBOI", "KRAP", "KJAN", "KPWN"],
    },
    {
      "id": "70bc176c-4b30-46c0-8720-b1535d15ba34",
      "name": "Unicorn Gallop Grand Prix",
      "status": "running",
      "liveAt": liveAt,
      "entryFee": '$1',
      "totalPrizePoolAmt": "$20",
      "cities": ["KTPA", "KMIA", "KATL", "KSDF", "KBNA"],
    },
    {
      "id": "295ecf23-ef65-4708-9314-0fc7614b623d",
      "name": "Gryphon's Claws Tournament",
      "status": "completed",
      "liveAt": liveAt,
      "entryFee": '$1',
      "totalPrizePoolAmt": "$16",
      "cities": ["KBFM", "KBHM", "KMSY", "KLIT", "KMCI"],
    },
    {
      "id": "57bd5d1e-a7ae-422e-8673-81ebb6227bf8",
      "name": "Mermaid's Song Showcase",
      "status": "live",
      "liveAt": liveAt,
      "entryFee": '$1',
      "totalPrizePoolAmt": "$60",
      "cities": ["KSTL", "KCID", "KMSP", "KABQ", "KTUL"],
    },
    {
      "id": "cdf5b892-8d21-4264-ab65-9bc3e80e535d",
      "name": "Chimera Chase Extravaganza",
      "status": "live",
      "liveAt": liveAt,
      "entryFee": '$1',
      "totalPrizePoolAmt": "$20",
      "cities": ["KBOS", "KSEA", "KDEN", "KIND", "KCLT"],
    },
  ];
}

function handleCompetitionClick(row, competition) {
  console.log("should show something when you click an entry");
  const parentElement = row.parentElement;
  const rows = parentElement.querySelectorAll("tr");
  rows.forEach(currentRow => {
    if (currentRow != row) {
      currentRow.classList.remove('is-selected');
    }
  });
  row.classList.toggle('is-selected');
  let rowIsSelected = row.classList.contains('is-selected');
}

function getStations() {
  return {
      "KDCA": "Washington, D.C.",
      "KLGA": "New York City, NY",
      "KLAX": "Los Angeles, CA",
      "KORD": "Chicago, IL",
      "KPHL": "Philadelphia, PA",
      "KIAH": "Houston, TX",
      "KPHX": "Phoenix, AZ",
      "KSJC": "San Jose, CA",
      "KSFO": "San Francisco, CA",
      "KCMH": "Columbus, OH",
      "KDTW": "Detroit, MI",
      "KCLT": "Charlotte, NC",
      "KIND": "Indianapolis, IN",
      "KDEN": "Denver, CO",
      "KSEA": "Seattle, WA",
      "KBOS": "Boston, MA",
      "KLAS": "Las Vegas, NV",
      "KJAX": "Jacksonville, FL",
      "KIDA": "Idaho Falls, ID",
      "KPDX": "Portland, OR",
      "KGTF": "Great Falls, MT",
      "KJAC": "Jackson, WY",
      "KBIS": "Bismarck, ND",
      "KFSD": "Sioux Falls, SD",
      "KOMA": "Omaha, NE",
      "KICT": "Wichita, KS",
      "KTUL": "Tulsa, OK",
      "KABQ": "Albuquerque, NM",
      "KMSP": "Minneapolis, MN",
      "KCID": "Cedar Rapids, IA",
      "KSTL": "St. Louis, MO",
      "KMCI": "Kansas City, MO",
      "KLIT": "Little Rock, AR",
      "KMSY": "New Orleans, LA",
      "KBHM": "Birmingham, AL",
      "KBFM": "Mobile, AL",
      "KBNA": "Nashville, TN",
      "KSDF": "Louisville, KY",
      "KATL": "Atlanta, GA",
      "KMIA": "Miami, FL",
      "KTPA": "Tampa, FL",
      "KCHS": "Charleston, SC",
      "KCRW": "Charleston, WV",
      "KPIT": "Pittsburgh, PA",
      "KBUF": "Buffalo, NY",
      "KEWR": "Newark, NJ",
      "KBWI": "Baltimore, MD",
      "KRDU": "Raleigh, NC",
      "KHFD": "Hartford, CT",
      "KBTV": "Burlington, VT",
      "KMHT": "Manchester, NH",
      "KPWM": "Portland, ME",
      "KJAN": "Jackson, MS",
      "KRAP": "Rapid City, SD",
      "KBOI": "Boise, ID",
      "KGRB": "Green Bay, WI",
  };
}
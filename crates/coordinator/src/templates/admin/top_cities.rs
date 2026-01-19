//! Top 200 US cities by population with their nearby airport ICAO codes.
//! Used to filter weather stations to only show major metropolitan areas.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Map of city names to their associated airport ICAO codes.
/// Cities may have multiple airports serving the metropolitan area.
/// City names include state abbreviation for disambiguation where needed.
pub static TOP_CITIES: LazyLock<HashMap<&'static str, Vec<&'static str>>> = LazyLock::new(|| {
    HashMap::from([
        // 1-10
        ("New York, NY", vec!["KJFK", "KLGA", "KEWR", "KTEB"]),
        (
            "Los Angeles, CA",
            vec!["KLAX", "KBUR", "KLGB", "KSNA", "KVNY"],
        ),
        ("Chicago, IL", vec!["KORD", "KMDW", "KPWK"]),
        ("Houston, TX", vec!["KIAH", "KHOU"]),
        ("Phoenix, AZ", vec!["KPHX", "KDVT", "KGYR"]),
        ("Philadelphia, PA", vec!["KPHL", "KPNE"]),
        ("San Antonio, TX", vec!["KSAT", "KSKF"]),
        ("San Diego, CA", vec!["KSAN", "KMYF", "KSEE"]),
        ("Dallas, TX", vec!["KDFW", "KDAL", "KADS"]),
        ("San Jose, CA", vec!["KSJC", "KRHV"]),
        // 11-20
        ("Austin, TX", vec!["KAUS", "KEDC"]),
        ("Jacksonville, FL", vec!["KJAX", "KCRG", "KVQQ"]),
        ("Fort Worth, TX", vec!["KDFW", "KAFW", "KFTW"]),
        ("Columbus, OH", vec!["KCMH", "KOSU", "KLCK"]),
        ("Charlotte, NC", vec!["KCLT", "KJQF"]),
        ("San Francisco, CA", vec!["KSFO", "KOAK"]),
        ("Indianapolis, IN", vec!["KIND", "KEYE"]),
        ("Seattle, WA", vec!["KSEA", "KBFI", "KPAE"]),
        ("Denver, CO", vec!["KDEN", "KAPA", "KBJC"]),
        ("Washington, DC", vec!["KDCA", "KIAD", "KBWI"]),
        // 21-30
        ("Boston, MA", vec!["KBOS", "KBED"]),
        ("El Paso, TX", vec!["KELP"]),
        ("Nashville, TN", vec!["KBNA", "KJWN"]),
        ("Detroit, MI", vec!["KDTW", "KDET", "KYIP"]),
        ("Oklahoma City, OK", vec!["KOKC", "KPWA", "KTIK"]),
        ("Portland, OR", vec!["KPDX", "KHIO", "KTTD"]),
        ("Las Vegas, NV", vec!["KLAS", "KVGT", "KHND"]),
        ("Memphis, TN", vec!["KMEM"]),
        ("Louisville, KY", vec!["KSDF", "KLOU"]),
        ("Baltimore, MD", vec!["KBWI", "KMTN"]),
        // 31-40
        ("Milwaukee, WI", vec!["KMKE", "KMWC"]),
        ("Albuquerque, NM", vec!["KABQ"]),
        ("Tucson, AZ", vec!["KTUS", "KDMA"]),
        ("Fresno, CA", vec!["KFAT", "KFCH"]),
        ("Mesa, AZ", vec!["KFFZ", "KIWA"]),
        ("Sacramento, CA", vec!["KSMF", "KSAC", "KMCC", "KMHR"]),
        ("Atlanta, GA", vec!["KATL", "KPDK", "KFTY"]),
        ("Kansas City, MO", vec!["KMCI", "KMKC", "KOJC"]),
        ("Colorado Springs, CO", vec!["KCOS"]),
        ("Omaha, NE", vec!["KOMA", "KOFF"]),
        // 41-50
        ("Raleigh, NC", vec!["KRDU"]),
        ("Miami, FL", vec!["KMIA", "KFLL", "KOPF", "KTMB"]),
        ("Long Beach, CA", vec!["KLGB"]),
        ("Virginia Beach, VA", vec!["KORF", "KNGU"]),
        ("Oakland, CA", vec!["KOAK"]),
        ("Minneapolis, MN", vec!["KMSP", "KFCM", "KSTP"]),
        ("Tulsa, OK", vec!["KTUL", "KRVS"]),
        ("Tampa, FL", vec!["KTPA", "KPIE"]),
        ("Arlington, TX", vec!["KDFW", "KGKY"]),
        ("New Orleans, LA", vec!["KMSY", "KNEW"]),
        // 51-60
        ("Wichita, KS", vec!["KICT", "KBEC"]),
        ("Bakersfield, CA", vec!["KBFL"]),
        ("Cleveland, OH", vec!["KCLE", "KCGF", "KBKL"]),
        ("Aurora, CO", vec!["KAPA"]),
        ("Anaheim, CA", vec!["KSNA", "KFUL"]),
        ("Honolulu, HI", vec!["PHNL", "PHHI"]),
        ("Santa Ana, CA", vec!["KSNA"]),
        ("Riverside, CA", vec!["KRAL", "KRIR"]),
        ("Corpus Christi, TX", vec!["KCRP"]),
        ("Lexington, KY", vec!["KLEX"]),
        // 61-70
        ("Henderson, NV", vec!["KHND"]),
        ("Stockton, CA", vec!["KSCK"]),
        ("Saint Paul, MN", vec!["KSTP"]),
        ("Cincinnati, OH", vec!["KCVG", "KLUK"]),
        ("St. Louis, MO", vec!["KSTL", "KSUS", "KCPS"]),
        ("Pittsburgh, PA", vec!["KPIT", "KAGC"]),
        ("Greensboro, NC", vec!["KGSO", "KPTN"]),
        ("Lincoln, NE", vec!["KLNK"]),
        ("Anchorage, AK", vec!["PANC", "PAED", "PAMR"]),
        ("Plano, TX", vec!["KADS"]),
        // 71-80
        ("Orlando, FL", vec!["KMCO", "KORL", "KSFB"]),
        ("Irvine, CA", vec!["KSNA"]),
        ("Newark, NJ", vec!["KEWR"]),
        ("Durham, NC", vec!["KRDU"]),
        ("Chula Vista, CA", vec!["KSDM"]),
        ("Toledo, OH", vec!["KTOL"]),
        ("Fort Wayne, IN", vec!["KFWA"]),
        ("St. Petersburg, FL", vec!["KPIE", "KSPG"]),
        ("Laredo, TX", vec!["KLRD"]),
        ("Jersey City, NJ", vec!["KEWR", "KJRB"]),
        // 81-90
        ("Chandler, AZ", vec!["KCHD"]),
        ("Madison, WI", vec!["KMSN"]),
        ("Lubbock, TX", vec!["KLBB"]),
        ("Scottsdale, AZ", vec!["KSDL"]),
        ("Reno, NV", vec!["KRNO"]),
        ("Buffalo, NY", vec!["KBUF"]),
        ("Gilbert, AZ", vec!["KCHD", "KIWA"]),
        ("Glendale, AZ", vec!["KGEU"]),
        ("North Las Vegas, NV", vec!["KVGT"]),
        ("Winston-Salem, NC", vec!["KINT"]),
        // 91-100
        ("Norfolk, VA", vec!["KORF"]),
        ("Chesapeake, VA", vec!["KORF"]),
        ("Fremont, CA", vec!["KOAK"]),
        ("Garland, TX", vec!["KDAL"]),
        ("Irving, TX", vec!["KDFW"]),
        ("Hialeah, FL", vec!["KOPF"]),
        ("Richmond, VA", vec!["KRIC"]),
        ("Boise, ID", vec!["KBOI"]),
        ("Spokane, WA", vec!["KGEG", "KSFF"]),
        ("Baton Rouge, LA", vec!["KBTR"]),
        // 101-110
        ("Tacoma, WA", vec!["KSEA", "KTIW"]),
        ("San Bernardino, CA", vec!["KSBD"]),
        ("Modesto, CA", vec!["KMOD"]),
        ("Fontana, CA", vec!["KONT"]),
        ("Des Moines, IA", vec!["KDSM"]),
        ("Moreno Valley, CA", vec!["KRAL"]),
        ("Santa Clarita, CA", vec!["KVNY"]),
        ("Fayetteville, NC", vec!["KFAY"]),
        ("Birmingham, AL", vec!["KBHM"]),
        ("Rochester, NY", vec!["KROC"]),
        // 111-120
        ("Oxnard, CA", vec!["KOXR"]),
        ("Port St. Lucie, FL", vec!["KFPR"]),
        ("Grand Rapids, MI", vec!["KGRR"]),
        ("Huntsville, AL", vec!["KHSV"]),
        ("Salt Lake City, UT", vec!["KSLC"]),
        ("Frisco, TX", vec!["KDFW"]),
        ("Yonkers, NY", vec!["KHPN"]),
        ("Amarillo, TX", vec!["KAMA"]),
        ("Glendale, CA", vec!["KBUR"]),
        ("Worcester, MA", vec!["KORH"]),
        // 121-130
        ("McKinney, TX", vec!["KTKI"]),
        ("Little Rock, AR", vec!["KLIT"]),
        ("Augusta, GA", vec!["KAGS"]),
        ("Grand Prairie, TX", vec!["KGPM"]),
        ("Montgomery, AL", vec!["KMGM"]),
        ("Akron, OH", vec!["KCAK"]),
        ("Huntington Beach, CA", vec!["KSNA"]),
        ("Tallahassee, FL", vec!["KTLH"]),
        ("Shreveport, LA", vec!["KSHV"]),
        ("Overland Park, KS", vec!["KOJC"]),
        // 131-140
        ("Mobile, AL", vec!["KMOB", "KBFM"]),
        ("Tempe, AZ", vec!["KPHX"]),
        ("Cape Coral, FL", vec!["KRSW"]),
        ("Knoxville, TN", vec!["KTYS"]),
        ("Providence, RI", vec!["KPVD"]),
        ("Chattanooga, TN", vec!["KCHA"]),
        ("Brownsville, TX", vec!["KBRO"]),
        ("Fort Lauderdale, FL", vec!["KFLL", "KFXE"]),
        ("Newport News, VA", vec!["KPHF"]),
        ("Ontario, CA", vec!["KONT"]),
        // 141-150
        ("Cary, NC", vec!["KRDU"]),
        ("Elk Grove, CA", vec!["KSAC"]),
        ("Santa Rosa, CA", vec!["KSTS"]),
        ("Rancho Cucamonga, CA", vec!["KONT"]),
        ("Eugene, OR", vec!["KEUG"]),
        ("Oceanside, CA", vec!["KCRQ"]),
        ("Salem, OR", vec!["KSLE"]),
        ("Pembroke Pines, FL", vec!["KFLL"]),
        ("Garden Grove, CA", vec!["KSNA"]),
        ("Fort Collins, CO", vec!["KFNL"]),
        // 151-160
        ("Springfield, MO", vec!["KSGF"]),
        ("Peoria, IL", vec!["KPIA"]),
        ("Corona, CA", vec!["KRAL"]),
        ("Clarksville, TN", vec!["KCKV"]),
        ("Sioux Falls, SD", vec!["KFSD"]),
        ("Lancaster, CA", vec!["KPMD"]),
        ("Palmdale, CA", vec!["KPMD"]),
        ("Hayward, CA", vec!["KHWD"]),
        ("Salinas, CA", vec!["KSNS"]),
        ("Pomona, CA", vec!["KPOC"]),
        // 161-170
        ("Alexandria, VA", vec!["KDCA"]),
        ("Escondido, CA", vec!["KSEE"]),
        ("Sunnyvale, CA", vec!["KNUQ"]),
        ("Lakewood, CO", vec!["KBJC"]),
        ("Kansas City, KS", vec!["KMKC"]),
        ("Hollywood, FL", vec!["KFLL"]),
        ("Surprise, AZ", vec!["KLUF"]),
        ("Naperville, IL", vec!["KDPA"]),
        ("Pasadena, TX", vec!["KHOU"]),
        ("Macon, GA", vec!["KMCN"]),
        // 171-180
        ("Bridgeport, CT", vec!["KBDR"]),
        ("Rockford, IL", vec!["KRFD"]),
        ("Joliet, IL", vec!["KJOT"]),
        ("Paterson, NJ", vec!["KTEB"]),
        ("Savannah, GA", vec!["KSAV"]),
        ("Torrance, CA", vec!["KTOA"]),
        ("Syracuse, NY", vec!["KSYR"]),
        ("McAllen, TX", vec!["KMFE"]),
        ("Bellevue, WA", vec!["KBFI"]),
        ("Mesquite, TX", vec!["KDAL"]),
        // 181-190
        ("Pasadena, CA", vec!["KBUR"]),
        ("Miramar, FL", vec!["KNKX"]),
        ("Dayton, OH", vec!["KDAY", "KMGY"]),
        ("Fullerton, CA", vec!["KFUL"]),
        ("Olathe, KS", vec!["KOJC"]),
        ("Orange, CA", vec!["KSNA"]),
        ("Killeen, TX", vec!["KGRK"]),
        ("Hampton, VA", vec!["KLFI"]),
        ("Waco, TX", vec!["KACT"]),
        ("Columbia, SC", vec!["KCAE"]),
        // 191-200
        ("Thornton, CO", vec!["KBJC"]),
        ("Carrollton, TX", vec!["KADS"]),
        ("Midland, TX", vec!["KMAF"]),
        ("Charleston, SC", vec!["KCHS"]),
        ("Denton, TX", vec!["KDTO"]),
        ("West Valley City, UT", vec!["KSLC"]),
        ("Visalia, CA", vec!["KVIS"]),
        ("Murfreesboro, TN", vec!["KMBT"]),
        ("Gainesville, FL", vec!["KGNV"]),
        ("New Haven, CT", vec!["KHVN"]),
        // Additional cities to ensure all states are covered
        ("Wilmington, DE", vec!["KILG"]),
        ("Portland, ME", vec!["KPWM"]),
        ("Jackson, MS", vec!["KJAN"]),
        ("Billings, MT", vec!["KBIL"]),
        ("Fargo, ND", vec!["KFAR"]),
        ("Manchester, NH", vec!["KMHT"]),
        ("Burlington, VT", vec!["KBTV"]),
        ("Charleston, WV", vec!["KCRW"]),
        ("Cheyenne, WY", vec!["KCYS"]),
    ])
});

/// Returns a set of all unique ICAO codes from the top 200 cities.
pub fn get_allowed_station_ids() -> std::collections::HashSet<&'static str> {
    TOP_CITIES
        .values()
        .flat_map(|codes| codes.iter().copied())
        .collect()
}

/// Check if a station ID is in the allowed list.
pub fn is_allowed_station(station_id: &str) -> bool {
    get_allowed_station_ids().contains(station_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_top_cities_count() {
        // 200 top cities + 9 additional to cover all 50 states + DC
        assert_eq!(TOP_CITIES.len(), 209);
    }

    #[test]
    fn test_allowed_stations() {
        assert!(is_allowed_station("KJFK"));
        assert!(is_allowed_station("KLAX"));
        assert!(!is_allowed_station("KXYZ"));
    }
}

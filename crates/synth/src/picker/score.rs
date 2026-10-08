//! Scoring candidates on their forecast and choosing a competition's stations from the scores.
//!
//! Nothing here talks to the oracle or the database, so the choice can be tested on fixtures.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::oracle::{Forecast, StationInfo};
use super::Weights;

/// Miles per hour in a knot, the unit the oracle forecasts wind in.
const MPH_PER_KNOT: f64 = 1.150_78;
/// At or above this high, in °F, a station's forecast is extreme.
const VERY_HOT_F: i64 = 95;
/// At or below this low, in °F, a station's forecast is extreme.
const VERY_COLD_F: i64 = 20;
/// Precipitation amounts count up to this many inches; beyond it a station is wet enough.
const PRECIP_AMOUNT_CAP_IN: f64 = 2.0;
/// The fewest kilometres between two stations picked independently of each other.
pub const MIN_SPREAD_KM: f64 = 300.0;
/// A known airport within this fraction of a better unknown station's score ranks above it.
const KNOWN_AIRPORT_BAND: f64 = 0.10;

/// What a station's forecast for the window holds, in the units people read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Weather {
    /// The highest high less the lowest low, °F.
    pub swing_f: i64,
    pub high_f: i64,
    pub low_f: i64,
    /// The strongest forecast wind, mph.
    pub wind_mph: f64,
    /// The highest chance of precipitation in any period, percent.
    pub precip_chance: i64,
    /// Rain, snow and ice forecast across the window, inches.
    pub rain_in: f64,
    pub snow_in: f64,
    pub ice_in: f64,
}

impl Weather {
    /// The window's weather from a station's forecast rows, None without any rows.
    pub fn of(rows: &[&Forecast]) -> Option<Self> {
        let high_f = rows.iter().map(|row| row.temp_high).max()?;
        let low_f = rows.iter().map(|row| row.temp_low).min()?;
        let sum = |amount: fn(&Forecast) -> Option<f64>| -> f64 {
            rows.iter().filter_map(|row| amount(row)).sum()
        };
        Some(Self {
            swing_f: (high_f - low_f).max(0),
            high_f,
            low_f,
            wind_mph: rows
                .iter()
                .filter_map(|row| row.wind_speed)
                .max()
                .unwrap_or(0) as f64
                * MPH_PER_KNOT,
            precip_chance: rows
                .iter()
                .filter_map(|row| row.precip_chance)
                .max()
                .unwrap_or(0),
            rain_in: sum(|row| row.rain_amt),
            snow_in: sum(|row| row.snow_amt),
            ice_in: sum(|row| row.ice_amt),
        })
    }

    /// The chance of precipitation, and how much falls, each counting up to one.
    fn precipitation(&self) -> f64 {
        let amount = (self.rain_in + self.snow_in + self.ice_in).clamp(0.0, PRECIP_AMOUNT_CAP_IN);
        self.precip_chance.clamp(0, 100) as f64 / 100.0 + amount / PRECIP_AMOUNT_CAP_IN
    }

    /// One for each of very hot, very cold, any snow and any ice.
    fn extremes(&self) -> f64 {
        [
            self.high_f >= VERY_HOT_F,
            self.low_f <= VERY_COLD_F,
            self.snow_in > 0.0,
            self.ice_in > 0.0,
        ]
        .into_iter()
        .filter(|extreme| *extreme)
        .count() as f64
    }

    /// The extremes in words, for the explanation.
    pub fn extreme_words(&self) -> Vec<&'static str> {
        [
            (self.high_f >= VERY_HOT_F, "very hot"),
            (self.low_f <= VERY_COLD_F, "very cold"),
            (self.snow_in > 0.0, "snow"),
            (self.ice_in > 0.0, "ice"),
        ]
        .into_iter()
        .filter_map(|(extreme, word)| extreme.then_some(word))
        .collect()
    }
}

/// Each component scaled to 0..=1 across the candidates, and the weighted score.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Components {
    pub swing: f64,
    pub wind: f64,
    pub precip: f64,
    pub extremes: f64,
    pub score: f64,
}

/// A candidate with its forecast scored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scored {
    pub station: StationInfo,
    pub weather: Weather,
    pub components: Components,
}

impl Scored {
    pub fn known_airport(&self) -> bool {
        self.station.known_airport()
    }
}

/// Score each candidate with a forecast for the window, dropping those without one. Each
/// component is scaled from the lowest to the highest value among the candidates, so a unit of
/// one does not outweigh another; a component every candidate shares counts nothing.
pub fn score(candidates: &[StationInfo], forecasts: &[Forecast], weights: &Weights) -> Vec<Scored> {
    let mut rows: HashMap<&str, Vec<&Forecast>> = HashMap::new();
    for forecast in forecasts {
        rows.entry(forecast.station_id.as_str())
            .or_default()
            .push(forecast);
    }
    let forecast: Vec<(StationInfo, Weather)> = candidates
        .iter()
        .filter_map(|station| {
            let weather = Weather::of(rows.get(station.station_id.as_str())?)?;
            Some((station.clone(), weather))
        })
        .collect();
    let raw: Vec<[f64; 4]> = forecast
        .iter()
        .map(|(_, weather)| {
            [
                weather.swing_f as f64,
                weather.wind_mph,
                weather.precipitation(),
                weather.extremes(),
            ]
        })
        .collect();
    let ranges: Vec<(f64, f64)> = (0..4)
        .map(|component| {
            raw.iter()
                .fold((f64::MAX, f64::MIN), |(low, high), values| {
                    (low.min(values[component]), high.max(values[component]))
                })
        })
        .collect();
    let scale = |component: usize, value: f64| {
        let (low, high) = ranges[component];
        if high > low {
            (value - low) / (high - low)
        } else {
            0.0
        }
    };
    let total = weights.total();
    forecast
        .into_iter()
        .zip(raw)
        .map(|((station, weather), values)| {
            let (swing, wind, precip, extremes) = (
                scale(0, values[0]),
                scale(1, values[1]),
                scale(2, values[2]),
                scale(3, values[3]),
            );
            let score = (weights.swing * swing
                + weights.wind * wind
                + weights.precip * precip
                + weights.extremes * extremes)
                / total;
            Scored {
                station,
                weather,
                components: Components {
                    swing,
                    wind,
                    precip,
                    extremes,
                    score,
                },
            }
        })
        .collect()
}

/// Why a station is in the competition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The most going on.
    Leader,
    /// Near the leader, in the same weather.
    Cluster,
    /// Too few were near the leader; the best of the rest.
    Outside,
    /// Picked on its own score, far enough from the others.
    Spread,
    /// Too few were far enough apart; the best of the rest.
    Crowded,
    /// From the lane's own list, for want of scored candidates.
    Fallback,
}

/// A station picked, with its forecast and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Picked {
    pub station_id: String,
    pub role: Role,
    /// None for a station picked from the lane's list without a forecast.
    pub scored: Option<Scored>,
    /// From the leader, when both are located.
    pub km_from_leader: Option<f64>,
}

/// Great-circle distance in kilometres.
pub fn distance_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6371.0;
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let h = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().asin()
}

fn between(a: &Scored, b: &Scored) -> Option<f64> {
    Some(distance_km(a.station.location()?, b.station.location()?))
}

/// Order the scored candidates best first. With `prefer_known_airports`, a station with an IATA
/// code ranks as if it scored a tenth of the top score more, so it beats an unknown station
/// unless that one is more than 10% better.
pub fn rank(mut scored: Vec<Scored>, prefer_known_airports: bool) -> Vec<Scored> {
    let top = scored
        .iter()
        .map(|candidate| candidate.components.score)
        .fold(0.0, f64::max);
    let band = |candidate: &Scored| {
        let bonus = if prefer_known_airports && candidate.known_airport() {
            KNOWN_AIRPORT_BAND * top
        } else {
            0.0
        };
        candidate.components.score + bonus
    };
    scored.sort_by(|a, b| {
        a.station
            .coverage
            .as_ref()
            .is_none_or(|c| c.missed_report())
            .cmp(
                &b.station
                    .coverage
                    .as_ref()
                    .is_none_or(|c| c.missed_report()),
            )
            .then_with(|| band(b).total_cmp(&band(a)))
            .then_with(|| {
                (prefer_known_airports && b.known_airport())
                    .cmp(&(prefer_known_airports && a.known_airport()))
            })
            .then_with(|| a.station.station_id.cmp(&b.station.station_id))
    });
    scored
}

/// Choose up to `count` of the ranked candidates: the first leads; with `cluster_km` the others
/// are the best within that distance of it, then the best anywhere if too few are; without, the
/// best that are each at least [`MIN_SPREAD_KM`] from every other pick, then the best of the
/// rest if too few are. A candidate whose place is unknown is never near and never too close.
pub fn choose(ranked: &[Scored], count: usize, cluster_km: f64) -> Vec<Picked> {
    let Some(leader) = ranked.first() else {
        return Vec::new();
    };
    let mut picked = vec![Picked {
        station_id: leader.station.station_id.clone(),
        role: Role::Leader,
        scored: Some(leader.clone()),
        km_from_leader: None,
    }];
    let mut taken: HashSet<usize> = HashSet::from([0]);
    let pick = |picked: &mut Vec<Picked>, taken: &mut HashSet<usize>, index: usize, role| {
        taken.insert(index);
        picked.push(Picked {
            station_id: ranked[index].station.station_id.clone(),
            role,
            scored: Some(ranked[index].clone()),
            km_from_leader: between(leader, &ranked[index]),
        });
    };
    let (near, rest) = if cluster_km > 0.0 {
        (Role::Cluster, Role::Outside)
    } else {
        (Role::Spread, Role::Crowded)
    };
    for (index, candidate) in ranked.iter().enumerate().skip(1) {
        if picked.len() >= count {
            break;
        }
        let fits = if cluster_km > 0.0 {
            between(leader, candidate).is_some_and(|km| km <= cluster_km)
        } else {
            taken.iter().all(|&other| {
                between(&ranked[other], candidate).is_none_or(|km| km >= MIN_SPREAD_KM)
            })
        };
        if fits {
            pick(&mut picked, &mut taken, index, near);
        }
    }
    for index in 1..ranked.len() {
        if picked.len() >= count {
            break;
        }
        if !taken.contains(&index) {
            pick(&mut picked, &mut taken, index, rest);
        }
    }
    picked
}

/// One line on why each station was picked, as the run's page shows it.
pub fn explain(picked: &[Picked]) -> String {
    picked
        .iter()
        .map(|pick| {
            let mut words = vec![pick.station_id.clone()];
            if let Some(km) = pick.km_from_leader {
                words.push(format!("{km:.0} km away"));
            }
            if let Some(scored) = &pick.scored {
                let weather = &scored.weather;
                let mut facts = vec![
                    format!("swing {} °F", weather.swing_f),
                    format!("wind {:.0} mph", weather.wind_mph),
                    format!("precip {}%", weather.precip_chance),
                ];
                facts.extend(weather.extreme_words().into_iter().map(str::to_string));
                facts.push(format!("score {:.2}", scored.components.score));
                if let Some(coverage) = &scored.station.coverage {
                    facts.push(format!(
                        "coverage {}/{} clean days, latest {}h gap at most {} min",
                        coverage.clean_days,
                        coverage.days_checked,
                        coverage.recent_window_hours,
                        coverage.max_report_gap_seconds.div_ceil(60)
                    ));
                }
                words.push(facts.join(", "));
            }
            let role = match pick.role {
                Role::Leader => "leader",
                Role::Cluster => "near the leader",
                Role::Outside => "best outside the cluster",
                Role::Spread => "spread",
                Role::Crowded => "closer than 300 km, too few far apart",
                Role::Fallback => "from the lane's list",
            };
            format!("{}: {role}", words.join(" "))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::fixtures::{forecast, station};

    fn scored(id: &str, iata: &str, at: (f64, f64), score: f64) -> Scored {
        Scored {
            station: station(id, iata, at),
            weather: Weather::default(),
            components: Components {
                score,
                ..Default::default()
            },
        }
    }

    #[test]
    fn coverage_margin_ranks_before_weather_and_airport_bonus() {
        let mut stormy = scored("KGAP", "GAP", (0.0, 0.0), 1.0);
        stormy
            .station
            .coverage
            .as_mut()
            .unwrap()
            .max_report_gap_seconds = 120 * 60;
        let steady = scored("KCLN", "", (0.0, 0.0), 0.1);
        let ranked = rank(vec![stormy, steady], true);
        assert_eq!(ranked[0].station.station_id, "KCLN");
    }

    const DENVER: (f64, f64) = (39.86, -104.67);
    const COLORADO_SPRINGS: (f64, f64) = (38.81, -104.70);
    const CHEYENNE: (f64, f64) = (41.16, -104.81);
    const CHICAGO: (f64, f64) = (41.98, -87.90);
    const NEW_YORK: (f64, f64) = (40.64, -73.78);
    const AURORA: (f64, f64) = (39.57, -104.85);

    #[test]
    fn components_are_scaled_across_candidates_and_weighted() {
        let candidates = [
            station("KDEN", "DEN", DENVER),
            station("KORD", "ORD", CHICAGO),
            station("KJFK", "JFK", NEW_YORK),
            station("KNONE", "", AURORA),
        ];
        let mut snowy = forecast("KDEN", 15, 46, 30, 70);
        snowy.snow_amt = Some(3.0);
        let forecasts = [
            // Two periods for Denver: the window's high, low and wind are across both.
            forecast("KDEN", 30, 40, 10, 20),
            snowy,
            forecast("KORD", 50, 60, 20, 0),
            forecast("KJFK", 55, 60, 10, 0),
        ];
        let weights = Weights::default();
        let scored = score(&candidates, &forecasts, &weights);
        assert_eq!(scored.len(), 3, "no forecast, no score");
        let den = &scored[0];
        assert_eq!(den.weather.swing_f, 31);
        assert_eq!(den.weather.low_f, 15);
        assert!((den.weather.wind_mph - 30.0 * MPH_PER_KNOT).abs() < 1e-9);
        assert_eq!(den.weather.precip_chance, 70);
        assert_eq!(den.weather.extreme_words(), ["very cold", "snow"]);
        // Denver has the most of everything: one in each component, the whole weight.
        assert_eq!(den.components.swing, 1.0);
        assert_eq!(den.components.extremes, 1.0);
        assert!((den.components.score - 1.0).abs() < 1e-9);
        // Chicago: swing 10 of 5..31, wind 20 of 10..30, no rain, no extremes.
        let ord = &scored[1];
        assert!((ord.components.swing - 5.0 / 26.0).abs() < 1e-9);
        assert!((ord.components.wind - 0.5).abs() < 1e-9);
        assert_eq!(ord.components.precip, 0.0);
        let expected = 0.35 * 5.0 / 26.0 + 0.25 * 0.5;
        assert!((ord.components.score - expected).abs() < 1e-9);
        // New York is the least of everything.
        assert_eq!(scored[2].components.score, 0.0);

        // Weights need not add up to one; they are shares of their total.
        let doubled = Weights {
            swing: 0.70,
            wind: 0.50,
            precip: 0.50,
            extremes: 0.30,
        };
        let again = score(&candidates, &forecasts, &doubled);
        assert!((again[1].components.score - expected).abs() < 1e-9);

        // A component every candidate shares counts for nothing.
        let flat = score(
            &candidates[1..3],
            &[
                forecast("KORD", 50, 60, 10, 0),
                forecast("KJFK", 50, 60, 10, 0),
            ],
            &weights,
        );
        assert!(flat
            .iter()
            .all(|candidate| candidate.components.score == 0.0));

        let line = explain(&choose(&rank(scored, true), 1, 0.0));
        assert_eq!(
            line,
            "KDEN swing 31 °F, wind 35 mph, precip 70%, very cold, snow, score 1.00, coverage 3/3 clean days, latest 24h gap at most 60 min: leader"
        );
    }

    #[test]
    fn a_known_airport_within_a_tenth_of_the_top_leads() {
        let ranked = rank(
            vec![
                scored("KXYZ", "", DENVER, 1.0),
                scored("KORD", "ORD", CHICAGO, 0.91),
            ],
            true,
        );
        assert_eq!(ranked[0].station.station_id, "KORD");
        let ranked = rank(
            vec![
                scored("KXYZ", "", DENVER, 1.0),
                scored("KORD", "ORD", CHICAGO, 0.89),
            ],
            true,
        );
        assert_eq!(ranked[0].station.station_id, "KXYZ", "more than 10% better");
        let ranked = rank(
            vec![
                scored("KXYZ", "", DENVER, 1.0),
                scored("KORD", "ORD", CHICAGO, 0.95),
            ],
            false,
        );
        assert_eq!(ranked[0].station.station_id, "KXYZ", "no preference");
        // Equal scores: the known airport, then by id.
        let ranked = rank(
            vec![
                scored("KAAA", "", DENVER, 0.0),
                scored("KZZZ", "ZZZ", CHICAGO, 0.0),
                scored("KBBB", "BBB", NEW_YORK, 0.0),
            ],
            true,
        );
        let ids: Vec<_> = ranked
            .iter()
            .map(|c| c.station.station_id.as_str())
            .collect();
        assert_eq!(ids, ["KBBB", "KZZZ", "KAAA"]);
    }

    #[test]
    fn the_others_come_from_near_the_leader_then_anywhere() {
        let ranked = rank(
            vec![
                scored("KDEN", "DEN", DENVER, 1.0),
                scored("KJFK", "JFK", NEW_YORK, 0.9),
                scored("KCOS", "COS", COLORADO_SPRINGS, 0.5),
                scored("KCYS", "CYS", CHEYENNE, 0.4),
                scored("KORD", "ORD", CHICAGO, 0.8),
            ],
            true,
        );
        let picked = choose(&ranked, 3, 600.0);
        let ids: Vec<_> = picked.iter().map(|p| p.station_id.as_str()).collect();
        assert_eq!(
            ids,
            ["KDEN", "KCOS", "KCYS"],
            "New York scores more, but is far"
        );
        assert_eq!(picked[1].role, Role::Cluster);
        let km = picked[1].km_from_leader.unwrap();
        assert!((110.0..125.0).contains(&km), "{km}");

        // Only one is near: the best of the rest fills.
        let picked = choose(&ranked, 4, 130.0);
        let ids: Vec<_> = picked.iter().map(|p| p.station_id.as_str()).collect();
        assert_eq!(ids, ["KDEN", "KCOS", "KJFK", "KORD"]);
        assert_eq!(
            picked.iter().map(|p| p.role).collect::<Vec<_>>(),
            [Role::Leader, Role::Cluster, Role::Outside, Role::Outside]
        );
        assert!(explain(&picked).contains("KCOS 117 km away"));
    }

    #[test]
    fn independent_picks_are_at_least_300_km_apart() {
        let ranked = rank(
            vec![
                scored("KDEN", "DEN", DENVER, 1.0),
                scored("KAPA", "APA", AURORA, 0.95),
                scored("KCOS", "COS", COLORADO_SPRINGS, 0.9),
                scored("KORD", "ORD", CHICAGO, 0.5),
                scored("KJFK", "JFK", NEW_YORK, 0.4),
            ],
            true,
        );
        let picked = choose(&ranked, 3, 0.0);
        let ids: Vec<_> = picked.iter().map(|p| p.station_id.as_str()).collect();
        assert_eq!(
            ids,
            ["KDEN", "KORD", "KJFK"],
            "not three of Denver's suburbs"
        );
        assert!(picked[1..].iter().all(|p| p.role == Role::Spread));
        for a in &picked {
            for b in &picked {
                if a.station_id != b.station_id {
                    let km = between(a.scored.as_ref().unwrap(), b.scored.as_ref().unwrap());
                    assert!(km.unwrap() >= MIN_SPREAD_KM);
                }
            }
        }
        // Too few far apart: the closest of the best fill in.
        let picked = choose(&ranked[..3], 3, 0.0);
        assert_eq!(
            picked.iter().map(|p| p.role).collect::<Vec<_>>(),
            [Role::Leader, Role::Crowded, Role::Crowded]
        );
    }
}

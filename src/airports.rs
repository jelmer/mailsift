//! IATA airport code to IANA time zone, from `data/airport-timezones.tsv`.

use std::collections::HashMap;
use std::sync::OnceLock;

use chrono_tz::Tz;

const TABLE: &str = include_str!("../data/airport-timezones.tsv");

/// The time zone of the airport with this IATA code, if it is known.
pub fn time_zone(iata: &str) -> Option<Tz> {
    static ZONES: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    let zones = ZONES.get_or_init(|| {
        TABLE
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| line.split_once('\t'))
            .collect()
    });
    zones
        .get(iata.trim().to_ascii_uppercase().as_str())?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_zone_in_the_table_is_known_to_chrono_tz() {
        for line in TABLE.lines().filter(|line| !line.starts_with('#')) {
            let (code, zone) = line.split_once('\t').expect("code<TAB>zone");
            assert!(zone.parse::<Tz>().is_ok(), "{code}: unknown zone {zone}");
        }
    }

    #[test]
    fn looks_up_by_code_in_any_case() {
        assert_eq!(time_zone("HNL"), Some(chrono_tz::Pacific::Honolulu));
        assert_eq!(time_zone(" lax "), Some(chrono_tz::America::Los_Angeles));
        assert_eq!(time_zone("ZZZ"), None);
    }
}

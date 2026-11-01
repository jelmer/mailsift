"""Regenerate airport-timezones.tsv: IATA code to IANA time zone.

    uv run --with airportsdata python data/gen-airport-timezones.py > data/airport-timezones.tsv

Source: the airportsdata package (MIT licence), https://github.com/mborsetti/airportsdata
"""

import importlib.metadata

import airportsdata

airports = airportsdata.load("IATA")
print(f"# IATA airport code to IANA time zone, from airportsdata {importlib.metadata.version('airportsdata')} (MIT).")
print("# Regenerate with data/gen-airport-timezones.py.")
for code in sorted(airports):
    tz = airports[code]["tz"]
    if tz:
        print(f"{code}\t{tz}")

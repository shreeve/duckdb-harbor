#!/usr/bin/env bash
#
# fixture.sh — build the database the suites run against.
#
#   test/scripts/fixture.sh sample.duckdb
#   LABS_DATA=/path/to/data test/scripts/fixture.sh sample.duckdb    # from real CSVs
#
# The suites need a database with some shape to it: several tables, a few
# thousand rows, text with the awkward characters in it. This builds one.
#
# There are two sources. If LABS_DATA points at the real CSV export, the tables
# are loaded from it, which is what a developer wants locally — the data is
# familiar and the queries mean something. Otherwise the same tables are
# synthesised, deterministically, so CI can build a fixture without the export
# and every run gets byte-identical data.
#
# Nothing in the suites asserts on specific values from these tables; they read
# their own oracles out of whichever database they are given. What the fixture
# has to provide is the schema, enough rows to make streaming and concurrency
# mean something, and text that is not all ASCII.

set -euo pipefail

out=${1:-sample.duckdb}
labs=${LABS_DATA:-}

command -v duckdb >/dev/null || { echo "fixture: needs the duckdb CLI on PATH" >&2; exit 2; }

# Never build on top of an existing file: a fixture that accumulates state
# across runs is a fixture that passes on one machine and not another.
rm -f "$out" "$out.wal"

if [[ -n "$labs" && -d "$labs/synopsis" ]]; then
  echo "fixture: loading from $labs/synopsis"
  duckdb -no-init "$out" <<SQL
CREATE OR REPLACE TABLE sites   AS FROM read_csv('$labs/synopsis/sites.csv',   all_varchar=true);
CREATE OR REPLACE TABLE cohorts AS FROM read_csv('$labs/synopsis/cohorts.csv', all_varchar=true);
CREATE OR REPLACE TABLE plans   AS FROM read_csv('$labs/synopsis/plans.csv',   all_varchar=true);
CREATE OR REPLACE TABLE rules   AS FROM read_csv('$labs/synopsis/rules.csv',   all_varchar=true);
CHECKPOINT;
SQL
else
  echo "fixture: synthesising (set LABS_DATA to load the real export instead)"
  # Same table and column names as the real export, and all VARCHAR, because
  # that is how the CSVs are read. Row counts match too, so a suite that has a
  # number in it means the same thing either way.
  #
  # hash(i) gives deterministic pseudo-randomness without a seed to thread
  # through, and the text columns deliberately include non-ASCII, an embedded
  # quote and a newline: the encoder's edge cases should be present in ordinary
  # data, not only in the tests that go looking for them.
  duckdb -no-init "$out" <<'SQL'
CREATE OR REPLACE TABLE sites AS
SELECT
  'A' || lpad(i::VARCHAR, 6, '0')                              AS account,
  CASE WHEN i % 7 = 0 THEN 'yes' ELSE 'no' END                 AS skip,
  ['elation','athena','epic','cerner'][i % 4 + 1]              AS emr,
  'Customer ' || (i % 23)                                      AS customer,
  'C' || lpad((i % 23)::VARCHAR, 4, '0')                       AS client,
  CASE i % 5
    WHEN 0 THEN 'Café Clinic ' || i
    WHEN 1 THEN 'St. Mary''s ' || i
    WHEN 2 THEN 'Clinic — North ' || i
    ELSE 'Site ' || i END                                      AS name,
  ['active','pending','closed'][i % 3 + 1]                     AS status,
  ['onsite','nearsite','virtual'][i % 3 + 1]                   AS type,
  (hash(i) % 500)::VARCHAR                                     AS needed,
  (100 + i) || ' Main St' || CASE WHEN i % 11 = 0 THEN chr(10) || 'Suite ' || i ELSE '' END AS address,
  ['Austin','Boston','Chicago','Denver','El Paso'][i % 5 + 1]  AS city,
  ['TX','MA','IL','CO','CA'][i % 5 + 1]                        AS state,
  lpad(((hash(i) % 99999))::VARCHAR, 5, '0')                   AS zip,
  '555-' || lpad((i % 10000)::VARCHAR, 4, '0')                 AS phone,
  CASE WHEN i % 3 = 0 THEN NULL ELSE '555-' || lpad((i % 9999)::VARCHAR, 4, '0') END AS fax,
  'Contact ' || i                                              AS contact,
  'site' || i || '@example.invalid'                            AS email,
  CASE WHEN i % 4 = 0 THEN NULL ELSE 'PO-' || i END            AS po,
  ((hash(i) % 100000) / 100.0)::VARCHAR                        AS budget,
  lpad(((hash(i * 7) % 10000000000))::VARCHAR, 10, '0')        AS npi,
  CASE WHEN i % 13 = 0 THEN 'note: 世界 🦆 "quoted" ' || i ELSE NULL END AS note
FROM range(109) t(i);

CREATE OR REPLACE TABLE cohorts AS
SELECT
  'EF' || lpad(i::VARCHAR, 6, '0')                             AS ef_id,
  'C' || lpad((i % 23)::VARCHAR, 4, '0')                       AS client,
  CASE WHEN i % 6 = 0 THEN 'Groupe Santé ' || i ELSE 'Cohort ' || i END AS name,
  ['alpha','beta','gamma','delta'][i % 4 + 1]                  AS cohort,
  CASE WHEN i % 5 = 0 THEN NULL ELSE 'EL' || (hash(i) % 1000000) END AS elation
FROM range(268) t(i);

CREATE OR REPLACE TABLE plans AS
SELECT
  'EF' || lpad((i % 268)::VARCHAR, 6, '0')                     AS ef_id,
  'C' || lpad((i % 23)::VARCHAR, 4, '0')                       AS client,
  (hash(i) % 8)::VARCHAR                                       AS rules,
  ['PPO','HMO','EPO','POS','HDHP'][i % 5 + 1] || ' ' || (i % 40) AS plan,
  ['Aetna','Cigna','United','BCBS','Humana'][i % 5 + 1]        AS payer,
  'G' || lpad(((hash(i) % 900000) + 100000)::VARCHAR, 6, '0')  AS "group",
  CASE WHEN i % 9 = 0 THEN NULL ELSE 'R' || (i % 61) END       AS rule
FROM range(1075) t(i);

CREATE OR REPLACE TABLE rules AS
SELECT
  'C' || lpad((i % 23)::VARCHAR, 4, '0')                       AS client,
  'Rule ' || i                                                 AS name,
  ['active','draft','retired'][i % 3 + 1]                      AS status,
  (i % 2)::VARCHAR                                             AS onsite,
  ((i + 1) % 2)::VARCHAR                                       AS nearsite,
  (i % 3 = 0)::VARCHAR                                         AS virtual,
  CASE WHEN i % 4 = 0 THEN 'y' ELSE 'n' END                    AS nysite,
  CASE WHEN i % 8 = 0 THEN 'force' ELSE NULL END               AS force
FROM range(61) t(i);

CHECKPOINT;
SQL
fi

# A fixture with a WAL beside it is a fixture that has not been checkpointed,
# and copying only the .duckdb file — which every suite does — would silently
# lose rows.
# Fatal, not a warning. Every suite copies the .duckdb file alone, and every
# oracle is then read from that same truncated copy — so oracle and server agree
# on the wrong data and the whole run goes green over a fixture missing rows.
# There is no downstream check that can catch this; it has to stop here.
if [[ -f "$out.wal" ]]; then
  echo "fixture: $out.wal remains after CHECKPOINT — the fixture is not safe to copy" >&2
  exit 1
fi

duckdb -no-init -readonly "$out" -c "SELECT table_name, estimated_size AS rows FROM duckdb_tables() ORDER BY 1"
echo "fixture: wrote $out ($(du -h "$out" | cut -f1))"

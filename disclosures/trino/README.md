# Trino

Flawrence found these bugs in [Trino](https://github.com/trinodb/trino).
We reported all of them as private GitHub security advisories on 2026-05-13 and 2026-05-14.
The maintainers never responded.
The 90 day disclosure deadline has passed, so we publish them here.

All reports target Trino 481 and earlier.
We checked again on 2026-11-25. All 15 bugs are still present in master and in release 484.
The advisories are still private on GitHub. Only Trino maintainers can open them.

## Authentication

- **GHSA-7g72-mqh4-9673.** The default internal communication secret falls back to `node.environment`, which the public `/v1/status` endpoint returns. Anyone can forge an `X-Trino-Internal-Bearer` token and reach internal and management endpoints.
- **GHSA-pp9q-vj3m-c33f.** The OAuth2 token exchange flow does not bind the polling client to the browser that logged in. A victim who opens one link hands their bearer token to the attacker.
- **GHSA-33vj-rj4c-vp6p.** `AnnounceNodeResource` accepts any URI. A holder of the shared secret can register a rogue worker.
- **GHSA-j2gf-4cj2-jr63.** Task results under `/v1/task/{id}/results` have no per task authorization. A compromised worker can read the query results of other users.

## Access control

- **GHSA-5c4h-4j7j-v343.** File based access control puts the user name into a regular expression without escaping it. A crafted user name can impersonate an admin.
- **GHSA-5rr5-4c3r-6m4h.** The Iceberg and Delta Lake `table_changes()` function ignores row filters and column masks.
- **GHSA-9gqh-fcvv-fgfx.** The `system.query` and `raw_query` table functions of the MongoDB, Elasticsearch and OpenSearch connectors skip table level SELECT checks.
- **GHSA-fp45-6cj9-88mx.** `REFRESH MATERIALIZED VIEW` resolves the stored view body with the schema of the invoking session. Another user can poison the view.

## Hive connector

- **GHSA-95gm-rf66-4jhf.** Manifest targets of `SymlinkTextInputFormat` bypass location access control. This allows reads across tenants.
- **GHSA-6ph9-mfrc-36gp.** `partition_projection_location_template` bypasses the check for non-managed tables and reads from any location.
- **GHSA-3x9q-rxjm-5hm8.** With the Glue metastore, `dropPartition` deletes any storage prefix with the catalog credentials.

## SQL injection in connectors

- **GHSA-9qh7-g3fm-78r9.** ClickHouse. A backslash in the `order_by` or `primary_key` table property breaks out of the quoted identifier. The injected SQL runs with the connector credentials.
- **GHSA-vqmx-vq5f-fx5c.** SQL Server. A quote in a table name breaks out of the `sp_tableoption` call during INSERT.
- **GHSA-g6xv-fc5h-56vp.** BigQuery. The case insensitive table lookup puts the table name into SQL without escaping. Only affects catalogs with `bigquery.case-insensitive-name-matching=true`.

## Geospatial plugin

- **GHSA-86m5-h93v-x9wc.** `ST_GeomFromKML` passes user input to the JTS `KMLReader`, which resolves external entities. This allows SSRF and file reads from the worker. The root cause is in JTS, see [disclosures/jts](../jts/README.md).

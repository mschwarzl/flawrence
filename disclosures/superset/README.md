# Apache Superset

Flawrence found these bugs in [Apache Superset](https://github.com/apache/superset).
We sent 13 reports to the Superset security team on 2026-05-14.
The team never gave a proper reply.
The 90 day disclosure deadline has passed, so we publish them here.

All reports target the `latest-dev` image of 2026-04-27 and master at commit `d1e9a5df06`.
Unless a report names a setting, the default Docker Compose configuration is affected.
No report needs the Admin role.

We checked the status on 2026-11-25.
Eight of the 13 are fixed on master and in the 7.0.0rc3 pre-release.
No fix is in a final release yet, so the latest stable release 6.1.0 has all of them.

## Code execution

- **Jinja sandbox escape through the `metric` macro.** The macro is a `functools.partial` that holds the live `SandboxedEnvironment`. Its `template_class` creates a normal Jinja environment, so a template can run OS commands as the web worker. A user with the Gamma and sql_lab roles and access to one dataset can trigger it. Needs `ENABLE_TEMPLATE_PROCESSING`. **Fixed on master** in [`e5986b32f`](https://github.com/apache/superset/commit/e5986b32f).

## SQL injection

- **`url_param()` skips escaping.** A value that comes from the HTTP query string is returned before it is escaped. A Gamma user who can view a chart can inject SQL into the virtual dataset behind it. Needs `ENABLE_TEMPLATE_PROCESSING`. **Fixed on master** in [`12bef03f4`](https://github.com/apache/superset/commit/12bef03f4).
- **Schema field in Postgres and DB2 prequeries.** The schema name goes unescaped into a raw `cursor.execute()`. A sql_lab user can run DML and DDL on a database with `allow_dml=False`. **Fixed on master** in [`2e7a2b1f2`](https://github.com/apache/superset/commit/2e7a2b1f2).
- **CTAS table name.** The SQL Lab execute API puts `tmp_table_name` unquoted into `CREATE TABLE ... AS` and does not check `allow_ctas`. On drivers that allow several statements, text after a semicolon runs as extra SQL. **Fixed on master** in [`f7f50a797`](https://github.com/apache/superset/commit/f7f50a797) and [`584466e02`](https://github.com/apache/superset/commit/584466e02).

## SQL Lab restrictions

- **`allow_dml=False` bypass through `EXPLAIN (ANALYZE)`.** The mutation check only matches the prefix `ANALYZE `. Postgres runs the wrapped DML, and a trailing `COMMIT` keeps the change. **Fixed on master** in [`7aba2d339`](https://github.com/apache/superset/commit/7aba2d339).
- **Table authorization bypass through `VALUES`.** For a query whose top level is a `VALUES` statement, the parser reports no tables. The access check passes and the nested `SELECT` reads tables the user has no access to. **Not fixed.**

## Authorization

- **Database credentials in the asset export.** An Alpha user calls `GET /api/v1/assets/export/` and gets every database connection string with its password, the SSH tunnel settings and all saved queries of other users. **Unclear.** The export code is unchanged, but newer versions mask the password when a connection is saved.
- **Chart ownership bypass.** A `PUT` on a chart with `query_context_generation` set skips the ownership check. A Gamma user can overwrite the query context of charts they do not own. **Not fixed.** Later changes require chart access and keep the datasource bound, which limits the impact.
- **Themes.** A Gamma user can overwrite any theme that is not built in, including the active default theme, and can delete themes of other users. The theme content leads to stored XSS for every user. **Fixed on master** in [`7f1b41444`](https://github.com/apache/superset/commit/7f1b41444) and [`dca54da24`](https://github.com/apache/superset/commit/dca54da24). The 7.0.0rc3 pre-release only has the first commit.
- **Guest token tamper guard.** For an embedded dashboard, the check for a modified query context always passes for guests. The holder of a guest token can query any column and any ad hoc SQL metric of the datasets behind the dashboard. Needs `EMBEDDED_SUPERSET`. **Not fixed.** Later changes cover native filter and Drill By requests only.

## Authentication

- **MCP user mapping.** With MCP JWT authentication on, the default resolver picks the Superset user whose name matches the `preferred_username` claim. Many identity providers let users change that claim, so anyone with an account at the provider can act as any Superset user. **Not fixed.**
- **OAuth2 database callback.** `GET /api/v1/database/oauth2/` has no session check and stores the token under the user ID from the state parameter. An attacker can make a victim's database token land in the attacker's account. **Fixed on master** in [`13eb47a1d`](https://github.com/apache/superset/commit/13eb47a1d).

## Cross-site scripting

- **deck.gl JavaScript parameters.** Chart parameters such as `js_tooltip` and `js_data_mutator` are evaluated when the chart renders, even with `ENABLE_JAVASCRIPT_CONTROLS=False`. A Gamma user with chart write access can store script that runs for every viewer. The shipped development configuration allows `unsafe-eval`, which this needs. **Fixed on master** in [`23d18743b`](https://github.com/apache/superset/commit/23d18743b). Later, [`b362d3601`](https://github.com/apache/superset/commit/b362d3601) removed JavaScript controls from deck.gl.

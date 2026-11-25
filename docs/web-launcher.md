# Browser launcher

`flawrence-web` starts one `flawrence-runner` process at a time from a browser.
The form asks for a target directory, an artifacts directory, environment settings, a top-level agent and a query.
The default agent is `security-review`.
A top-level agent is one that no other agent lists as a subagent or output reviewer.

## Local use

```sh
npm --prefix frontend ci
npm --prefix frontend run build
cargo build --release --bin flawrence-runner --bin flawrence-web
./target/release/flawrence-web
```

Open <http://127.0.0.1:8080/>.
`--host`, `--port` and `--runner` change the address, the port and the runner binary.
By default the launcher uses the `flawrence-runner` binary next to it.

## The form

The launcher reads `.env` and the process environment and fills the form with known settings.
Process variables win over `.env`.

Sliders and fields cover memory, parallel agents, dashboard events, token limits, iterations and prices.
The text box below them takes any other `NAME=value` lines.
A value in the text box always wins over the matching control.
Use it for values outside the slider ranges.

The last position of the memory slider means no memory limit.
The last position of the context slider means no context limit.
In both cases the variable is removed for the run, even if `.env` sets it.
An empty model field lets each agent use its own model.

A read-only preview shows the final environment the runner gets.
Start stays disabled while the text box contains a line that cannot be parsed.

The launcher never writes your settings to disk or to browser storage.
Anyone who can open the launcher can see the configured credentials.
The launcher password is never shown and never passed to the runner.

## Providers

Tick **Configure providers** to edit connections.
Each OpenAI-compatible provider row has a name, an endpoint and a key.
A row named `local` sets `FLAWRENCE_PROVIDER_LOCAL_BASE_URL` and `FLAWRENCE_PROVIDER_LOCAL_API_KEY`.
The names `anthropic`, `openai` and `workers-ai` are reserved.

For a custom provider, the launcher asks `<endpoint>/models` for its model list.
The arrow next to the model field opens that list.
You can always type a model name by hand.

## Running

Start opens the run dashboard in a new tab.
If the browser blocks the tab, use the link on the launcher page.
Untick **Enable run dashboard** to run without it.

Runner output goes to `launcher.log` in the artifacts directory.
The panel at the bottom of the launcher shows the last 1000 lines.

The form stays locked while the runner is alive.
This holds across reloads and across browser tabs.
After a reload the form shows the settings of the last run.

The Ctrl+C button sends SIGINT to the runner and keeps its three stages.
With the dashboard enabled, the runner keeps serving it after the analysis ends.
Press Ctrl+C once more to stop it and unlock the form.

## Dashboard memory

The dashboard keeps the newest `FLAWRENCE_WEB_MAX_EVENTS` events per run, 1000 by default.
Older events are dropped from memory.
Artifacts stay available in the artifact browser.
Use `--transcripts` if you need the full event history on disk.

## Docker

The image starts `flawrence-web --host 0.0.0.0`.
It refuses to start without `FLAWRENCE_WEB_PASSWORD`.
Sign in with the user name `flawrence` and that password.

```sh
docker build -t flawrence .
docker run --rm --env-file .env -e FLAWRENCE_WEB_PASSWORD \
  -p 127.0.0.1:8080:8080 \
  -v /path/to/code:/workspace \
  -v /path/to/results:/artifacts flawrence
```

Paths in the form are paths inside the container.
`FLAWRENCE_WORKSPACE_DIR` and `FLAWRENCE_ARTIFACTS_BASE_DIR` set their defaults.
`PORT` sets the listening port.

With Compose, put the password in `.env` and run this.

```sh
docker compose up --build flawrence-web
```

`FLAWRENCE_WEB_PORT` sets the host port, 8080 by default.

## Hosting

The launcher only needs one public port.
It proxies each run dashboard under `/run/<id>/`, and the dashboards themselves listen on loopback.
Put HTTPS in front of it and use `/health` as the health check.
The health check needs no password and shows no run state.

Mount persistent storage on `/workspace` and `/artifacts`.
The launcher does not clone repositories, so put the target in place first.
Run a single replica, because only one launcher process knows that a run is active.

Only give launcher access to people you trust.
They can run any analysis with the container's files and credentials.

## Tests

```sh
node --test bin/flawrence-runner/src/launcher.test.cjs
cargo test -p flawrence-runner
```

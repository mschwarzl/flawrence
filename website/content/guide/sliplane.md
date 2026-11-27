+++
title = "Deploy to Sliplane"
weight = 10
+++
## Service setup

Deploying the Flawrence GitHub repository as a service using the root `Dockerfile` is straight-forward: 

1. Fork the repository
2. Log into Sliplane, and grant read access to the fork.
3. Configure persistent volumes for the workspace and the results; mount them at `/workspace`, and `/artifacts`, respectively
4. Use a single replica ans `/health` for health checks
5. Since you are deploying a fork of the Flawrence repository, you can set auto-deploy on changes, and simply mirror over Flawrence releases and have then auto-deploy
6. Setup env variables as below (the example includes a pre-selected model for faster start times)


FLAWRENCE_AGENT_MEMORY_MB="1536" # highly recommended, to prevent flawrence being shot down by the OOM killer!
FLAWRENCE_MAX_PARALLEL_AGENTS="4" # this times four = maximum memory consumption (see above)
FLAWRENCE_MODEL="tensorx/deepseek/deepseek-v4.1-flash" # pre-set a model with a custom openai-compatible provider
FLAWRENCE_PROVIDER_TENSORX_API_KEY="***********"
FLAWRENCE_PROVIDER_TENSORX_BASE_URL="https://api.tensorx.ai/v1"
FLAWRENCE_WEB_MAX_EVENTS="25" # bound live dashboard total events shown
FLAWRENCE_WEB_PASSWORD="**********"
FLAWRENCE_WORKTREE_ISOLATION="true"
PORT="8080"
TOKEN_COST_CACHE_READ="0.13" # adjust to model
TOKEN_COST_INPUT="0.5" # adjust to model
TOKEN_COST_OUTPUT="1.5" # adjust to model


## First run

Prepare the target in the persistent workspace through your trusted administrative workflow.
Sliplane allow SSH login into the docker container; SSH into it, clone the repo to analyse into the workspace, set the workspace and artifact directories and you're good to go!
Sign in as `flawrence`, select the target and artifacts paths inside the container, and configure your model and limits. The launcher proxies the run dashboard through the same service port; no second public port is necessary.

> [!IMPORTANT]
> Keep one launcher instance. It tracks the active run in process memory. Restrict access to trusted users, and do not share the service with unrelated workloads or sensitive files.
> The live dashboard will take until after the pre-warm is done to show. This can take several minutes, or (for larger codebases even hours, or however long it takes for a first full compilation run).

See [Sliplane service deployment](https://docs.sliplane.io/services/deploying-a-service/) and the [hosting notes](@/guide/web-launcher.md#hosting).

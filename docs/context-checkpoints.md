# Context checkpoints

Long agents eventually fill the model's context window.
A checkpoint writes the agent's progress to an artifact and starts a fresh conversation from it.

Set the context window of your model.

```sh
MAX_CONTEXT_TOKENS=524288 flawrence-runner --agent security-review \
  --artifacts ./findings "Review this codebase"
```

`MAX_CONTEXT_TOKENS` must be larger than `MAX_TOKENS`.
It applies to every model in the run, so use the smallest window when you mix models.
Without it, agents only checkpoint at their normal points.

## When an agent checkpoints

`security-review` and `vulnerability-research` write a checkpoint after each batch of `spawn_task` or `report_finding` calls.
They also write one when the context gets full.
The runner estimates the size from the last reported token count, the new messages, `MAX_TOKENS` and 10% headroom.
The estimate is conservative, so a checkpoint can come early.

The same model writes the checkpoint as a JSON record of at most 12,000 characters.
The record holds the finished work, the facts and their evidence, rejected ideas, open work and the next step.
Checkpoints count toward the cost and token limits but not toward `MAX_ITERATIONS`.

## What is kept

Each checkpoint creates two artifacts.

| Artifact | Contents |
| --- | --- |
| `checkpoint_<task>_<n>` | The progress record, the todo list and the status of child agents. |
| `checkpoint_<task>_<n>_history` | The full conversation that was replaced. |

The new conversation starts with the original instructions, the query and the progress record.
The agent can read the history artifact when it needs a detail.
Todos, workspaces, budgets and limits stay as they were.

If the checkpoint is malformed or cannot be saved, the agent keeps its old conversation.

## Using checkpoints in your own agent

Grant read and write access to `checkpoint_*` in the agent's artifact permissions.
Without that grant, an agent that runs out of context stops with an error.

A checkpoint keeps what the agent wrote down, not everything it knew.
Agents should store exact evidence in artifacts and mark open hypotheses as such.

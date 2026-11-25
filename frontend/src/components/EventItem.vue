<template>
  <div class="event-item" :class="event.event.type">
    <div class="event-ts">{{ formatTime(event.ts) }}</div>

    <div class="event-wrap">
      <div v-if="showAgentHeader && agentName" class="agent-header">
        <i class="fa-solid fa-robot agent-icon"></i>
        {{ agentName }}
      </div>

      <div v-if="event.event.type === 'TaskStarted'" class="event-body start">
        <strong>Task started</strong> — {{ event.event.agent }}<br/>
        <span class="muted">{{ event.event.query }}</span>
      </div>

      <div v-else-if="event.event.type === 'IterationStarted'" class="event-body iteration">
        <strong>Iteration {{ event.event.iteration }}</strong>
      </div>

      <div v-else-if="event.event.type === 'AssistantMessage'" class="event-body assistant">
        <div v-if="showReasoning && event.event.reasoning" class="reasoning">
          <em>Reasoning:</em> {{ event.event.reasoning }}
        </div>
        <div v-if="event.event.content" class="content">{{ event.event.content }}</div>
        <div v-if="event.event.tool_calls?.length" class="tool-calls">
          <ToolCall v-for="tc in event.event.tool_calls" :key="tc.call_id" :call="tc" :show-args="showToolArgs" />
        </div>
      </div>

      <div v-else-if="event.event.type === 'ToolCallStarted'" class="event-body tool-start">
        <span class="muted">→</span> {{ event.event.tool }} <code v-if="showToolArgs" class="inline-args">{{ JSON.stringify(event.event.args) }}</code>
      </div>

      <div v-else-if="event.event.type === 'ToolCallCompleted'" class="event-body tool-end">
        <span :class="event.event.ok ? 'ok' : 'fail'">
          <span class="muted">←</span> {{ event.event.tool }} {{ event.event.ok ? 'OK' : 'FAIL' }} {{ event.event.duration_ms }}ms
        </span>
        <pre v-if="event.event.output"><code>{{ JSON.stringify(event.event.output, null, 2) }}</code></pre>
      </div>

      <div v-else-if="event.event.type === 'SubagentSpawned'" class="event-body subagent">
        <strong>Spawned subagent:</strong> {{ event.event.agent }} — {{ event.event.title }}
      </div>

      <div v-else-if="event.event.type === 'SubagentCompleted'" class="event-body subagent">
        <strong>Subagent completed</strong>
      </div>

      <div v-else-if="event.event.type === 'TokenUsage'" class="event-body usage">
        Tokens: {{ event.event.model }} — prompt {{ event.event.prompt_tokens }}, completion {{ event.event.completion_tokens }} — cost ${{ event.event.cumulative_cost_usd?.toFixed(4) }}
      </div>

      <div v-else-if="event.event.type === 'TaskCompleted'" class="event-body terminal completed">
        <strong>Task completed</strong> in {{ event.event.elapsed_secs?.toFixed(1) }}s
      </div>

      <div v-else-if="event.event.type === 'TaskFailed'" class="event-body terminal failed">
        <strong>Task failed:</strong> {{ event.event.error }}
      </div>

      <div v-else-if="event.event.type === 'Warning'" class="event-body warning">
        <span class="warn-icon">⚠</span> {{ event.event.message }}
      </div>

      <div v-else class="event-body">
        <pre><code>{{ JSON.stringify(event.event, null, 2) }}</code></pre>
      </div>
    </div>
  </div>
</template>

<script setup>
import ToolCall from './ToolCall.vue'

const props = defineProps({
  event: Object,
  showReasoning: Boolean,
  showToolArgs: Boolean,
  agentName: String,
  showAgentHeader: Boolean,
})

function formatTime(ts) {
  const d = new Date(ts)
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })
}
</script>

<style scoped>
.event-item {
  padding: 8px 0;
  border-bottom: 1px solid var(--border);
  display: flex;
  gap: 12px;
  min-width: 0;
}
.event-ts {
  font-size: 0.7rem;
  color: var(--text-muted);
  white-space: nowrap;
  width: 60px;
  flex-shrink: 0;
  padding-top: 2px;
}
.event-wrap {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}
.agent-header {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 0.65rem;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  color: var(--text-muted);
  background: var(--bg-surface-alt);
  border: 1px solid var(--border);
  border-radius: 999px;
  padding: 1px 8px;
  align-self: flex-start;
}
.agent-icon {
  font-size: 0.6rem;
}
.event-body {
  flex: 1;
  font-size: 0.9rem;
  min-width: 0;
  overflow-wrap: break-word;
  word-break: break-word;
  max-height: 24rem;
  overflow-y: auto;
}
.event-body.start { color: var(--accent); }
.event-body.iteration { font-weight: 700; color: var(--text-primary); }
.event-body.assistant .reasoning { color: var(--text-muted); font-style: italic; margin-bottom: 6px; }
.event-body.assistant .content { white-space: pre-wrap; }
.event-body.tool-start { color: var(--text-secondary); font-size: 0.85rem; }
.event-body.tool-end .ok { color: var(--success); }
.event-body.tool-end .fail { color: var(--danger); }
.event-body.tool-end pre { margin-top: 6px; overflow-x: auto; white-space: pre-wrap; }
.event-body pre { overflow-x: auto; white-space: pre-wrap; max-width: 100%; }
.event-body code { overflow-wrap: break-word; word-break: break-word; }
.inline-args {
  display: inline;
  overflow-wrap: break-word;
  word-break: break-word;
  white-space: pre-wrap;
}
.event-body.subagent { color: var(--info); font-size: 0.85rem; }
.event-body.usage { color: var(--text-muted); font-size: 0.8rem; }
.event-body.terminal { font-weight: 700; padding: 8px; border-radius: var(--radius); }
.event-body.terminal.completed { background: #d1fae5; color: #065f46; }
.event-body.terminal.failed { background: #fee2e2; color: #991b1b; }
.event-body.warning {
  background: #fef3c7;
  color: #92400e;
  border-radius: var(--radius);
  padding: 6px 10px;
  font-size: 0.85rem;
}
.event-body.warning .warn-icon { margin-right: 6px; }
.muted { color: var(--text-muted); }
</style>

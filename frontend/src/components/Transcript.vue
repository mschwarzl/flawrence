<template>
  <div class="transcript" ref="container">
    <div class="transcript-controls">
      <label><input type="checkbox" v-model="showReasoning" /> Show reasoning</label>
      <label><input type="checkbox" v-model="showToolArgs" /> Show tool args</label>
    </div>
    <div class="events" ref="eventsEl">
      <EventItem
        v-for="(ev, i) in displayEvents"
        :key="ev.seq"
        :event="ev"
        :show-reasoning="showReasoning"
        :show-tool-args="showToolArgs"
        :agent-name="agentMap[ev.task_id]"
        :show-agent-header="true"
      />
    </div>
  </div>
</template>

<script setup>
import { ref, watch, nextTick, computed } from 'vue'
import EventItem from './EventItem.vue'

const props = defineProps({ events: Array })

const container = ref(null)
const eventsEl = ref(null)
const showReasoning = ref(false)
const showToolArgs = ref(true)

const displayEvents = computed(() => {
  const arr = (props.events || []).slice()
  arr.reverse()
  return arr
})

const agentMap = computed(() => {
  const map = {}
  for (const ev of props.events || []) {
    if (ev.event?.type === 'TaskStarted' && ev.event?.agent) {
      map[ev.task_id] = ev.event.agent
    }
  }
  return map
})

watch(() => props.events.at(-1)?.seq, async () => {
  await nextTick()
  const el = eventsEl.value
  if (!el) return
  // Only snap to top if user is already near the top (within 60px)
  if (el.scrollTop <= 60) {
    el.scrollTop = 0
  }
})
</script>

<style scoped>
.transcript {
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: hidden;
}
.transcript-controls {
  display: flex;
  gap: 16px;
  padding: 10px 0;
  border-bottom: 1px solid var(--border);
  margin-bottom: 8px;
}
.transcript-controls label {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 0.85rem;
  color: var(--text-secondary);
  cursor: pointer;
}
.events {
  flex: 1;
  overflow-y: auto;
  padding-right: 4px;
  min-width: 0;
}
</style>

<template>
  <div class="job-detail">
    <div v-if="loading" class="loading">Loading job...</div>
    <div v-else-if="!job" class="error">Job not found.</div>
    <template v-else>
      <div class="job-header card">
        <div class="title-row">
          <h2>Run #{{ job.run_id + 1 }}</h2>
          <StatusBadge :status="job.status.status" />
        </div>
        <div class="meta">
          <div><strong>Agent:</strong> {{ job.agent_name }}</div>
          <div><strong>Query:</strong> {{ job.query }}</div>
          <div v-if="job.target"><strong>Target:</strong> {{ job.target }}</div>
          <div><strong>Started:</strong> {{ formatTime(job.started_at) }}</div>
          <div v-if="job.completed_at"><strong>Completed:</strong> {{ formatTime(job.completed_at) }}</div>
        </div>
        <TokenUsage v-if="job.status.token_usage" :usage="job.status.token_usage" />
        <div v-if="ctx.has" class="context-box">
          <strong>Context:</strong>
          <span class="ctx-now">{{ fmtK(ctx.current) }}</span>
          <span class="ctx-sep">·</span>
          <span class="ctx-peak">window peak {{ fmtK(ctx.peak) }}</span>
        </div>
        <div v-if="job.status.output" class="final-output">
          <h4>Final Output</h4>
          <div class="markdown-body" v-html="marked(job.status.output || '')"></div>
        </div>
        <div v-if="job.status.error" class="final-error">
          <h4>Error</h4>
          <pre>{{ job.status.error }}</pre>
        </div>
      </div>

      <div class="task-tree card">
        <h3>Tasks</h3>
        <JobGraph :events="events" />
      </div>

      <div class="detail-columns">
        <div class="column transcript-column card">
          <h3>Transcript</h3>
          <p class="retention-note">Showing the latest {{ events.length }} events (limit {{ eventLimit }}).
            Older events are discarded. Artifacts remain available.</p>
          <Transcript :events="events" />
        </div>
        <div class="column artifacts-column card">
          <h3>Artifacts</h3>
          <ArtifactList :artifacts="artifacts" @select="selectedArtifact = $event" />
        </div>
      </div>

      <div v-if="selectedArtifact.key" class="modal-overlay" @click.self="selectedArtifact = { key: null, contentType: null }">
        <div class="modal-content">
          <ArtifactViewer :job-id="id" :key-name="selectedArtifact.key" :content-type="selectedArtifact.contentType" @close="selectedArtifact = { key: null, contentType: null }" />
        </div>
      </div>
    </template>
  </div>
</template>

<script setup>
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { marked } from 'marked'
import { fetchJob, fetchJobEvents, fetchArtifacts, streamJobEvents } from '@/api'
import StatusBadge from '@/components/StatusBadge.vue'
import TokenUsage from '@/components/TokenUsage.vue'
import Transcript from '@/components/Transcript.vue'
import ArtifactList from '@/components/ArtifactList.vue'
import ArtifactViewer from '@/components/ArtifactViewer.vue'
import JobGraph from '@/components/JobGraph.vue'

const props = defineProps({ id: String })

const job = ref(null)
const events = ref([])
const eventLimit = ref(1000)
const artifacts = ref([])
const loading = ref(true)
const selectedArtifact = ref({ key: null, contentType: null })

// Live context metric, derived from the per-call TokenUsage events.
// We attribute to the root (orchestrator) task: its task_id equals the job
// id, and it is the context that approaches the model's window ceiling.
// Subagent TokenUsage events (different task_id) are excluded.
const ctx = computed(() => {
  const arr = events.value.filter(
    (ev) => ev.event?.type === 'TokenUsage' && ev.task_id === props.id
  )
  let peak = 0
  for (const ev of arr) peak = Math.max(peak, ev.event.prompt_tokens || 0)
  const current = arr.length ? arr[arr.length - 1].event.prompt_tokens : 0
  return { current, peak, has: arr.length > 0 }
})
function fmtK(n) {
  if (!n) return '—'
  return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : `${n}`
}
let es = null
let lastSeq = null
let artifactPollId = null
let jobPollId = null

function addEvent(ev) {
  if (ev && typeof ev.seq === 'number' && (lastSeq === null || ev.seq > lastSeq)) {
    lastSeq = ev.seq
    events.value.push(ev)
    if (events.value.length > eventLimit.value) {
      events.value.splice(0, events.value.length - eventLimit.value)
    }
  }
}

function formatTime(ts) {
  const d = new Date(ts)
  return d.toLocaleString()
}

async function loadJob() {
  try {
    job.value = await fetchJob(props.id)
    if (job.value.status.status !== 'running' && es) es.close()
  } catch (e) {
    console.error(e)
  }
}

async function loadEvents() {
  try {
    const data = await fetchJobEvents(props.id)
    if (Number.isSafeInteger(data.limit) && data.limit > 0) eventLimit.value = data.limit
    events.value = data.events.slice(-eventLimit.value)
    lastSeq = events.value.at(-1)?.seq ?? null
  } catch (e) {
    console.error(e)
  }
}

async function loadArtifacts() {
  try {
    const data = await fetchArtifacts(props.id)
    artifacts.value = data.artifacts || []
  } catch (e) {
    console.error(e)
  }
}

function connectStream() {
  if (es) es.close()
  if (job.value?.status?.status !== 'running') return
  // EventSource reconnects itself using the server's Last-Event-ID cursor.
  es = streamJobEvents(props.id, lastSeq)
  es.addEventListener('job-event', (e) => {
    try {
      const ev = JSON.parse(e.data)
      addEvent(ev)
      if (ev.event.type === 'ToolCallCompleted' || ev.event.type === 'TaskCompleted' || ev.event.type === 'TaskFailed') {
        loadArtifacts()
      }
      if (ev.task_id === props.id && (ev.event.type === 'TaskCompleted' || ev.event.type === 'TaskFailed')) {
        es.close()
        loadJob()
      }
    } catch (err) {
      console.error('Failed to parse SSE event', err)
    }
  })
}

function startArtifactPolling() {
  if (artifactPollId) return
  artifactPollId = setInterval(() => {
    const status = job.value?.status?.status
    if (!status || status === 'running') {
      loadArtifacts()
    } else {
      stopArtifactPolling()
    }
  }, 3000)
}

function stopArtifactPolling() {
  if (artifactPollId) {
    clearInterval(artifactPollId)
    artifactPollId = null
  }
}

function startJobPolling() {
  if (jobPollId) return
  jobPollId = setInterval(() => {
    const status = job.value?.status?.status
    if (!status || status === 'running') {
      loadJob()
    } else {
      stopJobPolling()
    }
  }, 10000)
}

function stopJobPolling() {
  if (jobPollId) {
    clearInterval(jobPollId)
    jobPollId = null
  }
}

onMounted(async () => {
  await loadJob()
  await loadEvents()
  await loadArtifacts()
  loading.value = false
  connectStream()
  startArtifactPolling()
  startJobPolling()
})

onUnmounted(() => {
  if (es) es.close()
  stopArtifactPolling()
  stopJobPolling()
})
</script>

<style scoped>
.job-detail {
  display: flex;
  flex-direction: column;
  gap: 20px;
}
.job-header {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.title-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.title-row h2 {
  margin: 0;
}
.meta {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
  gap: 6px 20px;
  font-size: 0.9rem;
  color: var(--text-secondary);
}
.final-output .markdown-body {
  padding: 12px;
  border-radius: var(--radius);
  background: var(--bg-surface);
  max-height: 24rem;
  overflow-y: auto;
}
.final-error pre {
  background: #f6f8fa;
  padding: 12px;
  border-radius: var(--radius);
  overflow-x: auto;
  white-space: pre-wrap;
  color: var(--danger);
  max-height: 24rem;
  overflow-y: auto;
}
.context-box {
  font-size: 0.9rem;
  color: var(--text-secondary);
  padding: 8px 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--bg-surface);
}
.context-box .ctx-now {
  font-weight: 600;
  color: var(--text-primary);
  margin-left: 6px;
}
.context-box .ctx-sep {
  margin: 0 6px;
  color: var(--text-muted);
}
.context-box .ctx-peak {
  color: var(--text-muted);
}
.detail-columns {
  display: grid;
  grid-template-columns: 2fr 1fr;
  gap: 20px;
  min-width: 0;
}
.column {
  min-width: 0;
}
@media (max-width: 900px) {
  .detail-columns {
    grid-template-columns: 1fr;
  }
}
.column h3 {
  margin-top: 0;
  font-size: 1.1rem;
  border-bottom: 1px solid var(--border);
  padding-bottom: 8px;
}
.transcript-column {
  height: clamp(420px, 75vh, 900px);
  display: flex;
  flex-direction: column;
  min-width: 0;
  overflow: hidden;
}
.artifacts-column {
  max-height: clamp(420px, 75vh, 900px);
  overflow-y: auto;
}
.retention-note {
  margin-top: 0;
  font-size: .85rem;
  color: var(--text-secondary);
}
.task-tree {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 20px;
  box-shadow: var(--shadow);
}
.task-tree h3 {
  margin-top: 0;
  font-size: 1.1rem;
  border-bottom: 1px solid var(--border);
  padding-bottom: 8px;
}
.task-tree :deep(.job-graph) {
  max-height: 18rem;
  overflow-y: auto;
}
.transcript-column :deep(.transcript) {
  flex: 1;
  min-height: 0;
}
.modal-overlay {
  position: fixed;
  inset: 0;
  background: rgba(0,0,0,0.35);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 200;
  padding: 20px;
}
.modal-content {
  width: 100%;
  max-width: 900px;
  max-height: 90vh;
  display: flex;
  flex-direction: column;
}
.loading, .error {
  padding: 40px;
  text-align: center;
  color: var(--text-muted);
}
.error {
  color: var(--danger);
}
</style>

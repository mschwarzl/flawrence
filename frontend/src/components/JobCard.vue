<template>
  <div class="job-card" @click="goToJob">
    <div class="job-card-header">
      <StatusBadge :status="job.status.status" />
      <span class="job-id">#{{ job.run_id + 1 }}</span>
    </div>
    <div class="job-agent">{{ job.agent_name }}</div>
    <div class="job-query" :title="job.query">{{ job.query }}</div>
    <div class="job-meta">
      <span v-if="elapsed">{{ elapsed }}</span>
      <span v-if="tokens">{{ tokens }} tokens</span>
    </div>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { useRouter } from 'vue-router'
import StatusBadge from './StatusBadge.vue'

const props = defineProps({ job: Object })
const router = useRouter()

function goToJob() {
  router.push(`/job/${props.job.task_id}`)
}

const elapsed = computed(() => {
  if (props.job.status.status === 'completed') {
    return `${props.job.status.elapsed_secs?.toFixed(1) || 0}s`
  }
  if (props.job.status.status === 'running') {
    const created = new Date(props.job.started_at)
    const now = new Date()
    const secs = Math.floor((now - created) / 1000)
    return `${secs}s`
  }
  return null
})

const tokens = computed(() => {
  if (props.job.status.status !== 'completed') return null
  const u = props.job.status.token_usage || {}
  return Object.values(u).reduce((sum, m) => sum + (m.input_tokens || 0) + (m.output_tokens || 0) + (m.cache_read_tokens || 0) + (m.cache_write_tokens || 0), 0)
})
</script>

<style scoped>
.job-card {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 16px;
  cursor: pointer;
  transition: box-shadow 0.2s, transform 0.1s;
}
.job-card:hover {
  box-shadow: 0 4px 12px rgba(0,0,0,0.1);
  transform: translateY(-1px);
}
.job-card-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 10px;
}
.job-id {
  font-size: 0.8rem;
  color: var(--text-muted);
}
.job-agent {
  font-weight: 600;
  font-size: 0.95rem;
  margin-bottom: 6px;
  color: var(--text-primary);
}
.job-query {
  color: var(--text-secondary);
  font-size: 0.85rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  margin-bottom: 10px;
}
.job-meta {
  display: flex;
  gap: 12px;
  font-size: 0.8rem;
  color: var(--text-muted);
}
</style>

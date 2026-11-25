<template>
  <div class="dashboard">
    <div class="stats-row">
      <div class="stat-card">
        <div class="stat-value">{{ jobs.length }}</div>
        <div class="stat-label">Total Runs</div>
      </div>
      <div class="stat-card">
        <div class="stat-value" :style="{ color: 'var(--success)' }">{{ completedCount }}</div>
        <div class="stat-label">Completed</div>
      </div>
      <div class="stat-card">
        <div class="stat-value" :style="{ color: 'var(--danger)' }">{{ failedCount }}</div>
        <div class="stat-label">Failed</div>
      </div>
      <div class="stat-card">
        <div class="stat-value" :style="{ color: 'var(--warning)' }">{{ runningCount }}</div>
        <div class="stat-label">Running</div>
      </div>
      <div class="stat-card">
        <div class="stat-value">{{ totalTokens }}</div>
        <div class="stat-label">Total Tokens</div>
      </div>
    </div>

    <div class="filters">
      <button
        v-for="f in filters"
        :key="f.key"
        :class="['filter-btn', { active: activeFilter === f.key }]"
        @click="activeFilter = f.key"
      >
        {{ f.label }}
      </button>
    </div>

    <div v-if="loading" class="loading">Loading jobs...</div>
    <div v-else-if="error" class="error">{{ error }}</div>
    <JobList v-else :jobs="filteredJobs" />
  </div>
</template>

<script setup>
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { useJobsStore } from '@/stores/jobs'
import JobList from '@/components/JobList.vue'

const store = useJobsStore()
const activeFilter = ref('all')
const intervalId = ref(null)

const filters = [
  { key: 'all', label: 'All' },
  { key: 'running', label: 'Running' },
  { key: 'completed', label: 'Completed' },
  { key: 'failed', label: 'Failed' },
]

const jobs = computed(() => store.jobs)
const loading = computed(() => store.loading)
const error = computed(() => store.error)

const filteredJobs = computed(() => {
  if (activeFilter.value === 'all') return jobs.value
  return jobs.value.filter(j => j.status.status === activeFilter.value)
})

const completedCount = computed(() => store.completedJobs.length)
const failedCount = computed(() => store.failedJobs.length)
const runningCount = computed(() => store.runningJobs.length)

const totalTokens = computed(() => {
  let tokens = 0
  for (const j of jobs.value) {
    if (j.status.status === 'completed' && j.status.token_usage) {
      for (const u of Object.values(j.status.token_usage)) {
        tokens += (u.input_tokens || 0) + (u.output_tokens || 0) + (u.cache_read_tokens || 0) + (u.cache_write_tokens || 0)
      }
    }
  }
  return tokens
})

onMounted(() => {
  store.loadJobs()
  intervalId.value = setInterval(() => store.loadJobs(), 3000)
})

onUnmounted(() => {
  if (intervalId.value) clearInterval(intervalId.value)
})
</script>

<style scoped>
.dashboard {
  display: flex;
  flex-direction: column;
  gap: 20px;
}
.stats-row {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
  gap: 16px;
}
.stat-card {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 16px;
  text-align: center;
  box-shadow: var(--shadow);
}
.stat-value {
  font-size: 1.5rem;
  font-weight: 700;
  color: var(--text-primary);
}
.stat-label {
  font-size: 0.8rem;
  color: var(--text-muted);
  margin-top: 4px;
}
.filters {
  display: flex;
  gap: 8px;
}
.filter-btn {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 6px 14px;
  cursor: pointer;
  font-size: 0.85rem;
  color: var(--text-secondary);
}
.filter-btn.active {
  background: var(--accent);
  color: #fff;
  border-color: var(--accent);
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

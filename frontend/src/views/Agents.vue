<template>
  <div class="agents">
    <div v-if="loading" class="loading">Loading agents...</div>
    <div v-else-if="agents.length === 0" class="empty">No agents found.</div>
    <div v-else class="agent-grid">
      <div v-for="agent in agents" :key="agent.name" class="agent-card card">
        <div class="agent-header">
          <h3>{{ agent.name }}</h3>
          <span class="agent-model">{{ agent.model }}</span>
        </div>
        <p class="agent-desc">{{ agent.description }}</p>
        <div class="agent-section">
          <strong>Tools</strong>
          <div class="tag-list">
            <span v-for="t in agent.tools" :key="t" class="tag">{{ t }}</span>
          </div>
        </div>
        <div class="agent-section">
          <strong>Subagents</strong>
          <div class="tag-list">
            <span v-for="s in agent.subagents" :key="s" class="tag">{{ s }}</span>
            <span v-if="agent.subagents.length === 0" class="muted">None</span>
          </div>
        </div>
        <div v-if="agent.recent_runs.length" class="agent-section">
          <strong>Recent Runs</strong>
          <ul class="recent-list">
            <li v-for="run in agent.recent_runs" :key="run.task_id">
              <router-link :to="`/job/${run.task_id}`">
                #{{ run.run_id + 1 }} — <StatusBadge :status="run.status.status" />
              </router-link>
            </li>
          </ul>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup>
import { ref, onMounted } from 'vue'
import { fetchAgents } from '@/api'
import StatusBadge from '@/components/StatusBadge.vue'

const agents = ref([])
const loading = ref(true)

onMounted(async () => {
  try {
    const data = await fetchAgents()
    agents.value = data.agents
  } catch (e) {
    console.error(e)
  } finally {
    loading.value = false
  }
})
</script>

<style scoped>
.agents {
  display: flex;
  flex-direction: column;
  gap: 20px;
}
.agent-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
  gap: 16px;
}
.agent-card {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.agent-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.agent-header h3 {
  margin: 0;
  font-size: 1.1rem;
}
.agent-model {
  font-size: 0.8rem;
  color: var(--text-muted);
  background: var(--bg-surface-alt);
  padding: 2px 8px;
  border-radius: 999px;
}
.agent-desc {
  margin: 0;
  font-size: 0.9rem;
  color: var(--text-secondary);
}
.agent-section {
  font-size: 0.85rem;
}
.agent-section strong {
  display: block;
  margin-bottom: 4px;
  font-size: 0.8rem;
  text-transform: uppercase;
  letter-spacing: 0.03em;
  color: var(--text-muted);
}
.tag-list {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.tag {
  background: #eef2ff;
  color: #3730a3;
  padding: 2px 8px;
  border-radius: 4px;
  font-size: 0.75rem;
  font-weight: 500;
}
.recent-list {
  list-style: none;
  margin: 0;
  padding: 0;
}
.recent-list li {
  margin-bottom: 4px;
}
.recent-list a {
  color: var(--accent);
  text-decoration: none;
  font-size: 0.85rem;
}
.recent-list a:hover {
  text-decoration: underline;
}
.muted {
  color: var(--text-muted);
}
.loading, .empty {
  padding: 40px;
  text-align: center;
  color: var(--text-muted);
}
</style>

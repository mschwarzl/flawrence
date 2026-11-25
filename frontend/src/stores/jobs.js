import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import { fetchJobs, fetchJob, fetchJobEvents, streamJobEvents, fetchAgents } from '@/api'

export const useJobsStore = defineStore('jobs', () => {
  const jobs = ref([])
  const agents = ref([])
  const loading = ref(false)
  const error = ref(null)

  const runningJobs = computed(() => jobs.value.filter(j => j.status.status === 'running'))
  const completedJobs = computed(() => jobs.value.filter(j => j.status.status === 'completed'))
  const failedJobs = computed(() => jobs.value.filter(j => j.status.status === 'failed'))

  async function loadJobs() {
    loading.value = true
    error.value = null
    try {
      jobs.value = await fetchJobs()
    } catch (e) {
      error.value = e.message || 'Failed to load jobs'
    } finally {
      loading.value = false
    }
  }

  async function loadAgents() {
    try {
      const data = await fetchAgents()
      agents.value = data.agents
    } catch (e) {
      console.error('Failed to load agents', e)
    }
  }

  function updateJob(job) {
    const idx = jobs.value.findIndex(j => j.task_id === job.task_id)
    if (idx >= 0) {
      jobs.value[idx] = job
    } else {
      jobs.value.push(job)
    }
  }

  return {
    jobs, agents, loading, error,
    runningJobs, completedJobs, failedJobs,
    loadJobs, loadAgents, updateJob,
  }
})

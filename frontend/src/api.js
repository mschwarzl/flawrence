import axios from 'axios'

export const dashboardBase = document.querySelector('base')?.getAttribute('href') || '/'

const api = axios.create({
  baseURL: dashboardBase,
  headers: { 'Content-Type': 'application/json' },
})

export async function fetchJobs() {
  const { data } = await api.get('/api/jobs')
  return data
}

export async function fetchJob(id) {
  const { data } = await api.get(`/api/jobs/${id}`)
  return data
}

export async function fetchJobEvents(id) {
  const { data, headers } = await api.get(`/api/jobs/${id}/events`)
  return { events: data, limit: Number(headers['x-flawrence-max-events']) }
}

export function streamJobEvents(id, since) {
  const query = since === null ? '' : `?since=${since}`
  return new EventSource(`${dashboardBase}api/jobs/${id}/events/stream${query}`)
}

export async function fetchArtifacts(id) {
  const { data } = await api.get(`/api/jobs/${id}/artifacts`)
  return data
}

export async function fetchArtifact(id, key) {
  const { data } = await api.get(`/api/jobs/${id}/artifacts/${key}`)
  return data
}

export async function fetchAgents() {
  const { data } = await api.get('/api/agents')
  return data
}

export async function healthCheck() {
  const { data } = await api.get('/health')
  return data
}

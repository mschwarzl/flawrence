import { createRouter, createWebHistory } from 'vue-router'
import Dashboard from './views/Dashboard.vue'
import JobDetail from './views/JobDetail.vue'
import Agents from './views/Agents.vue'
import { dashboardBase } from './api'

const routes = [
  { path: '/', name: 'Dashboard', component: Dashboard },
  { path: '/job/:id', name: 'JobDetail', component: JobDetail, props: true },
  { path: '/agents', name: 'Agents', component: Agents },
]

const router = createRouter({
  history: createWebHistory(dashboardBase),
  routes,
})

export default router

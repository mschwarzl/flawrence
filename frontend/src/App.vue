<template>
  <div class="app">
    <aside class="sidebar">
      <div class="brand">
        <span class="brand-short">H</span>
        <span class="brand-text">Flawrence</span>
      </div>

      <nav class="nav">
        <router-link to="/" class="nav-link" exact-active-class="active" title="Dashboard">
          <i class="fa-solid fa-house nav-icon"></i>
          <span class="nav-text">Dashboard</span>
        </router-link>
        <router-link to="/agents" class="nav-link" exact-active-class="active" title="Agents">
          <i class="fa-solid fa-users nav-icon"></i>
          <span class="nav-text">Agents</span>
        </router-link>
      </nav>

      <div class="divider"></div>

      <div class="runs-list">
        <router-link
          v-for="job in store.jobs"
          :key="job.task_id"
          :to="`/job/${job.task_id}`"
          class="run-item"
          :class="{ active: route.path === `/job/${job.task_id}` }"
          :title="`#${job.run_id + 1} ${job.agent_name}`"
        >
          <span class="run-dot" :class="job.status.status"></span>
          <span class="run-num">{{ job.run_id + 1 }}</span>
          <span class="run-agent">{{ job.agent_name }}</span>
        </router-link>
      </div>
    </aside>

    <main class="main">
      <router-view :key="route.path" />
    </main>
  </div>
</template>

<script setup>
import { onMounted, onUnmounted } from 'vue'
import { useRoute } from 'vue-router'
import { useJobsStore } from '@/stores/jobs'

const store = useJobsStore()
const route = useRoute()
let pollId = null

onMounted(() => {
  store.loadJobs()
  pollId = setInterval(() => store.loadJobs(), 3000)
})

onUnmounted(() => {
  if (pollId) clearInterval(pollId)
})
</script>

<style>
:root {
  --bg-body: #f5f7fa;
  --bg-surface: #ffffff;
  --bg-surface-alt: #f0f2f5;
  --text-primary: #1a1a2e;
  --text-secondary: #4a4a68;
  --text-muted: #6b6b80;
  --border: #e1e4e8;
  --accent: #2563eb;
  --accent-hover: #1d4ed8;
  --success: #16a34a;
  --warning: #d97706;
  --danger: #dc2626;
  --info: #0891b2;
  --shadow: 0 1px 3px rgba(0,0,0,0.08);
  --radius: 8px;
  --sidebar-collapsed: 56px;
  --sidebar-expanded: 220px;
}

* { box-sizing: border-box; }

html, body {
  margin: 0;
  padding: 0;
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Oxygen, Ubuntu, sans-serif;
  background: var(--bg-body);
  color: var(--text-primary);
  line-height: 1.5;
}

.sidebar {
  position: fixed;
  left: 0;
  top: 0;
  width: var(--sidebar-collapsed);
  height: 100vh;
  background: var(--bg-surface);
  border-right: 1px solid var(--border);
  box-shadow: var(--shadow);
  z-index: 100;
  display: flex;
  flex-direction: column;
  transition: width 0.2s ease;
  overflow: hidden;
}

.sidebar:hover {
  width: var(--sidebar-expanded);
}

.brand {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 12px 16px;
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}

.brand-short {
  font-weight: 700;
  font-size: 1.2rem;
  color: var(--accent);
  width: 24px;
  text-align: center;
  flex-shrink: 0;
}

.brand-text {
  font-weight: 700;
  font-size: 1rem;
  color: var(--accent);
  opacity: 0;
  white-space: nowrap;
  transition: opacity 0.12s ease;
}

.sidebar:hover .brand-text {
  opacity: 1;
  transition-delay: 0.08s;
}

.nav {
  display: flex;
  flex-direction: column;
  padding: 8px;
  gap: 4px;
  flex-shrink: 0;
}

.nav-link {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 8px 10px;
  border-radius: var(--radius);
  color: var(--text-secondary);
  text-decoration: none;
  font-size: 0.9rem;
  font-weight: 500;
  transition: background 0.15s, color 0.15s;
  overflow: hidden;
}

.nav-link:hover,
.nav-link.active {
  background: var(--bg-surface-alt);
  color: var(--accent);
}

.nav-icon {
  width: 20px;
  text-align: center;
  flex-shrink: 0;
}

.nav-text {
  opacity: 0;
  white-space: nowrap;
  transition: opacity 0.12s ease;
}

.sidebar:hover .nav-text {
  opacity: 1;
  transition-delay: 0.08s;
}

.divider {
  height: 1px;
  background: var(--border);
  margin: 4px 12px;
  flex-shrink: 0;
}

.runs-list {
  flex: 1;
  overflow-y: auto;
  overflow-x: hidden;
  padding: 4px 8px;
}

.run-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 5px 8px;
  border-radius: 6px;
  text-decoration: none;
  color: var(--text-secondary);
  font-size: 0.8rem;
  transition: background 0.15s, color 0.15s;
  overflow: hidden;
  white-space: nowrap;
}

.run-item:hover,
.run-item.active {
  background: var(--bg-surface-alt);
  color: var(--accent);
}

.run-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex-shrink: 0;
}
.run-dot.running { background: var(--accent); }
.run-dot.completed { background: var(--success); }
.run-dot.failed { background: var(--danger); }

.run-num {
  font-weight: 600;
  flex-shrink: 0;
}

.run-agent {
  opacity: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  min-width: 0;
  transition: opacity 0.12s ease;
}

.sidebar:hover .run-agent {
  opacity: 1;
  transition-delay: 0.08s;
}

.main {
  margin-left: var(--sidebar-collapsed);
  padding: 24px;
  min-height: 100vh;
}

.container {
  max-width: 1400px;
  margin: 0 auto;
}

.card {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 20px;
  box-shadow: var(--shadow);
}

.badge {
  display: inline-block;
  padding: 3px 10px;
  border-radius: 999px;
  font-size: 0.75rem;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.02em;
}
.badge-running { background: #dbeafe; color: #1e40af; }
.badge-completed { background: #d1fae5; color: #065f46; }
.badge-failed { background: #fee2e2; color: #991b1b; }

pre, code {
  font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', Menlo, monospace;
  font-size: 0.85rem;
}
pre {
  background: #f6f8fa;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 12px;
  overflow-x: auto;
  white-space: pre-wrap;
  word-break: break-word;
}

/* Markdown rendered content */
.markdown-body h1, .markdown-body h2, .markdown-body h3, .markdown-body h4 {
  margin: 16px 0 8px;
  font-weight: 700;
  line-height: 1.3;
  color: var(--text-primary);
}
.markdown-body h1 { font-size: 1.4rem; border-bottom: 1px solid var(--border); padding-bottom: 4px; }
.markdown-body h2 { font-size: 1.2rem; }
.markdown-body h3 { font-size: 1.05rem; }
.markdown-body h4 { font-size: 0.95rem; }
.markdown-body p { margin: 6px 0; }
.markdown-body ul, .markdown-body ol { margin: 6px 0; padding-left: 20px; }
.markdown-body li { margin: 2px 0; }
.markdown-body blockquote {
  margin: 8px 0;
  padding: 6px 12px;
  border-left: 3px solid var(--accent);
  background: var(--bg-surface-alt);
  color: var(--text-secondary);
  border-radius: 0 var(--radius) var(--radius) 0;
}
.markdown-body code {
  background: #f0f2f5;
  padding: 1px 4px;
  border-radius: 4px;
  font-size: 0.85em;
}
.markdown-body pre {
  margin: 8px 0;
}
.markdown-body pre code {
  background: transparent;
  padding: 0;
}
.markdown-body a { color: var(--accent); text-decoration: none; }
.markdown-body a:hover { text-decoration: underline; }
.markdown-body hr { border: none; border-top: 1px solid var(--border); margin: 10px 0; }
.markdown-body table { width: 100%; border-collapse: collapse; margin: 8px 0; font-size: 0.85rem; }
.markdown-body th, .markdown-body td { border: 1px solid var(--border); padding: 6px 8px; text-align: left; }
.markdown-body th { background: var(--bg-surface-alt); font-weight: 600; }
.markdown-body tr:nth-child(even) { background: #f8fafc; }
</style>

<template>
  <div class="job-graph">
    <ul class="tree" v-if="roots.length">
      <TreeNode v-for="node in roots" :key="node.task_id" :node="node" :map="nodeMap" />
    </ul>
    <p v-else class="empty">No job hierarchy data available.</p>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import TreeNode from './TreeNode.vue'

const props = defineProps({ events: Array })

const nodeMap = computed(() => {
  const map = new Map()
  for (const ev of props.events || []) {
    const e = ev.event
    if (e.type === 'TaskStarted') {
      map.set(ev.task_id, {
        task_id: ev.task_id,
        agent: e.agent,
        title: e.title,
        parent: e.parent_task_id,
        status: 'running',
      })
    }
    if (e.type === 'SubagentSpawned') {
      if (!map.has(e.child_task_id)) {
        map.set(e.child_task_id, {
          task_id: e.child_task_id,
          agent: e.agent,
          title: e.title,
          parent: ev.task_id,
          status: 'running',
        })
      }
    }
    if (e.type === 'TaskCompleted') {
      const n = map.get(ev.task_id)
      if (n) n.status = 'completed'
    }
    if (e.type === 'TaskFailed') {
      const n = map.get(ev.task_id)
      if (n) n.status = 'failed'
    }
  }
  return map
})

const roots = computed(() => {
  const arr = []
  for (const n of nodeMap.value.values()) {
    if (!n.parent || !nodeMap.value.has(n.parent)) arr.push(n)
  }
  return arr
})
</script>

<style scoped>
.job-graph {
  overflow-x: auto;
}
.tree {
  list-style: none;
  margin: 0;
  padding: 0;
}
.empty {
  color: var(--text-muted);
  font-size: 0.9rem;
}
</style>

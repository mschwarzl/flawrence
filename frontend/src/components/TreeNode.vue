<template>
  <li class="tree-node">
    <div class="node-content" :class="node.status">
      <span class="node-agent">{{ node.agent }}</span>
      <span class="node-title">{{ node.title }}</span>
      <StatusBadge :status="node.status" />
    </div>
    <ul v-if="children.length" class="tree-children">
      <TreeNode v-for="child in children" :key="child.task_id" :node="child" :map="map" />
    </ul>
  </li>
</template>

<script setup>
import { computed } from 'vue'
import StatusBadge from './StatusBadge.vue'

const props = defineProps({ node: Object, map: Map })

const children = computed(() => {
  const arr = []
  for (const n of props.map.values()) {
    if (n.parent === props.node.task_id) arr.push(n)
  }
  return arr
})
</script>

<style scoped>
.tree-node {
  margin: 4px 0;
}
.node-content {
  display: inline-flex;
  align-items: center;
  gap: 10px;
  padding: 6px 12px;
  background: var(--bg-surface-alt);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  cursor: default;
}
.node-agent {
  font-weight: 600;
  font-size: 0.85rem;
}
.node-title {
  color: var(--text-muted);
  font-size: 0.8rem;
  max-width: 300px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.tree-children {
  list-style: none;
  margin: 4px 0 0 20px;
  padding: 0;
  border-left: 2px solid var(--border);
  padding-left: 12px;
}
</style>

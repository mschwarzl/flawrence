<template>
  <div class="artifact-list">
    <div v-if="artifacts.length === 0" class="empty">No artifacts.</div>
    <div v-else class="artifact-items">
      <div
        v-for="art in artifacts"
        :key="art.key"
        class="artifact-item"
        @click="$emit('select', { key: art.key, contentType: art.content_type })"
      >
        <i class="fa-regular fa-file-lines artifact-icon"></i>
        <span class="artifact-name">{{ art.key }}</span>
        <span class="artifact-type">{{ art.content_type }}</span>
      </div>
    </div>
  </div>
</template>

<script setup>
defineProps({ artifacts: Array })
defineEmits(['select'])
</script>

<style scoped>
.artifact-list {
  padding: 8px 0;
}
.empty {
  color: var(--text-muted);
  font-size: 0.85rem;
}
.artifact-item {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 10px;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  margin-bottom: 6px;
  cursor: pointer;
  background: var(--bg-surface-alt);
  transition: background 0.15s;
}
.artifact-item:hover {
  background: #e8eaf6;
}
.artifact-icon {
  font-size: 1rem;
  color: var(--text-muted);
  width: 18px;
  text-align: center;
}
.artifact-name {
  flex: 1;
  font-weight: 500;
  font-size: 0.85rem;
  word-break: break-all;
}
.artifact-type {
  font-size: 0.7rem;
  color: var(--text-muted);
  background: #fff;
  padding: 2px 6px;
  border-radius: 4px;
  border: 1px solid var(--border);
}
</style>

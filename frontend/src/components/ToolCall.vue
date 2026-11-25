<template>
  <div class="tool-call">
    <span class="tool-name">{{ call.name }}</span>
    <code v-if="showArgs" class="tool-args">{{ JSON.stringify(call.arguments) }}</code>
  </div>
</template>

<script setup>
defineProps({ call: Object, showArgs: Boolean })
</script>

<style scoped>
.tool-call {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  padding: 4px 8px;
  background: var(--bg-surface-alt);
  border-radius: 4px;
  margin-top: 4px;
}
.tool-name {
  font-weight: 600;
  font-size: 0.8rem;
  color: var(--accent);
  flex-shrink: 0;
  white-space: nowrap;
  padding-top: 1px;
}
.tool-args {
  font-size: 0.75rem;
  color: var(--text-muted);
  white-space: pre-wrap;
  overflow-wrap: break-word;
  word-break: break-word;
  min-width: 0;
}
</style>

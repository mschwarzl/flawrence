<template>
  <span class="badge" :class="cls">{{ label }}</span>
</template>

<script setup>
import { computed } from 'vue'

const props = defineProps({ status: String })

const map = {
  running: { label: 'Running', cls: 'badge-running' },
  completed: { label: 'Completed', cls: 'badge-completed' },
  failed: { label: 'Failed', cls: 'badge-failed' },
}

const label = computed(() => map[props.status]?.label || props.status)
const cls = computed(() => map[props.status]?.cls || '')
</script>

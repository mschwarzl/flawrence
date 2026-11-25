<template>
  <div class="artifact-viewer">
    <div class="viewer-header">
      <h4>{{ keyName }}</h4>
      <div class="viewer-actions">
        <button @click="copy" title="Copy to clipboard">
          <i class="fa-regular fa-copy"></i>
        </button>
        <button @click="$emit('close')" title="Close">
          <i class="fa-solid fa-xmark"></i>
        </button>
      </div>
    </div>
    <div class="viewer-body">
      <div v-if="isMarkdown" class="markdown-body" v-html="marked(content)"></div>
      <pre v-else class="pre-body"><code>{{ content }}</code></pre>
    </div>
  </div>
</template>

<script setup>
import { ref, watch, computed } from 'vue'
import { marked } from 'marked'
import { fetchArtifact } from '@/api'

const props = defineProps({ jobId: String, keyName: String, contentType: String })
const emit = defineEmits(['close'])

const content = ref('')

const isMarkdown = computed(() => props.contentType === 'text/markdown')

watch(() => props.keyName, async (key) => {
  if (!key) return
  try {
    const data = await fetchArtifact(props.jobId, key)
    const text = typeof data === 'string' ? data : JSON.stringify(data, null, 2)
    content.value = text
  } catch (e) {
    content.value = 'Error loading artifact.'
  }
}, { immediate: true })

async function copy() {
  try {
    await navigator.clipboard.writeText(content.value)
  } catch (e) {
    console.error('Copy failed', e)
  }
}
</script>

<style scoped>
.artifact-viewer {
  background: var(--bg-surface);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  box-shadow: 0 8px 24px rgba(0,0,0,0.12);
  display: flex;
  flex-direction: column;
  max-height: 80vh;
}
.viewer-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 16px;
  border-bottom: 1px solid var(--border);
}
.viewer-header h4 {
  margin: 0;
  font-size: 1rem;
}
.viewer-actions {
  display: flex;
  gap: 8px;
}
.viewer-actions button {
  background: var(--bg-surface-alt);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 4px 8px;
  cursor: pointer;
  font-size: 0.9rem;
  color: var(--text-secondary);
  line-height: 1;
}
.viewer-actions button:hover {
  background: var(--border);
  color: var(--text-primary);
}
.viewer-body {
  flex: 1;
  overflow: auto;
  margin: 0;
  padding: 16px;
  background: #f6f8fa;
}
.pre-body {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-word;
  overflow-wrap: break-word;
}
</style>

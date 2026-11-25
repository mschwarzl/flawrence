<template>
  <div class="token-usage" v-if="usage && Object.keys(usage).length">
    <h4>Token Usage</h4>
    <table>
      <thead>
        <tr><th>Model</th><th>Input</th><th>Output</th><th>Cache Read</th><th>Cache Write</th></tr>
      </thead>
      <tbody>
        <tr v-for="(u, model) in usage" :key="model">
          <td>{{ model }}</td>
          <td>{{ u.input_tokens }}</td>
          <td>{{ u.output_tokens }}</td>
          <td>{{ u.cache_read_tokens }}</td>
          <td>{{ u.cache_write_tokens }}</td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<script setup>
defineProps({ usage: Object })
</script>

<style scoped>
.token-usage h4 {
  margin: 0 0 8px 0;
  font-size: 0.95rem;
}
.token-usage table {
  width: 100%;
  border-collapse: collapse;
  font-size: 0.85rem;
}
.token-usage th,
.token-usage td {
  text-align: left;
  padding: 6px 8px;
  border-bottom: 1px solid var(--border);
}
.token-usage th {
  color: var(--text-muted);
  font-weight: 600;
}
</style>

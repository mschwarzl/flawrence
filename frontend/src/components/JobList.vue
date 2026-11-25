<template>
  <div class="job-list">
    <JobCard v-for="job in jobs" :key="job.task_id" :job="job" />
  </div>
</template>

<script setup>
import JobCard from './JobCard.vue'

defineProps({ jobs: Array })
</script>

<style scoped>
.job-list {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
  gap: 16px;
}
</style>

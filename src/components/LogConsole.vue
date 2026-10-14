<template>
  <div class="log-console" :class="{ 'is-embedded': embedded }">
    <div class="log-header">
      <span class="log-title">下载日志 <small>{{ logs.length }} 条</small></span>
      <el-button link size="small" @click="clearLogs" :disabled="logs.length === 0">清空</el-button>
    </div>
    <div class="log-content" ref="logContainer">
      <div v-for="(log, index) in logs" :key="index" class="log-item" :class="log.level">
        <span class="log-time">{{ formatTime(log.timestamp) }}</span>
        <span class="log-message">{{ log.message }}</span>
      </div>
      <div v-if="logs.length === 0" class="log-empty">
        暂无日志
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, watch, nextTick, computed } from 'vue'
import { useDownloadStore } from '@/stores/download'

withDefaults(defineProps<{ embedded?: boolean }>(), {
  embedded: false
})
const downloadStore = useDownloadStore()
const logContainer = ref<HTMLDivElement | null>(null)

const logs = computed(() => downloadStore.logs)

const clearLogs = (): void => {
  downloadStore.clearLogs()
}

const formatTime = (timestamp: string | null): string => {
  if (!timestamp) return ''
  const date = new Date(timestamp)
  return date.toLocaleTimeString('zh-CN', { hour12: false })
}

watch(
  () => downloadStore.logs,
  () => {
    nextTick(() => {
      if (logContainer.value) {
        logContainer.value.scrollTop = logContainer.value.scrollHeight
      }
    })
  },
  { immediate: true }
)
</script>

<style scoped>
.log-console {
  background: #151c2b;
  border-radius: 12px;
  padding: 14px;
  margin-top: 16px;
  height: 220px;
  display: flex;
  flex-direction: column;
}
.log-console.is-embedded {
  height: 210px;
  margin-top: 12px;
}
.log-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 8px;
  color: #fff;
}
.log-title {
  font-size: 13px;
  font-weight: 600;
}
.log-title small {
  color: #8590a8;
  font-size: 11px;
  font-weight: 400;
  margin-left: 6px;
}
.log-header :deep(.el-button) {
  color: #a5b4fc;
}
.log-content {
  min-height: 0;
  flex: 1 1 auto;
  overflow-y: auto;
  font-family: monospace;
  font-size: 12px;
}
.log-item {
  padding: 4px 0;
  color: #d4d4d4;
  line-height: 1.65;
}
.log-message {
  overflow-wrap: anywhere;
}
.log-item.info {
  color: #9cdcfe;
}
.log-item.warning {
  color: #ce9178;
}
.log-item.error {
  color: #f44747;
}
.log-item.success {
  color: #4ec9b0;
}
.log-time {
  color: #8590a8;
  margin-right: 8px;
}
.log-empty {
  color: #8590a8;
  text-align: center;
  padding: 20px;
}
</style>

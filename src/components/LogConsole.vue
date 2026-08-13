<template>
  <div class="log-console">
    <div class="log-header">
      <span class="log-title">控制台日志</span>
      <el-button size="small" @click="clearLogs">清空日志</el-button>
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
  () => downloadStore.logs.length,
  () => {
    nextTick(() => {
      if (logContainer.value) {
        logContainer.value.scrollTop = logContainer.value.scrollHeight
      }
    })
  }
)
</script>

<style scoped>
.log-console {
  background: #1e1e1e;
  border-radius: 8px;
  padding: 12px;
  margin-top: 16px;
  height: 220px;
  display: flex;
  flex-direction: column;
}
.log-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 8px;
  color: #fff;
}
.log-title {
  font-weight: bold;
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
  color: #6a9955;
  margin-right: 8px;
}
.log-empty {
  color: #6a9955;
  text-align: center;
  padding: 20px;
}
</style>

<template>
  <div class="history-page">
    <el-card class="history-card">
      <template #header>
        <div class="card-header">
          <span>下载历史</span>
          <el-button type="danger" size="default" @click="clearHistory" :disabled="history.length === 0">
            清空历史
          </el-button>
        </div>
      </template>

      <el-empty v-if="history.length === 0" description="暂无下载历史" />

      <div v-else class="history-list">
        <div v-for="item in history" :key="item.id" class="history-item">
          <div class="item-header">
            <el-tag :type="getStatusType(item.status)">
              {{ getStatusText(item.status) }}
            </el-tag>
            <span class="item-name">{{ item.packageName }}:{{ item.tag }}</span>
            <span class="item-arch">({{ item.arch }})</span>
          </div>
          <div class="item-details">
            <div class="detail-row">
              <span class="detail-label">镜像源:</span>
              <span class="detail-value">{{ item.mirror }}</span>
            </div>
            <div class="detail-row">
              <span class="detail-label">完整地址:</span>
              <span class="detail-value">{{ item.fullImage }}</span>
            </div>
            <div v-if="item.outputPath" class="detail-row">
              <span class="detail-label">输出路径:</span>
              <span class="detail-value">{{ item.outputPath }}</span>
            </div>
            <div class="detail-row">
              <span class="detail-label">大小:</span>
              <span class="detail-value">{{ item.size || '未知' }}</span>
            </div>
            <div class="detail-row">
              <span class="detail-label">时间:</span>
              <span class="detail-value">{{ formatTime(item.downloadedAt) }}</span>
            </div>
            <div v-if="item.error" class="detail-row error">
              <span class="detail-label">错误:</span>
              <span class="detail-value">{{ item.error }}</span>
            </div>
          </div>
          <div class="item-actions">
            <el-button size="small" @click="redownload(item)">重新下载</el-button>
            <el-button v-if="item.outputPath" size="small" @click="openDirectory(item.outputPath)">打开目录</el-button>
            <el-button type="danger" size="small" @click="deleteHistory(item.id)">删除</el-button>
          </div>
        </div>
      </div>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessage, ElMessageBox } from 'element-plus'
import { historyApi, systemApi } from '@/api/tauri'
import { useSettingsStore } from '@/stores/settings'
import type { HistoryItem } from '@/types/api'

const router = useRouter()
const settingsStore = useSettingsStore()
const history = ref<HistoryItem[]>([])

onMounted(async () => {
  await loadHistory()
})

const loadHistory = async () => {
  try {
    history.value = await historyApi.list()
  } catch (error) {
    ElMessage.error('加载历史记录失败')
  }
}

const clearHistory = async () => {
  try {
    await ElMessageBox.confirm('确定要清空所有历史记录吗？', '警告', {
      type: 'warning'
    })
    await historyApi.clear()
    history.value = []
    ElMessage.success('历史记录已清空')
  } catch (error) {
    if (error !== 'cancel') {
      ElMessage.error('清空失败')
    }
  }
}

const deleteHistory = async (id: string): Promise<void> => {
  try {
    await historyApi.delete(id)
    history.value = history.value.filter((h: HistoryItem) => h.id !== id)
    ElMessage.success('已删除')
  } catch (error) {
    ElMessage.error('删除失败')
  }
}

const redownload = async (item: HistoryItem): Promise<void> => {
  try {
    await settingsStore.loadSettings()
    const mirrorExists = settingsStore.mirrors.some(mirror => mirror.registry === item.mirror)
    const mirror = mirrorExists ? item.mirror : settingsStore.getDefaultMirrorRegistry()

    if (!mirrorExists) {
      ElMessage.warning(`镜像源 ${item.mirror} 已不存在，将使用默认镜像源`)
    }

    await router.push({
      path: '/',
      query: {
        redownload: 'true',
        packageName: item.packageName,
        tag: item.tag,
        arch: item.arch,
        mirror
      }
    })
  } catch (error) {
    ElMessage.error(commandErrorMessage(error, '重新下载准备失败'))
  }
}

const openDirectory = async (path: string): Promise<void> => {
  try {
    await systemApi.openParent(path)
  } catch (error) {
    ElMessage.error(commandErrorMessage(error, '打开目录失败'))
  }
}

const formatTime = (timestamp: string | null): string => {
  if (!timestamp) return '未知'
  return new Date(timestamp).toLocaleString('zh-CN')
}

const getStatusText = (status: string): string => {
  const statusMap: Record<string, string> = {
    completed: '完成',
    failed: '失败',
    cancelled: '已取消',
    downloading: '下载中',
    preparing: '准备中',
    verifying: '校验中',
    packing: '打包中',
    idle: '等待'
  }
  return statusMap[status] || '未知'
}

const getStatusType = (status: string): 'success' | 'danger' | 'warning' | 'info' | 'primary' => {
  const typeMap: Record<string, 'success' | 'danger' | 'warning' | 'info' | 'primary'> = {
    completed: 'success',
    failed: 'danger',
    cancelled: 'warning',
    downloading: 'primary',
    preparing: 'primary',
    verifying: 'primary',
    packing: 'primary',
    idle: 'info'
  }
  return typeMap[status] || 'info'
}

const commandErrorMessage = (error: unknown, fallback: string): string => {
  if (typeof error === 'object' && error !== null && 'message' in error) {
    const message = (error as { message?: unknown }).message
    if (typeof message === 'string' && message.trim()) return message
  }
  return error instanceof Error && error.message ? error.message : fallback
}
</script>

<style scoped>
.history-page {
  padding: 20px;
  max-width: 960px;
  margin: 0 auto;
  height: calc(100vh - 60px);
  overflow: hidden;
}
.history-card {
  height: 92%;
}
.history-card :deep(.el-card__body) {
  height: calc(100% - 60px);
  overflow-y: auto;
  padding: 20px;
}
.card-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
}
.history-list {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.history-item {
  background-color: #fafafa;
  border: 1px solid #e4e7ed;
  border-radius: 8px;
  padding: 12px;
}
.item-header {
  font-weight: bold;
  margin-bottom: 8px;
  display: flex;
  align-items: center;
  gap: 8px;
}
.item-arch {
  color: #909399;
}
.item-details {
  margin-bottom: 8px;
}
.detail-row {
  display: flex;
  margin-bottom: 4px;
  font-size: 13px;
}
.detail-label {
  color: #909399;
  width: 80px;
}
.detail-value {
  color: #606266;
}
.detail-row.error .detail-value {
  color: #f56c6c;
}
.item-actions {
  display: flex;
  gap: 8px;
}
</style>

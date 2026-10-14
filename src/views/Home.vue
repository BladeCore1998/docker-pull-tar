<template>
  <div class="home-page">
    <div class="home-hero">
      <h1>下载镜像</h1>
      <span class="card-note">{{ normalizedImages().length }} / 10 个镜像</span>
    </div>

    <el-card class="download-card" shadow="never">
      <el-form :model="form" label-position="top" class="download-form">
        <el-form-item class="form-item-images" label="镜像列表">
          <div class="image-list">
            <div v-for="(_, index) in form.images" :key="index" class="image-row-wrap">
              <div class="image-row">
                <span class="image-index">{{ index + 1 }}</span>
                <el-input
                  v-model="form.images[index]"
                  :placeholder="index === 0 ? '例如：nginx:latest' : '输入镜像名称'"
                  :disabled="isDownloading"
                  clearable
                  @blur="validateImage(index)"
                >
                  <template #append v-if="index === form.images.length - 1">
                    <el-button
                      aria-label="添加镜像"
                      title="添加镜像"
                      @click="addImage"
                      :disabled="isDownloading || isImageLimitReached"
                    >
                      <el-icon><Plus /></el-icon>
                    </el-button>
                  </template>
                </el-input>
                <el-button
                  v-if="form.images.length > 1"
                  class="image-remove"
                  link
                  type="danger"
                  aria-label="移除镜像"
                  title="移除镜像"
                  @click="removeImage(index)"
                  :disabled="isDownloading"
                >
                  <el-icon><Delete /></el-icon>
                </el-button>
              </div>
              <div v-if="imageErrors[index]" class="field-error">{{ imageErrors[index] }}</div>
            </div>
          </div>
          <div class="image-toolbar">
            <span class="form-help">批量下载统一使用所选镜像源</span>
            <div class="image-actions">
              <el-button link @click="parseAndPreview" :disabled="isDownloading">
                检查镜像
              </el-button>
            </div>
          </div>
        </el-form-item>

        <el-form-item class="form-item-mirror" label="镜像源">
          <div class="mirror-control">
            <MirrorSelector
              v-model="form.mirror"
              :disabled="isDownloading"
              @update:model-value="handleMirrorChange"
            />
            <el-button
              type="primary"
              plain
              @click="validateMirror"
              :disabled="isDownloading"
            >
              <el-icon><CircleCheck /></el-icon>
              校验
            </el-button>
          </div>
          <div v-if="mirrorValidation.message" :class="['mirror-status', `is-${mirrorValidation.state}`]">
            {{ mirrorValidation.message }}
          </div>
        </el-form-item>

        <el-form-item class="form-item-arch" label="架构">
          <el-select v-model="form.arch" :disabled="isDownloading">
            <el-option label="amd64 (x86_64)" value="amd64" />
            <el-option label="arm64 (ARMv8)" value="arm64" />
            <el-option label="armv7" value="armv7" />
          </el-select>
        </el-form-item>

        <el-form-item class="form-item-output" label="输出目录">
          <el-input v-model="form.outputDir" :disabled="isDownloading">
            <template #append>
              <el-button @click="selectOutputDir" :disabled="isDownloading">选择</el-button>
            </template>
          </el-input>
        </el-form-item>

        <el-form-item class="form-actions">
          <el-button
            type="primary"
            size="large"
            @click="startDownload"
            :loading="isDownloading"
            :disabled="!hasImageInput"
          >
            {{ isDownloading ? '下载中...' : '开始下载' }}
          </el-button>
          <el-button
            v-if="isDownloading"
            size="large"
            @click="cancelDownload"
          >
            {{ downloadStore.batch ? '取消全部' : '取消下载' }}
          </el-button>
          <el-button
            v-if="hasProgress"
            size="large"
            @click="openProgressDialog"
          >
            查看下载进度
          </el-button>
        </el-form-item>
      </el-form>

      <el-card v-if="downloadStore.batch" class="batch-card" shadow="never">
        <template #header>
          <div class="batch-header">
            <span class="batch-title">下载列表</span>
            <span class="batch-summary">
              {{ downloadStore.batch.tasks.filter(task => task.status === 'completed').length }} /
              {{ downloadStore.batch.tasks.length }} 已完成
              <el-tag>{{ batchStatusText(downloadStore.batch.status) }}</el-tag>
            </span>
          </div>
        </template>
        <div
          v-for="task in downloadStore.batch.tasks"
          :key="task.taskId || task.image.reference"
          class="task-row"
          :class="{ 'is-selected': task.taskId === downloadStore.selectedTaskId }"
        >
          <button
            type="button"
            class="task-main"
            :aria-label="`查看 ${formatTaskImage(task)} 的进度`"
            @click="openTaskProgress(task.taskId)"
          >
            <span class="task-image">{{ formatTaskImage(task) }}</span>
            <el-progress
              :percentage="taskPercentage(task)"
              :stroke-width="6"
              :show-text="false"
              class="task-progress"
            />
          </button>
          <span class="task-layer">{{ task.download.progress.currentLayer || 0 }}/{{ task.download.progress.totalLayers || 0 }} 层</span>
          <el-tag size="small" :type="taskTagType(task.status)">{{ taskStatusText(task.status) }}</el-tag>
          <el-button
            v-if="task.status === 'running' || task.status === 'queued'"
            size="small"
            @click.stop="cancelTask(task.taskId)"
          >
            取消
          </el-button>
          <el-button
            v-if="task.status === 'failed' || task.status === 'cancelled'"
            size="small"
            type="warning"
            @click.stop="retryTask(task.taskId)"
          >
            重试
          </el-button>
        </div>
        <el-button
          v-if="downloadStore.batch.status === 'queued' || downloadStore.batch.status === 'running'"
          type="danger"
          size="small"
          @click="cancelDownload"
        >
          取消整个批次
        </el-button>
      </el-card>

      <button
        v-else-if="downloadStore.snapshot.taskId"
        type="button"
        class="single-task-summary"
        @click="openProgressDialog"
      >
        <el-icon class="summary-icon"><Box /></el-icon>
        <div class="summary-copy">
          <span>{{ formatImageReference(downloadStore.snapshot.image) }}</span>
          <small>{{ downloadStatusText(downloadStore.snapshot.status) }}</small>
        </div>
        <span class="summary-action">查看进度 <el-icon><ArrowRight /></el-icon></span>
      </button>

      <el-dialog
        v-model="progressDialogVisible"
        title="下载进度"
        width="min(880px, calc(100vw - 32px))"
        top="6vh"
        class="progress-dialog"
        append-to-body
        :destroy-on-close="false"
      >
        <div v-if="downloadStore.batch" class="progress-dialog-layout">
          <div class="progress-task-list">
            <div class="progress-task-list-header">
              <span>批次任务</span>
              <span>{{ downloadStore.batch.tasks.length }} 个镜像</span>
            </div>
            <button
              v-for="task in downloadStore.batch.tasks"
              :key="task.taskId"
              type="button"
              class="progress-task-button"
              :class="{ 'is-selected': task.taskId === downloadStore.selectedTaskId }"
              :aria-pressed="task.taskId === downloadStore.selectedTaskId"
              @click="openTaskProgress(task.taskId)"
            >
              <span class="progress-task-name">{{ formatTaskImage(task) }}</span>
              <span class="progress-task-meta">
                <span>{{ taskStatusText(task.status) }}</span>
                <span>{{ taskPercentage(task) }}%</span>
              </span>
              <el-progress
                :percentage="taskPercentage(task)"
                :stroke-width="6"
                :show-text="false"
              />
            </button>
          </div>
          <div v-if="selectedTask" class="progress-detail">
            <div class="progress-detail-heading">
              <span class="progress-detail-name" :title="formatTaskImage(selectedTask)">
                {{ formatTaskImage(selectedTask) }}
              </span>
              <el-tag size="small" :type="taskTagType(selectedTask.status)">
                {{ taskStatusText(selectedTask.status) }}
              </el-tag>
            </div>
            <DownloadProgress embedded />
            <LogConsole embedded />
          </div>
        </div>
        <div v-else class="progress-detail">
          <DownloadProgress embedded />
          <LogConsole embedded />
        </div>
        <template #footer>
          <el-button @click="progressDialogVisible = false">关闭</el-button>
          <el-button
            v-if="canCancelSelectedTask"
            type="danger"
            @click="cancelSelectedProgress"
          >
            取消当前下载
          </el-button>
        </template>
      </el-dialog>
    </el-card>

  </div>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { ArrowRight, Box, CircleCheck, Delete, Plus } from '@element-plus/icons-vue'
import { useSettingsStore } from '@/stores/settings'
import { useDownloadStore } from '@/stores/download'
import { parseImageInput } from '@/utils/imageParser'
import { systemApi } from '@/api/tauri'
import MirrorSelector from '@/components/MirrorSelector.vue'
import DownloadProgress from '@/components/DownloadProgress.vue'
import LogConsole from '@/components/LogConsole.vue'
import type { BatchStatus, BatchTaskSnapshot, BatchTaskStatus, DownloadStatus, ImageReference } from '@/types/api'

const route = useRoute()
const router = useRouter()
const settingsStore = useSettingsStore()
const downloadStore = useDownloadStore()
let batchPollTimer: number | null = null

interface FormState {
  images: string[]
  mirror: string
  arch: string
  outputDir: string
}

const form = ref<FormState>({
  images: [''],
  mirror: '',
  arch: 'amd64',
  outputDir: ''
})

const isDownloading = computed<boolean>(() => downloadStore.isDownloading)
const progressDialogVisible = ref(false)
const hasImageInput = computed(() => form.value.images.some(image => image.trim()))
const hasProgress = computed(() =>
  Boolean(downloadStore.batch) ||
  Boolean(downloadStore.snapshot.taskId) ||
  downloadStore.snapshot.status !== 'idle'
)
const selectedTask = computed<BatchTaskSnapshot | null>(() => {
  if (!downloadStore.batch) return null
  return downloadStore.batch.tasks.find(task => task.taskId === downloadStore.selectedTaskId) ||
    downloadStore.batch.tasks[0] ||
    null
})
const canCancelSelectedTask = computed(() => {
  const task = selectedTask.value
  if (task) return task.status === 'queued' || task.status === 'running'
  const activeStatuses: DownloadStatus[] = ['preparing', 'downloading', 'verifying', 'packing']
  return activeStatuses.includes(downloadStore.snapshot.status)
})
const isImageLimitReached = computed(() => form.value.images.length >= 10)
const imageErrors = ref<string[]>([])
const mirrorValidation = ref<{
  state: 'idle' | 'valid' | 'invalid'
  message: string
}>({
  state: 'idle',
  message: ''
})

const normalizedImages = (): string[] =>
  form.value.images.map(image => image.trim()).filter(Boolean)

const isValidRegistry = (value: string): boolean => {
  const registry = value.trim().replace(/^https?:\/\//i, '').replace(/\/+$/, '')
  if (!registry || /\s/.test(registry) || registry.includes('/')) return false
  try {
    const parsed = new URL(`https://${registry}`)
    return Boolean(parsed.hostname) && parsed.pathname === '/' && !parsed.search && !parsed.hash
  } catch {
    return false
  }
}

const normalizeRegistry = (value: string): string =>
  value.trim().replace(/^https?:\/\//i, '').replace(/\/+$/, '').toLowerCase()

const validateMirror = (showMessage = true): boolean => {
  const registry = form.value.mirror.trim()
  const configuredMirror = settingsStore.getMirrorByRegistry(registry)
  if (!configuredMirror) {
    mirrorValidation.value = {
      state: 'invalid',
      message: '请选择已配置的镜像源'
    }
    if (showMessage) ElMessage.warning(mirrorValidation.value.message)
    return false
  }
  if (!isValidRegistry(configuredMirror.registry)) {
    mirrorValidation.value = {
      state: 'invalid',
      message: '镜像源地址格式无效，请到设置中修正'
    }
    if (showMessage) ElMessage.warning(mirrorValidation.value.message)
    return false
  }
  form.value.mirror = configuredMirror.registry
  mirrorValidation.value = {
    state: 'valid',
    message: `地址格式正确：${configuredMirror.registry}`
  }
  if (showMessage) ElMessage.success('镜像源地址格式校验通过')
  return true
}

const validateImage = (index: number): boolean => {
  const value = form.value.images[index]?.trim() || ''
  if (!value) {
    imageErrors.value[index] = ''
    return true
  }
  const parsed = parseImageInput(value, form.value.mirror)
  imageErrors.value[index] = parsed ? '' : '镜像名称格式无效'
  if (
    parsed &&
    form.value.images.filter(image => image.trim()).length > 1 &&
    normalizeRegistry(parsed.registry) !== normalizeRegistry(form.value.mirror)
  ) {
    imageErrors.value[index] = '批量下载时所有镜像必须使用同一个镜像源'
    return false
  }
  return Boolean(parsed)
}

const validateImages = (showMessage = true): boolean => {
  imageErrors.value = form.value.images.map(() => '')
  let valid = true
  form.value.images.forEach((_, index) => {
    valid = validateImage(index) && valid
  })
  const images = normalizedImages()
  if (images.length === 0) {
    if (showMessage) ElMessage.warning('请输入至少一个镜像名称')
    return false
  }
  if (!valid) {
    if (showMessage) ElMessage.warning('请先修正镜像名称')
    return false
  }
  return true
}

const addImage = () => {
  if (isImageLimitReached.value) {
    ElMessage.warning('单批次最多支持 10 个镜像')
    return
  }
  form.value.images.push('')
}

const removeImage = (index: number) => {
  if (form.value.images.length === 1) {
    form.value.images[0] = ''
    imageErrors.value[0] = ''
    return
  }
  form.value.images.splice(index, 1)
  imageErrors.value.splice(index, 1)
}

const handleMirrorChange = (value: string) => {
  form.value.mirror = value
  mirrorValidation.value = { state: 'idle', message: '' }
}

const formatImageReference = (image: ImageReference): string =>
  `${image.registry}/${image.repository}${image.reference.startsWith('sha256:') ? '@' : ':'}${image.reference}`

const formatTaskImage = (task: BatchTaskSnapshot): string => formatImageReference(task.image)

const batchStatusText = (status: BatchStatus): string => ({
  queued: '等待中',
  running: '下载中',
  cancelling: '取消中',
  completed: '已完成',
  partial_failed: '部分失败',
  failed: '失败',
  cancelled: '已取消'
})[status]

const downloadStatusText = (status: DownloadStatus): string => ({
  idle: '等待中',
  preparing: '准备中',
  downloading: '下载中',
  verifying: '校验中',
  packing: '打包中',
  completed: '已完成',
  failed: '失败',
  cancelled: '已取消'
})[status]

const taskPercentage = (task: BatchTaskSnapshot): number => {
  const { downloadedBytes, totalBytes } = task.download.progress
  if (task.status === 'completed' || task.download.status === 'completed') return 100
  if (!totalBytes) return 0
  return Math.min(100, Math.round((downloadedBytes / totalBytes) * 100))
}

const taskStatusText = (taskStatus: BatchTaskStatus): string => {
  const texts: Record<BatchTaskStatus, string> = {
    queued: '等待中',
    running: '下载中',
    completed: '已完成',
    failed: '失败',
    cancelled: '已取消'
  }
  return texts[taskStatus]
}

const taskTagType = (taskStatus: BatchTaskStatus): 'success' | 'danger' | 'warning' | 'info' | undefined => {
  if (taskStatus === 'completed') return 'success'
  if (taskStatus === 'failed') return 'danger'
  if (taskStatus === 'cancelled') return 'warning'
  if (taskStatus === 'queued') return 'info'
  return undefined
}

const openProgressDialog = () => {
  if (downloadStore.batch && !downloadStore.selectedTaskId) {
    const firstTask = downloadStore.batch.tasks[0]
    if (firstTask) downloadStore.selectTask(firstTask.taskId)
  }
  progressDialogVisible.value = true
}

const openTaskProgress = (taskId: string) => {
  downloadStore.selectTask(taskId)
  progressDialogVisible.value = true
}

const cancelTask = async (taskId: string) => {
  try {
    await downloadStore.cancelBatchTask(taskId)
    ElMessage.info('已取消该镜像下载')
  } catch (error) {
    ElMessage.error(commandErrorMessage(error, '取消失败'))
  }
}

const retryTask = async (taskId: string) => {
  try {
    await downloadStore.retryBatchTask(taskId)
    ElMessage.info('已重新加入下载队列')
  } catch (error) {
    ElMessage.error(commandErrorMessage(error, '重试失败'))
  }
}

const cancelSelectedProgress = async () => {
  if (selectedTask.value) {
    await cancelTask(selectedTask.value.taskId)
    return
  }
  await cancelDownload()
}

watch(() => form.value.mirror, () => {
  mirrorValidation.value = { state: 'idle', message: '' }
})

onMounted(async () => {
  try {
    await settingsStore.loadSettings()
    await downloadStore.initialize()
    batchPollTimer = window.setInterval(() => {
      if (downloadStore.batch && !['completed', 'failed', 'partial_failed', 'cancelled'].includes(downloadStore.batch.status)) {
        downloadStore.refreshBatch().catch(() => undefined)
      }
    }, 1000)
    form.value.arch = settingsStore.settings.defaultArch
    form.value.outputDir = settingsStore.settings.defaultOutputDir
    form.value.mirror = settingsStore.getDefaultMirrorRegistry()
    const savedImages = settingsStore.settings.lastPackageName
      ?.split(/\r?\n/)
      .map(value => value.trim())
      .filter(Boolean)
      .slice(0, 10)
    form.value.images = savedImages?.length ? savedImages : ['']

    // 处理重新下载参数
    if (route.query.redownload) {
      const packageName = route.query.packageName as string
      const tag = route.query.tag as string
      const arch = route.query.arch as string
      const mirror = route.query.mirror as string

      if (packageName) {
        form.value.images = [tag ? `${packageName}:${tag}` : packageName]
      }
      if (arch) {
        form.value.arch = arch
      }
      if (mirror) {
        // 检查镜像源是否存在
        const mirrorExists = settingsStore.mirrors.some(m => m.registry === mirror)
        form.value.mirror = mirrorExists ? mirror : settingsStore.getDefaultMirrorRegistry()
      }

      // 清除 query 参数，避免刷新页面时重复填充
      router.replace({ path: '/' })
    }

  } catch (error) {
    console.error('Failed to load settings on mount:', error)
    ElMessage.error(commandErrorMessage(error, '加载配置失败'))
    // 使用默认值
    form.value.arch = 'amd64'
    form.value.outputDir = ''
    form.value.mirror = 'registry-1.docker.io'
    form.value.images = ['']
  }
})

onUnmounted(() => {
  if (batchPollTimer !== null) window.clearInterval(batchPollTimer)
})

const parseAndPreview = () => {
  if (!validateMirror(false) || !validateImages()) return
  const images = normalizedImages()
  const previews = images
    .map(image => parseImageInput(image, form.value.mirror)?.fullImage)
    .filter((value): value is string => Boolean(value))
  ElMessage.success(`已识别 ${previews.length} 个镜像`)
}

const startDownload = async () => {
  if (!form.value.outputDir) {
    ElMessage.warning('请选择输出目录')
    return
  }

  try {
    if (!validateMirror(false) || !validateImages()) return
    const images = normalizedImages()
    const packageName = images.join('\n')
    if (form.value.outputDir !== settingsStore.settings.defaultOutputDir || packageName !== settingsStore.settings.lastPackageName) {
      await settingsStore.updateSettings({
        defaultOutputDir: form.value.outputDir,
        lastPackageName: packageName
      })
    }

    if (images.length === 1) {
      await downloadStore.startDownload(images[0], form.value.outputDir, form.value.mirror, form.value.arch)
    } else {
      await downloadStore.startBatch(images, form.value.outputDir, form.value.mirror, form.value.arch)
    }
    openProgressDialog()
    ElMessage.success('开始下载')
  } catch (error: unknown) {
    ElMessage.error(commandErrorMessage(error, '下载失败'))
  }
}

const cancelDownload = async () => {
  try {
    if (downloadStore.batch) await downloadStore.cancelBatch()
    else await downloadStore.cancelDownload()
    ElMessage.info('下载已取消')
  } catch (error) {
    ElMessage.error(commandErrorMessage(error, '取消失败'))
  }
}

const selectOutputDir = async () => {
  try {
    const path = await systemApi.selectDirectory(form.value.outputDir)
    if (path) {
      form.value.outputDir = path
      // 自动保存到 settings（持久化），同时保存当前镜像列表
      await settingsStore.updateSettings({
        defaultOutputDir: path,
        lastPackageName: normalizedImages().join('\n')
      })
      ElMessage.success('输出目录已保存')
    }
  } catch (error) {
    ElMessage.error(commandErrorMessage(error, '选择目录失败'))
  }
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
.home-page {
  box-sizing: border-box;
  max-width: 1040px;
  margin: 0 auto;
  padding: 24px 28px 32px;
}
.home-hero {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  margin-bottom: 18px;
}
.home-hero h1 {
  color: #172033;
  font-size: 24px;
  font-weight: 650;
  letter-spacing: -0.03em;
  margin: 0;
}
.card-note, .form-help {
  color: #7b8499;
  font-size: 12px;
}
.download-card {
  background: rgba(255, 255, 255, 0.92);
  border: 0;
  border-radius: 18px;
  box-shadow: 0 12px 38px rgba(61, 70, 128, 0.06);
}
:deep(.download-card > .el-card__body) {
  padding: 24px;
}
.download-form {
  display: grid;
  column-gap: 20px;
  grid-template-columns: minmax(0, 1.6fr) minmax(180px, 1fr);
}
.download-form :deep(.el-form-item) {
  margin-bottom: 20px;
  min-width: 0;
}
.download-form :deep(.el-form-item__label) {
  color: #526078;
  font-size: 13px;
  font-weight: 600;
  margin-bottom: 8px;
  padding: 0;
}
.download-form :deep(.el-form-item__content) {
  min-width: 0;
}
.form-item-images, .form-item-output, .form-actions {
  grid-column: 1 / -1;
}
.form-item-arch :deep(.el-select) {
  width: 100%;
}
.image-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
  width: 100%;
}
.image-row {
  display: grid;
  align-items: center;
  gap: 8px;
  grid-template-columns: 20px minmax(0, 1fr) 28px;
}
.image-index {
  color: #8b93a8;
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  text-align: center;
}
.image-remove {
  height: 36px;
  padding: 0;
  width: 28px;
}
.field-error {
  color: #de5264;
  font-size: 12px;
  line-height: 18px;
  margin: 4px 36px 0 28px;
}
.image-toolbar {
  align-items: center;
  display: flex;
  justify-content: space-between;
  gap: 12px;
  margin-top: 6px;
  padding-left: 28px;
  width: 100%;
}
.image-actions {
  flex: 0 0 auto;
}
.mirror-control {
  align-items: center;
  display: flex;
  gap: 8px;
  width: 100%;
}
.mirror-control :deep(.mirror-selector) {
  flex: 1;
  min-width: 0;
}
.mirror-status {
  font-size: 12px;
  line-height: 18px;
  margin-top: 5px;
}
.mirror-status.is-valid {
  color: #17926b;
}
.mirror-status.is-invalid {
  color: #de5264;
}
.form-actions {
  margin-bottom: 0 !important;
}
.form-actions :deep(.el-form-item__content) {
  display: flex;
  gap: 10px;
  flex-wrap: wrap;
}
.form-actions :deep(.el-button) {
  margin-left: 0;
  min-width: 104px;
}
.form-actions :deep(.el-button--primary) {
  background: linear-gradient(125deg, #6155d9, #448cce);
  box-shadow: 0 6px 16px rgba(97, 85, 217, 0.18);
}
.batch-card {
  background: #f6f8fc;
  border: 0;
  border-radius: 14px;
  margin-top: 24px;
}
.batch-card :deep(.el-card__header) {
  border: 0;
  padding: 16px 16px 8px;
}
.batch-card :deep(.el-card__body) {
  padding: 0 10px 10px;
}
.batch-header, .batch-summary, .task-row {
  align-items: center;
  display: flex;
  gap: 12px;
  justify-content: space-between;
}
.batch-title {
  color: #263249;
  font-size: 14px;
  font-weight: 600;
}
.batch-summary {
  color: #7b8499;
  font-size: 12px;
}
.task-row {
  border-radius: 10px;
  padding: 10px 8px;
  transition: background 0.15s;
}
.task-row:hover, .task-row.is-selected {
  background: #edf0fa;
}
.task-main {
  background: transparent;
  border: 0;
  cursor: pointer;
  flex: 1;
  font: inherit;
  min-width: 0;
  padding: 0;
  text-align: left;
}
.task-image {
  color: #35415a;
  display: block;
  font-size: 13px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.task-progress {
  margin-top: 7px;
}
.task-layer {
  color: #7b8499;
  flex: 0 0 auto;
  font-size: 12px;
}
.single-task-summary {
  align-items: center;
  background: #f6f8fc;
  border: 0;
  border-radius: 12px;
  cursor: pointer;
  display: flex;
  font: inherit;
  gap: 12px;
  margin-top: 24px;
  padding: 14px;
  text-align: left;
  width: 100%;
}
.summary-icon {
  background: #eceefa;
  border-radius: 10px;
  color: #6155d9;
  height: 36px;
  width: 36px;
}
.summary-copy {
  color: #35415a;
  display: flex;
  flex: 1;
  flex-direction: column;
  font-size: 13px;
  gap: 4px;
  min-width: 0;
}
.summary-copy > span {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.summary-copy small {
  color: #7b8499;
  font-size: 12px;
}
.summary-action {
  align-items: center;
  color: #6155d9;
  display: flex;
  flex: 0 0 auto;
  font-size: 12px;
  gap: 4px;
}
.progress-dialog-layout {
  display: grid;
  gap: 20px;
  grid-template-columns: 230px minmax(0, 1fr);
}
.progress-task-list {
  max-height: min(68vh, 600px);
  min-width: 0;
  overflow-y: auto;
}
.progress-task-list-header {
  align-items: center;
  color: #7b8499;
  display: flex;
  font-size: 12px;
  justify-content: space-between;
  margin-bottom: 10px;
}
.progress-task-button {
  background: #f6f8fc;
  border: 0;
  border-radius: 10px;
  color: #35415a;
  cursor: pointer;
  display: block;
  font: inherit;
  margin-bottom: 8px;
  padding: 12px;
  text-align: left;
  transition: background 0.15s;
  width: 100%;
}
.progress-task-button:hover, .progress-task-button.is-selected {
  background: #e9edfc;
}
.progress-task-name {
  display: block;
  font-size: 13px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.progress-task-meta {
  color: #7b8499;
  display: flex;
  font-size: 12px;
  justify-content: space-between;
  margin: 6px 0;
}
.progress-detail {
  min-width: 0;
}
.progress-detail-heading {
  align-items: center;
  color: #263249;
  display: flex;
  font-size: 14px;
  font-weight: 600;
  gap: 8px;
  justify-content: space-between;
  margin-bottom: 14px;
}
.progress-detail-name {
  overflow-wrap: anywhere;
}
.task-main:focus-visible, .progress-task-button:focus-visible, .single-task-summary:focus-visible {
  outline: 2px solid #6155d9;
  outline-offset: 2px;
}
@media (max-width: 720px) {
  .home-page {
    padding: 20px 14px 28px;
  }
  :deep(.download-card > .el-card__body) {
    padding: 20px;
  }
  .download-form {
    grid-template-columns: minmax(0, 1fr);
  }
  .task-layer {
    display: none;
  }
  .progress-dialog-layout {
    grid-template-columns: minmax(0, 1fr);
  }
  .progress-task-list {
    max-height: 170px;
  }
}
</style>

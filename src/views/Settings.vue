<template>
  <div class="settings-page">
    <el-card class="settings-card">
      <template #header>
        <div class="card-header">
          <span>设置</span>
          <el-button type="primary" size="default" @click="showAddMirror">添加镜像源</el-button>
        </div>
      </template>

      <div class="card-body">
        <div class="section-title">基本设置</div>
        <el-form :model="settings" label-position="top" class="settings-form">
          <el-form-item label="默认输出目录">
            <el-input v-model="settings.defaultOutputDir">
              <template #append>
                <el-button @click="selectOutputDir">选择</el-button>
              </template>
            </el-input>
          </el-form-item>

          <el-form-item label="默认架构">
            <el-select v-model="settings.defaultArch">
              <el-option label="amd64" value="amd64" />
              <el-option label="arm64" value="arm64" />
              <el-option label="armv7" value="armv7" />
            </el-select>
          </el-form-item>

          <el-form-item label="下载线程数" class="workers-field">
            <el-slider v-model="settings.downloadWorkers" :min="1" :max="8" show-stops />
          </el-form-item>

          <el-form-item class="save-field">
            <el-button type="primary" @click="saveSettings">保存设置</el-button>
          </el-form-item>
        </el-form>

        <el-divider />

        <div class="section-title">镜像源管理</div>
        <el-table :data="mirrors" style="width: 100%" empty-text="暂无镜像源">
          <el-table-column prop="name" label="名称" width="120" />
          <el-table-column prop="registry" label="地址" />
          <el-table-column label="认证" width="100">
            <template #default="{ row }">
              <el-tag v-if="row.username" type="success" size="small">已配置</el-tag>
              <el-tag v-else type="info" size="small">无</el-tag>
            </template>
          </el-table-column>
          <el-table-column label="默认" width="100">
            <template #default="{ row }">
              <el-tag v-if="row.isDefault" type="success">默认</el-tag>
              <el-button v-else size="small" @click="setDefaultMirror(row.id)">设为默认</el-button>
            </template>
          </el-table-column>
          <el-table-column label="操作" width="140">
            <template #default="{ row }">
              <el-button size="small" @click="editMirror(row)">编辑</el-button>
              <el-button v-if="!row.isDefault" type="danger" size="small" @click="deleteMirror(row.id)">删除</el-button>
            </template>
          </el-table-column>
        </el-table>
      </div>
    </el-card>

    <el-dialog v-model="addMirrorVisible" :title="editMode ? '编辑镜像源' : '添加镜像源'" width="400px">
      <el-form :model="newMirror" label-width="80px">
        <el-form-item label="名称">
          <el-input v-model="newMirror.name" placeholder="例如: 自定义镜像源" />
        </el-form-item>
        <el-form-item label="地址">
          <el-input v-model="newMirror.registry" placeholder="例如: my.registry.com" />
        </el-form-item>
        <el-divider content-position="left">认证配置（私有仓库）</el-divider>
        <el-form-item label="用户名">
          <el-input v-model="newMirror.username" placeholder="可选，用于私有仓库" clearable />
        </el-form-item>
        <el-form-item label="密码">
          <el-input v-model="newMirror.password" type="password" placeholder="可选，用于私有仓库" show-password clearable />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="addMirrorVisible = false">取消</el-button>
        <el-button type="primary" @click="saveMirror">{{ editMode ? '保存' : '添加' }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup lang="ts">
import { ref, reactive, computed, onMounted } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { useSettingsStore } from '@/stores/settings'
import { systemApi } from '@/api/tauri'
import type { AppSettings, Mirror } from '@/types/api'

const settingsStore = useSettingsStore()

const settings: AppSettings = reactive({
  defaultOutputDir: '',
  defaultArch: 'amd64',
  defaultMirror: 'registry-1.docker.io',
  downloadWorkers: 4,
  lastPackageName: null
})

const mirrors = computed<Mirror[]>(() => settingsStore.mirrors)
const addMirrorVisible = ref<boolean>(false)
const editMode = ref<boolean>(false)
const editingMirrorId = ref<string>('')

interface NewMirrorForm {
  name: string
  registry: string
  username: string
  password: string
}

const newMirror: NewMirrorForm = reactive({
  name: '',
  registry: '',
  username: '',
  password: ''
})

onMounted(async () => {
  try {
    await settingsStore.loadSettings()
    Object.assign(settings, settingsStore.settings)
  } catch (error) {
    console.error('Failed to load settings on mount:', error)
    ElMessage.error(commandErrorMessage(error, '加载配置失败'))
  }
})

const saveSettings = async (): Promise<void> => {
  try {
    await settingsStore.updateSettings(settings)
    Object.assign(settings, settingsStore.settings)
    ElMessage.success('设置已保存')
  } catch (error) {
    ElMessage.error(commandErrorMessage(error, '保存失败'))
  }
}

const selectOutputDir = async (): Promise<void> => {
  try {
    const path = await systemApi.selectDirectory(settings.defaultOutputDir)
    if (path) {
      settings.defaultOutputDir = path
    }
  } catch (error) {
    ElMessage.error(commandErrorMessage(error, '选择目录失败'))
  }
}

const showAddMirror = (): void => {
  editMode.value = false
  editingMirrorId.value = ''
  newMirror.name = ''
  newMirror.registry = ''
  newMirror.username = ''
  newMirror.password = ''
  addMirrorVisible.value = true
}

const editMirror = (mirror: Mirror): void => {
  editMode.value = true
  editingMirrorId.value = mirror.id
  newMirror.name = mirror.name
  newMirror.registry = mirror.registry
  newMirror.username = mirror.username || ''
  newMirror.password = ''
  addMirrorVisible.value = true
}

const saveMirror = async (): Promise<void> => {
  const name = newMirror.name.trim()
  const registry = newMirror.registry.trim()
  if (!name || !registry) {
    ElMessage.warning('请填写名称和地址')
    return
  }

  try {
    if (editMode.value) {
      await settingsStore.updateMirror(editingMirrorId.value, {
        name,
        registry,
        username: newMirror.username || null,
        ...(newMirror.password ? { password: newMirror.password } : {})
      })
      ElMessage.success('镜像源已更新')
    } else {
      await settingsStore.addMirror({
        name,
        registry,
        isDefault: false,
        username: newMirror.username || null,
        password: newMirror.password || null
      })
      ElMessage.success('镜像源已添加')
    }
    addMirrorVisible.value = false
  } catch (error) {
    ElMessage.error(commandErrorMessage(error, editMode.value ? '更新失败' : '添加失败'))
  }
}

const setDefaultMirror = async (id: string): Promise<void> => {
  try {
    await settingsStore.setDefaultMirror(id)
    ElMessage.success('已设为默认镜像源')
  } catch (error) {
    ElMessage.error(commandErrorMessage(error, '设置失败'))
  }
}

const commandErrorMessage = (error: unknown, fallback: string): string => {
  if (typeof error === 'object' && error !== null && 'message' in error) {
    const message = (error as { message?: unknown }).message
    if (typeof message === 'string' && message.trim()) return message
  }
  return error instanceof Error && error.message ? error.message : fallback
}

const deleteMirror = async (id: string): Promise<void> => {
  try {
    await ElMessageBox.confirm('确定要删除此镜像源吗？', '警告', {
      type: 'warning'
    })
    await settingsStore.deleteMirror(id)
    ElMessage.success('已删除')
  } catch (error) {
    if (error !== 'cancel') {
      ElMessage.error(commandErrorMessage(error, '删除失败'))
    }
  }
}
</script>

<style scoped>
.settings-page {
  padding: 24px;
  max-width: 1040px;
  margin: 0 auto;
  box-sizing: border-box;
}
.settings-card :deep(.el-card__body) {
  padding: 0 24px 24px;
}
.settings-card :deep(.el-card__header) {
  padding: 20px 24px;
  font-size: 20px;
  font-weight: 600;
}
.card-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
}
.card-body {
  display: flex;
  flex-direction: column;
}
.section-title {
  font-weight: 600;
  font-size: 14px;
  color: #303133;
  margin-bottom: 12px;
}
.settings-form {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 200px;
  column-gap: 24px;
}
.settings-form :deep(.el-form-item:first-child),
.workers-field,
.save-field {
  grid-column: 1 / -1;
}
.settings-card :deep(.el-select) {
  width: 100%;
}
.settings-card :deep(.el-form-item) {
  margin-bottom: 16px;
}
.settings-card :deep(.el-slider) {
  max-width: 480px;
  padding: 0 8px;
  box-sizing: border-box;
}
.settings-card :deep(.el-divider) {
  margin: 8px 0 16px;
}
@media (max-width: 600px) {
  .settings-page { padding: 16px; }
  .settings-form { grid-template-columns: minmax(0, 1fr); }
}
</style>
